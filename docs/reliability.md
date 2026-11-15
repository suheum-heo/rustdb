# Reliability

This document describes the durability contract the current file engine implements, and the tests that exercise it. Process termination and injected commit errors are not a power loss. A filesystem that reorders writes across `sync_all`, or that loses a synced file, is outside what these tests show.

## Storage invariants

- A database page is 4096 bytes. Heap, catalog, and index-catalog pages are slotted. A cell offset and length are checked before the bytes are read. A free slot is offset 0 and length 0.
- The page checksum is CRC-32 over the page with the checksum field cleared. `verify` rejects a mismatch, an unexpected page id, an unknown page type, and a slot directory that overlaps or runs past the page.
- A stored value is `NULL`, `INTEGER`, `TEXT`, or `BOOLEAN`. A ratio is a query result and is not encoded. Text is a length plus UTF-8. A bad tag, a truncated buffer, or a length that does not land on the end of the record is an error.
- A row must fit in one cell, at most 4068 bytes. The encoder returns that error instead of writing a partial cell.
- An integer B+ tree stores `(key, row id)` pairs in strictly increasing order. Duplicate keys keep every row id. `validate` checks leaf sibling links in both directions. A zero page written over a node fails that check.

## WAL durability and recovery

One statement is one commit. There is no multi-statement transaction.

A statement becomes recoverable when `PATH.wal` contains a trailer with magic `RUSTCMT`, a frame count that matches the body, the checksum of those frames, and the next commit id, and `parse` accepts every logged page. The caller is told the statement succeeded only after three further steps: `sync_all` on that log, the logged pages written into the database and `sync_all` on the database, and truncation of the log. Success means all three happened in this process. It does not mean a later power loss will still see them.

An error is not a promise of rollback.

| Boundary | What this process did | What the caller hears | What the next open does |
| --- | --- | --- | --- |
| Before the log sync | Header and frames are written. There is no trailer, and the log is not synced. | Error, before commit. Memory for that statement is restored while the log is still incomplete. | Discards the log when its nonce matches and its base commit id is the database id or one behind it. The previous rows stay. |
| Commit sync reported as failed | The trailer is in the file. `sync_all` was not called. The outcome is uncertain: the caller cannot tell whether those bytes will still be there after a crash. | Error. Further statements are refused until reopen. This is not a rollback. | If the trailer is still intact and valid, recovery treats the statement as committed and replays it. The injected failure used here skips `sync_all` in this process. It does not cut power. |
| After the log sync, before page writes | `sync_all` on the log returned success. The database file is unchanged. | Error. The process is fenced. The statement is recoverable even though the caller saw a failure. | Replays the logged pages, including the heap and any index pages in that log. A second open sees the same rows. |
| During checkpoint | The log sync succeeded. Some database pages may already have been replaced. | Error. The process is fenced. | Replays the whole log, so a half-written checkpoint is replaced by the logged pages. |
| After the database sync, before the log is cleared | The database sync succeeded. The log is still complete. | Error. The process is fenced. | Writes the logged pages again and truncates the log. |

An empty log is ignored. A trailer whose magic matches but whose length, checksum, or commit id does not is left on disk and open fails. That file does not become a committed transaction. A log shorter than a header plus a trailer is not classified as committed. A second successful open of a recovered database returns the same catalog. None of these injections is a hardware power loss.

## Property-testing strategy

`proptest` is a dev-dependency. The tests live in `src/storage/reliability.rs` and run with `cargo test`. The default is 64 cases so the normal suite stays short. A fixed seed is printed by proptest when a case fails, and `PROPTEST_CASES` raises the count:

```sh
cargo test --locked --lib storage::reliability
PROPTEST_CASES=256 cargo test --locked --lib storage::reliability
```

`proptest` applies `PROPTEST_CASES` again when the test starts, so that variable overrides the 64-case default. `proptest_honors_proptest_cases` checks that.

The generated distributions are narrow on purpose:

- Record values are `NULL`, any `i64`, a boolean, or up to 24 ASCII characters, with at most six cells. That distribution round-trips. It does not generate oversized rows. A separate test rejects a text value of 4068 bytes.
- Malformed record, catalog, and index inputs are 0 to 96 arbitrary bytes. A separate test uses an unknown tag, a short text body, and a `u16::MAX` slot count.
- Page inputs are 0 to 4096 arbitrary bytes, passed to `verify` and then to slot reads. Checksum and slot-directory failures are also fixed cases.
- WAL inputs are 0 to 128 arbitrary bytes. Anything shorter than a header plus a trailer must not classify as committed. A full frame is 4100 bytes, so this distribution does not build a realistic frame. Fixed cases cover a partial frame, a frame count that does not match the body, a count of `u32::MAX`, a bad checksum, and an unknown version.
- Index edits use a leaf capacity of 4 and an internal capacity of 4, keys in `i16`, and row ids from 1 to 39. That is not the on-disk fanout of 254 leaf entries. A fixed sequence on that small tree inserts duplicate keys, forces an internal-node split (height at least 3), deletes every even key, and then rejects a zeroed leaf. The store test that inserts 255 keys is the one that crosses a production leaf split.

The manual workflow `.github/workflows/reliability.yml` sets `PROPTEST_CASES=256` and refuses to pass if `records_round_trip_or_reject_overflow` is not in the test list. It is not required for pull requests, and it has not been run on GitHub.

## Fuzz targets

`src/fuzzing.rs` is the stable entry point. Inputs longer than 64 KiB are cut before decoding, and SQL text is cut at 8 KiB. The normal test `fuzzing::tests::smoke_inputs_do_not_panic` feeds a handful of small buffers, including invalid UTF-8. That is a smoke test of the entry points, not a fuzzing campaign.

`fuzz/` is a separate package so `cargo test` does not need nightly or libFuzzer. From that directory, with nightly and `cargo-fuzz`:

```sh
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run record_decode -- -max_total_time=30
cargo +nightly fuzz run page_decode -- -max_total_time=30
cargo +nightly fuzz run wal_decode -- -max_total_time=30
cargo +nightly fuzz run sql_parse -- -max_total_time=30
```

A failing input is written under `fuzz/artifacts/`. The harness does not open a database file, a socket, or a directory of its own. A short run does not establish that those decoders are free of bugs.

## Crash fault-injection points

`Inject` returns from `Pager::checkpoint` at the steps in the table. It does not kill the process and does not interrupt `write` or `sync_all` in the kernel.

`recovery_is_idempotent_and_a_damaged_log_is_not_applied` checks the message for each injection, so a misspelled path fails the test. After a trailer exists, both opens show the new heap row and the log is empty. `BeforeLogSync` keeps the old row. A trailer with a flipped checksum fails open, leaves the database bytes unchanged, and leaves the log in place.

`a_recovered_heap_insert_keeps_the_matching_index_entry` does the same for a migrated file that already has an integer index. `CommitSyncFailed`, `AfterLogSync`, and checkpoint stops at page 0 and page 1 each reopen twice with the new heap row and both index keys. `BeforeLogSync` keeps the old row and does not add the new key. Older store tests also replay an index leaf split, a merge, and a dropped index after `AfterLogSync` and `DuringCheckpoint(1)`.

## Test reproducibility and seeds

Property failures print the proptest seed. The SQL reference tests in `tests/milestone13.rs` and `tests/milestone14.rs` use the seeds named in those files. Temporary databases use a process id and a counter, so parallel tests do not share a path. The lock tests in `tests/milestone4.rs` use a subprocess and `flock`, not a sleep.

Run the groups separately:

```sh
cargo test --locked
cargo test --locked --lib storage::reliability
cargo test --locked --lib storage::store::tests::recovery_is_idempotent_and_a_damaged_log_is_not_applied
cargo test --release --locked -- --ignored
```

The ignored tests are release profiles. CI does not run them and does not fail a pull request on a timing threshold.

## What was tested, and what was not

### A. Exercised on this machine

- `cargo fmt --check`, locked Clippy, a locked debug build, and `cargo test --locked`.
- The property module at 64 cases, and the same command the manual workflow runs, with `PROPTEST_CASES=256`.
- Each `Inject` variant listed above, including two opens and, for the index case, a heap row plus its index key.
- A damaged trailer left in place.
- The fuzz entry points on a handful of small buffers, including invalid UTF-8. `cargo clippy --all-targets --features fuzz --locked -- -D warnings`.

`actionlint` is not installed here. The workflow files were checked by reading them and by confirming the pinned commits exist: `actions/checkout` `11d5960a326750d5838078e36cf38b85af677262` is tag `v4.4.0`, `dtolnay/rust-toolchain` `683b588c487a69d01f6c6ddf40f036ff682cc2c8` is tag `v1`, and `Swatinem/rust-cache` `2656b87321093db1cb55fbd73183d195214fdfd1` is the commit for tag `v2.5.0`. GitHub Actions has not run these workflows.

### B. Implemented, not shown under power loss

The code calls `File::sync_all` on the log before it modifies the database, and on the database before it truncates the log. Recovery replays a valid trailer and refuses a damaged one. Those calls are what the engine treats as the durability boundary. No test cuts power while `sync_all` is running, and a successful return from `sync_all` is not a measurement of what a disk does after it loses power.

### C. Not guaranteed

- After a commit-sync error, the next open can still show the statement when the trailer is valid. The caller already received an error. That outcome is uncertain.
- An injected error stands in for power loss. The tests return `Err` at a boundary. They do not remove power.
- A crash in the middle of one `write` syscall, or a disk that reports success and then loses the write, is untested.
- Network filesystems, and a process that never calls `flock`, are outside the lock tests.
- A checksum that still matches after a bit flip is not repaired. A mismatch is reported.
- A damaged trailer is not rebuilt. Open fails and the log stays.
- The four fuzz targets have had only the smoke inputs run here. Nightly and `cargo-fuzz` are not installed.
- The property tree uses a leaf capacity of 4. Production leaves hold 254 entries. The property test does not measure that occupancy.

## Suggested next reliability work

A longer libFuzzer run of the four harnesses, with the artifacts checked in only when a crash is real. A subprocess that is killed after the log sync and before the database sync would sit closer to a process crash than an injected `Err`, and it would still not be a power-loss test. Checksums on individual WAL frames, rather than one trailer over every frame, would make a torn last frame distinguishable from a damaged commit record.

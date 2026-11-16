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

The manual workflow `.github/workflows/reliability.yml` sets `PROPTEST_CASES=256` and refuses to pass if `records_round_trip_or_reject_overflow` is not in the test list. It is not required for pull requests. It ran successfully on GitHub after Milestone 15 merged (`actions/runs/43754018755`). Milestone 17 did not start that workflow again.

## Fuzz targets

`src/fuzzing.rs` is the stable entry point. Inputs longer than 64 KiB are cut before decoding, and SQL text is cut at 8 KiB of that prefix. The functions return decoder and parser errors. They do not catch panics, open a database, or write a file.

| Target | Production code | Bound |
| --- | --- | --- |
| `record_decode` | `decode_record`, `decode_catalog`, `decode_index` | 64 KiB |
| `page_decode` | `verify`, then `live_slots` and `cell` only after `verify` succeeds | 4 KiB copied into a fresh page |
| `wal_decode` | `classify_log` | 64 KiB. A frame vector is reserved only after the body length matches the claimed frame count |
| `sql_parse` | `parser::parse` on lossy UTF-8 | 8 KiB |

`fuzz/fuzz_targets/` calls those four functions. `fuzz/` is a separate package so `cargo test` does not need nightly or libFuzzer. A libFuzzer crash would be written under `fuzz/artifacts/`. That directory is gitignored. This machine has `stable-aarch64-apple-darwin` only. `cargo-fuzz` is not on `PATH`. Nightly is not installed, and it was not installed for this campaign.

The replacement is a deterministic mutator in `fuzzing::tests`, not libFuzzer. It does not build a corpus and it does not report LLVM coverage. Fourteen seed inputs include an empty buffer, a record whose value count is `u16::MAX`, a sealed heap page, a one-frame committed log, that log cut to 120 bytes, and short SQL fragments. Each step flips a bit, writes `0xff` into a length field, truncates, deletes, inserts, or repeats a prefix, and never keeps more than 64 KiB. The same LCG seed is the reproducer. A panic fails the test and prints those bytes. There is no per-input timeout, so a hang would not be reported as a libFuzzer timeout.

`fuzzing::tests::mutation_prefix_does_not_panic` runs 4096 iterations of each target in the normal suite. The long run is ignored unless `RUSTDB_MUTATION_SECS` is set, so `cargo test --release --locked -- --ignored` does not start a campaign:

```sh
RUSTDB_MUTATION_SECS=120 cargo test --release --locked --lib mutation_campaign -- --ignored --nocapture
```

On 15 Nov 2026, release profile, `rustc 1.100.0 (0ef1bc386 2026-11-09)`, Apple M5, that command ran each target for 120000 ms. Crashes were 0. Timeouts were 0. Seed inputs were 14. The largest mutated input was 65536 bytes. Executions per second are integer division of `execs * 1000 / elapsed_ms`.

| Target | Seed | Executions | Per second |
| --- | --- | --- | --- |
| `record_decode` | `0x170100000017` | 704654699 | 5872122 |
| `page_decode` | `0x9e376eb87f4a7c02` | 5252021 | 43766 |
| `wal_decode` | `0x3c6ee473fe94f83d` | 80022406 | 666853 |
| `sql_parse` | `0xdaa67a2d7ddf7428` | 8468322 | 70569 |

The seed for target `n` is `0x170100000017 xor (n * 0x9E3779B97F4A7C15)` wrapping at 64 bits. No crash input was saved because none failed. A rejected malformed buffer is a normal `Err`, not a bug.

Before that run, `decode_record` and `decode_catalog` reserved a `u16` count before checking that the buffer could hold that many items. A 22-byte record with count 65535, and a catalog with the same count, now return `database file is corrupt` without that reservation. `rejects_an_oversized_text_value_and_invalid_bytes` covers both. `valid_pages_and_logs_reach_the_success_path` checks that a sealed page passes `verify` and that the committed log classifies as committed while a short copy classifies as incomplete.

## Crash fault-injection points

`Inject` returns from `Pager::checkpoint` at the steps in the table. It does not kill the process and does not interrupt `write` or `sync_all` in the kernel.

`recovery_is_idempotent_and_a_damaged_log_is_not_applied` checks the message for each injection, so a misspelled path fails the test. After a trailer exists, both opens show the new heap row and the log is empty. `BeforeLogSync` keeps the old row. A trailer with a flipped checksum fails open, leaves the database bytes unchanged, and leaves the log in place.

`a_recovered_heap_insert_keeps_the_matching_index_entry` does the same for a migrated file that already has an integer index. `CommitSyncFailed`, `AfterLogSync`, and checkpoint stops at page 0 and page 1 each reopen twice with the new heap row and both index keys. `BeforeLogSync` keeps the old row and does not add the new key. Older store tests also replay an index leaf split, a merge, and a dropped index after `AfterLogSync` and `DuringCheckpoint(1)`.

`recovery_stress_repeats_checkpoints_and_keeps_duplicate_keys` starts from one indexed row with key 7 and then:

- interrupts four publishes at `DuringCheckpoint(0)` and `DuringCheckpoint(1)`, alternating, each adding another row with key 7
- reopens three times after every interrupt and checks heap order plus `read_index_equal` for every distinct key
- updates the first row's text through `AfterLogSync`
- deletes the last duplicate through `CommitSyncFailed`, and requires the error to say durability is unknown and that this is not a confirmed rollback
- publishes one new key after recovery
- keeps a second `Store::open` failed with `is locked` while the recovered store is still open
- truncates a synced log to 120 bytes and checks that the next open discards it, restores the previous rows, and leaves the database bytes unchanged
- flips a trailer checksum and checks that open fails, the database bytes stay unchanged, and the log stays

`sql_recovery_stress_replays_dml_and_holds_the_lock` does the SQL form of that sequence: four interrupted `INSERT`s, an uncertain `UPDATE`, an interrupted `DELETE`, a duplicate indexed key, one more insert after recovery, three opens, and `EXPLAIN` showing `IndexScan`. A second `Database::open` fails while the recovered connection is open, and an insert on that connection is visible after the next open. These injections are not a power loss.

## Test reproducibility and seeds

Property failures print the proptest seed. The SQL reference tests in `tests/milestone13.rs` and `tests/milestone14.rs` use the seeds named in those files. The mutation campaign uses the four seeds in the table above. Temporary databases use a process id and a counter, so parallel tests do not share a path. The lock tests in `tests/milestone4.rs` use a subprocess and `flock`, not a sleep.

Run the groups separately:

```sh
cargo test --locked
cargo test --locked --lib storage::reliability
cargo test --locked --lib storage::store::tests::recovery_is_idempotent_and_a_damaged_log_is_not_applied
cargo test --release --locked -- --ignored
```

The ignored tests are release profiles, plus the mutation campaign when `RUSTDB_MUTATION_SECS` is set. CI does not run them and does not fail a pull request on a timing threshold. Pull requests do not run a fuzz job.

## What was tested, and what was not

### A. Exercised on this machine

- `cargo fmt --check`, locked Clippy, a locked debug build, and `cargo test --locked`.
- The property module at 64 cases, and the same command the manual workflow runs, with `PROPTEST_CASES=256`.
- Each `Inject` variant listed above, including two opens and, for the index case, a heap row plus its index key.
- A damaged trailer left in place.
- The fuzz entry points on the smoke inputs, the 4096-iteration mutation prefix, and the 120-second release campaign recorded above. `cargo clippy --all-targets --features fuzz --locked -- -D warnings`.
- `recovery_stress_repeats_checkpoints_and_keeps_duplicate_keys` and `sql_recovery_stress_replays_dml_and_holds_the_lock`.

`actionlint` is not installed here. The workflow files were checked by reading them and by confirming the pinned commits exist: `actions/checkout` `11d5960a326750d5838078e36cf38b85af677262` is tag `v4.4.0`, `dtolnay/rust-toolchain` `683b588c487a69d01f6c6ddf40f036ff682cc2c8` is tag `v1`, and `Swatinem/rust-cache` `2656b87321093db1cb55fbd73183d195214fdfd1` is the commit for tag `v2.5.0`. GitHub Actions has run the main CI workflow and, after Milestone 15, the manual reliability workflow. Milestone 17's campaign numbers are from this machine, not from a GitHub job.

### B. Implemented, not shown under power loss

The code calls `File::sync_all` on the log before it modifies the database, and on the database before it truncates the log. Recovery replays a valid trailer and refuses a damaged one. Those calls are what the engine treats as the durability boundary. No test cuts power while `sync_all` is running, and a successful return from `sync_all` is not a measurement of what a disk does after it loses power.

### C. Not guaranteed

- After a commit-sync error, the next open can still show the statement when the trailer is valid. The caller already received an error. That outcome is uncertain.
- An injected error stands in for power loss. The tests return `Err` at a boundary. They do not remove power.
- A crash in the middle of one `write` syscall, or a disk that reports success and then loses the write, is untested.
- Network filesystems, and a process that never calls `flock`, are outside the lock tests.
- A checksum that still matches after a bit flip is not repaired. A mismatch is reported.
- A damaged trailer is not rebuilt. Open fails and the log stays.
- The mutation campaign is not libFuzzer. It has no coverage report and no per-input timeout. Nightly and `cargo-fuzz` are not installed, so the `fuzz/` binaries were not run.
- The property tree uses a leaf capacity of 4. Production leaves hold 254 entries. The property test does not measure that occupancy.

## INNER JOIN runtime type errors

`INSERT` rejects a value whose runtime type is not the column's declared type. A test can still place one there by replacing a stored cell. `NULL` is allowed in every column. A ratio is a query result and is not a stored cell.

The nested loop evaluates `ON` for every pair. `AND` and `OR` evaluate both sides, and a comparison of two non-NULL values of different types returns `comparison values have different types`. `NULL` compared with any value is unknown, not a type error. `WHERE` runs only after `ON` is TRUE, so a malformed cell that appears only in `WHERE` is not read when `ON` is not TRUE.

`HashJoin` and `IndexNestedLoopJoin` can prune a pair before that `ON` runs. A NULL probe, a hash miss, and a cross-type key were treated as "no match". `same_cell` returns false for two different runtime types and does not report the comparison error. An index probe of a NULL key returns immediately, and a key with no index entry never sees a residual cell on some other row. Milestone 19 fixed the same hole for `HashLeftJoin`. Milestone 21 applies it to both optimized `INNER JOIN` paths.

Before hashing or probing, execution walks each input once to check row width, then once per `ON` column. The only allocation is that column-index list plus the declared-type vectors. It does not copy rows, build a pair buffer, or write the database or the log. On the 15 Nov 2026 release profile, that walk added 30 µs to a 1,201 µs warm 10,000 by 10,000 hash join. Several smaller shapes were faster with the walk enabled. Those differences are timer noise, not a second join algorithm. When every `ON` cell is NULL or its declared type, the planned hash or index path runs unchanged, including either hash build side. When one cell disagrees, that execution uses the nested loop. An empty input has no bad cell, so it stays on the planned path. An empty side also means the nested loop compares nothing, so a bad cell on the other side is not an error until some pair is formed.

`EXPLAIN` chooses the plan from the schema and the row counts. It does not read cells, so it can still say `HashJoin` or `IndexNestedLoopJoin` for a statement that falls back at execution. It does not modify the database.

A schema-valid index whose leaf is unsorted, whose row id is duplicated, or whose page type is wrong still returns the existing corrupt-index error. The fallback does not run after that error and does not turn it into a successful nested loop. A malformed `ON` cell is a recoverable query error and is detected before the index is read, including when the index is also damaged. A later statement sees the same database. `SELECT` and `EXPLAIN` do not publish a partial row and do not append a log frame.

The check does not look at `WHERE` for joins. Both join paths already agree there. It does not change the 64× index threshold, the hash build side, or the index page format.

## Indexed UPDATE and DELETE

`UPDATE` and `DELETE` look up one integer equality before they change a row. The lookup returns row ids. Those ids are resolved and checked against the heap. The statement then walks every heap row and collects the ones whose indexed column is that integer. `NULL` is not a member. The index list and the heap list must be the same positions, in heap order. That comparison happens before the original `WHERE`, so a missing posting is visible even when the omitted row would fail a residual predicate. Only a matching set is filtered, listed, and published. The index is not edited while the lookup cursor is open. Changing an indexed key cannot make the same row match twice, and deleting an earlier row cannot shift a later target, because the targets are row ids rather than slots captured after a mutation.

The checks run in this order. Binding and assignment types fail first. A wrong-typed `WHERE` cell then uses the heap scan, so the comparison reports `comparison values have different types` when the scan evaluates it. That choice is made before the probe, including when a posting is also missing. A corrupt page, a broken leaf link, a missing row id, a repeated row id, or an index key that disagrees with a returned heap integer fails during the probe. A set that disagrees with the heap equality walk fails next, with the same index-mismatch error. `WHERE` runs only after that. Publish has not started, so memory and the file stay as they were, and no affected-row count is reported. Publish also rejects a table image whose stored cell does not match the column.

Open compares the whole tree with the heap, so a file that is already missing a posting fails to open. The mutation walk covers a posting removed after open for the chosen key. It does not audit other keys, and `SELECT` and join probes still do not walk the heap for an absent posting. An `UPDATE` installs only the matched row images, after every assignment and record-size check has succeeded. Publish type-checks every in-memory row and then rewrites the listed row ids. It does not decode the other stored rows. A value that already matches its slot does not dirty a page. A larger record uses the existing relocate path. Index pages are included only when an integer key changes, and they share the commit with the heap pages. A `DELETE` that removes rows unlinks the listed row ids and does not decode the survivors. A delete that matches nothing still decodes the stored rows.

If the in-memory row ids or the row-id counter disagree with the stored snapshot, publish uses the previous full-image path. A normal `UPDATE` does not change those ids. A test can force that full path. Growth does not use it: relocation stays inside the selective commit. A selective `UPDATE` saves the previous image of each matched row before it replaces any slot. Restoring those images puts the in-memory rows back. Row ids, positions, the row-id counter, and the index catalog are not part of that journal because this statement does not change them. Heap relocation and index maintenance run on editor copies and are discarded when publish fails before the commit, so they are not journaled either. `INSERT` and DDL still snapshot the structures they change. A test can force the old table snapshot for `UPDATE` or `DELETE`. A failure while assigning a later row, while encoding a record, or while reserving the journal returns before any slot is replaced. A failure while the editor prepares an index change or relocates a record returns before the log is written, and the journal restores every matched row. If that restore cannot find an original image, the process is fenced and the error says the outcome is not a confirmed rollback. Injected failures after the log sync still fence the process and do not apply the journal. A multi-row update is one commit, so recovery shows every matched row or, when the log never synced, none of them. An out-of-memory abort while cloning a saved image is not a query error; reserving the journal's entry list is, and it happens before the first assignment.

A `DELETE` compacts the in-memory vector, so the update journal is not enough. Before any survivor moves, the statement saves the row id, the original image, and the original position of each removed row. It does not clone the survivors for that journal. Compaction moves survivors into new vectors and reserves a row-id map for the original row count before it installs them. A reservation failure leaves the table unchanged. Restore checks every saved id and position, builds a placement for every original slot, and reserves the restored vectors before it moves a live row. Those vectors replace the table only after every slot is filled. A reservation or placement failure leaves the compacted rows in place; the caller fences and does not report a confirmed rollback. Row ids are not reused and `next_row_id` is not advanced. A delete that removes nothing keeps the existing vector. A delete that removes every row journals every row; that case is still linear in the table. The index catalog is not journaled because `DELETE` does not change it. Index postings are updated only inside the editor, together with the heap unlinks, and they are discarded when publish fails before the log sync. Restoring the logical table therefore restores the indexes on the next attempt. If restore cannot put every removed row back, or a test forces that failure, the process is fenced and the error says the outcome is not a confirmed rollback. The journal is not applied after the log sync. A multi-row delete is one commit, so recovery shows every removed row gone or, when the log never synced, all of them still present. Publish copies those removed row ids and asks the editor to unlink them. It does not clone the surviving values into a table image, and it does not take the journal: the journal still holds the images and the original positions for rollback. The copy of the id list can abort the process if allocation fails. That is not a query error, and it happens after compaction and before the editor runs, while the file is still unchanged. A list that does not describe the compacted table is not unlinked; publish then uses the previous full image of the in-memory tables. A delete that matches nothing still audits every in-memory row, including the record-size check, and decodes every stored row of every table whose ids still match. A stored row that already matches does not dirty a page, so a genuine no-op writes no log and does not change the row-id counter. A real delete does not decode its survivors. Unlink starts at the highest stored index, so earlier indexes stay valid, and `next_row_id` is not changed. A duplicate id, a zero id, an id past the counter, or an id that is not stored fails before any row is removed. A test can force the old table image. The editor still copies its page snapshot, including the stored row-id vector, and it still reads each removed record in order to drop that row's index postings.

Injected commit failures keep the previous boundaries. A failure before the log sync restores memory. A failed commit sync, a failure after the log sync, and a failure during checkpoint fence the process; the next open can still show the statement. None of those injections is a power loss. The exclusive file lock is unchanged.

## Suggested next reliability work

A libFuzzer campaign, once nightly and `cargo-fuzz` are available, with an artifact kept only when a crash is real. A subprocess that is killed after the log sync and before the database sync would sit closer to a process crash than an injected `Err`, and it would still not be a power-loss test. Checksums on individual WAL frames, rather than one trailer over every frame, would make a torn last frame distinguishable from a damaged commit record.

# Storage benchmark

Two release runs are recorded here. They use the same example and the same row shape. They are not a paired control: the Milestone 3 numbers are from an earlier session that rewrote the whole file, and the Milestone 4 numbers are from a later session that checkpoints dirty pages. Do not subtract one table from the other and call the difference a speedup measured in one sitting.

## Reproduce

```bash
cargo run --release --example storage_bench
```

The example opens a fresh file under the process temporary directory for each size. It prints the hardware lines and the measurements below.

## Setup

Both runs used this machine and this harness.

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM, `arm64` |
| OS | macOS (`darwin` 25.2.0) |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo run --release --example storage_bench` |
| Schema | `bench (id INT, email TEXT, note TEXT, active BOOLEAN)` |
| Row shape | `user#####@example.com`, a 64-byte `note`, and a boolean |
| Location | Process temporary directory |

The timer for the load starts after `CREATE TABLE`. Each insert, update, and scan is one SQL statement. Scans read the in-memory table and do not write the file.

One discarded database of 20 rows runs first. Each measured size then uses a new file:

1. Insert `N` rows, one statement at a time, and record the total. The mean is that total divided by `N`.
2. Record the file length after those `N` rows.
3. Run `SELECT id, email, note, active FROM bench` five times.
4. Run `UPDATE bench SET active = ... WHERE id = 1` five times, flipping the boolean each time. Each update matches one row.
5. Insert five more rows. Those samples see `N`, then `N + 1`, through `N + 4` rows. The file length above is from before these five inserts.

For five samples, the median is the middle value after sorting.

The Milestone 3 publisher synced a temporary file, synced the parent directory, renamed the file, and synced the parent directory again. The Milestone 4 publisher syncs the redo log, syncs the database, and syncs the truncated log. Both are wall times for a durable statement, not CPU time.

## Milestone 3, historical full-file run

Measured on 13 November 2026. This is the run that was current when Milestone 3 shipped. It is kept so the later incremental run has something to sit next to. It is not a rerun from the Milestone 4 session.

| Rows | File size | Load | Mean insert during load | Scan min / median / max | Update min / median / max | Later insert min / median / max |
| --- | ---: | ---: | ---: | --- | --- | --- |
| 100 | 45,056 bytes (11 pages) | 1,051 ms | 10.5 ms | 0.036 / 0.041 / 0.130 ms | 9.1 / 9.8 / 12.9 ms | 8.8 / 12.0 / 13.1 ms |
| 1,000 | 290,816 bytes (71 pages) | 11,771 ms | 11.8 ms | 0.045 / 0.046 / 0.106 ms | 11.7 / 13.2 / 14.0 ms | 12.8 / 13.1 / 14.0 ms |
| 10,000 | 2,748,416 bytes (671 pages) | 264,912 ms | 26.5 ms | 0.351 / 0.372 / 0.465 ms | 44.8 / 44.9 / 45.2 ms | 43.7 / 43.8 / 44.2 ms |

Page size is 4,096 bytes. The microsecond samples from that run are:

| Rows | Scan (µs) | Update (µs) | Later insert (µs) |
| --- | --- | --- | --- |
| 100 | 130, 52, 39, 36, 41 | 9,615, 9,957, 9,821, 9,139, 12,869 | 9,140, 8,755, 13,125, 13,020, 11,963 |
| 1,000 | 106, 48, 45, 46, 46 | 11,683, 12,877, 13,985, 13,987, 13,184 | 12,835, 14,011, 13,121, 12,799, 13,069 |
| 10,000 | 465, 400, 364, 372, 351 | 44,832, 44,821, 45,118, 45,161, 44,886 | 44,235, 43,731, 43,817, 44,234, 43,764 |

## Milestone 4, incremental checkpoint

Measured on 13 November 2026, after the redo-log checkpoint replaced ordinary full-file publishes. Same harness, same machine model, a different process and a later clock time. Each mutating statement synced the log, synced the database, and synced the truncated log.

| Rows | File size | Load | Mean insert during load | Scan min / median / max | Update min / median / max | Later insert min / median / max |
| --- | ---: | ---: | ---: | --- | --- | --- |
| 100 | 24,576 bytes (6 pages) | 1,245 ms | 12.5 ms | 0.059 / 0.065 / 0.148 ms | 11.0 / 12.0 / 13.4 ms | 12.2 / 14.0 / 15.0 ms |
| 1,000 | 147,456 bytes (36 pages) | 12,683 ms | 12.7 ms | 0.116 / 0.117 / 0.207 ms | 13.0 / 14.9 / 15.0 ms | 11.9 / 12.0 / 12.0 ms |
| 10,000 | 1,376,256 bytes (336 pages) | 140,412 ms | 14.0 ms | 0.495 / 0.577 / 0.626 ms | 23.3 / 25.8 / 27.7 ms | 13.9 / 14.8 / 15.2 ms |

The microsecond samples from this run are:

| Rows | Scan (µs) | Update (µs) | Later insert (µs) |
| --- | --- | --- | --- |
| 100 | 148, 80, 62, 65, 59 | 13,401, 11,973, 11,018, 12,091, 11,743 | 12,214, 13,930, 14,038, 14,023, 14,979 |
| 1,000 | 207, 120, 116, 117, 116 | 14,862, 14,950, 14,947, 13,985, 13,023 | 11,993, 11,863, 11,983, 12,034, 12,027 |
| 10,000 | 602, 626, 495, 577, 567 | 27,744, 27,136, 25,818, 24,934, 23,312 | 13,907, 14,776, 15,154, 13,987, 14,899 |

At 100 and 1,000 rows the mutation times sit next to the older run. The fixed cost is the three syncs, and the files are small enough that rewriting them was not yet the dominant part of that older run. At 10,000 rows the incremental file is 336 pages instead of 671, the load finished in 140 s instead of 265 s, and a single-row insert after the load had a median of 14.8 ms instead of 43.8 ms. The median update was 25.8 ms instead of 44.9 ms. The scan median was 0.577 ms. Scans still read memory. The first sample in each scan series is the slow one, same as before.

These two runs do not isolate the checkpoint from the rest of the machine. Background activity was not controlled, and the older run was not repeated in this session.

## One-row update profile

This is a separate release run on the same machine, on 13 November 2026. It is not part of `storage_bench`. The command is:

```bash
cargo test --release --lib profile_single_row_update_on_ten_thousand_rows -- --ignored --nocapture
```

The test is ignored, so `cargo test` does not run it. It builds 10,000 rows with one publish, then runs one SQL `UPDATE` that changes a single boolean. The row text is 64 bytes. Every sample compared all 10,000 rows and wrote 2 frames: page 0 and the heap page that holds the row.

An in-memory database, with no file, ran the same update five times. Those times are parse, the `WHERE` scan of the in-memory vector, and the in-memory row replace:

`538, 544, 546, 549, 716` µs. Median `546` µs.

The file update, five samples sorted by total wall time, in microseconds:

| Total | Row compare | Editor apply | WAL write | WAL sync | DB write | DB sync | WAL clear |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 13,625 | 862 | 1,437 | 44 | 3,822 | 7 | 2,968 | 3,202 |
| 20,746 | 7,703 | 8,278 | 47 | 3,708 | 6 | 4,085 | 2,751 |
| 21,003 | 7,715 | 8,283 | 39 | 3,760 | 7 | 4,000 | 3,015 |
| 21,011 | 7,657 | 8,233 | 36 | 4,082 | 9 | 3,999 | 2,781 |
| 21,102 | 7,761 | 8,344 | 65 | 3,684 | 11 | 3,937 | 3,029 |

Editor apply includes the row compare. On the median sample the rest of the editor is `8,283 − 7,715 = 568` µs. That is page serialization and the in-place cell write. The WAL write is 39 µs and the database page write is 7 µs. The three syncs are `3,760 + 4,000 + 3,015 = 10,775` µs, about half of the 21.0 ms statement. The remainder, about 1.9 ms, is SQL parsing, the in-memory scan, and the handoff into the editor.

The fast sample spent 0.86 ms comparing rows. The other four spent 7.7–7.8 ms. A checkpoint clears the pager cache, so an update after that cache is gone re-reads every heap page while it compares every row. The open that precedes the first update leaves the cache warm, which is why one sample is much faster. The compare is O(N) in the number of rows even when one row changes. On a cold cache that work is real, and it is still smaller than the three syncs together. This run did not change the update algorithm.

## Limitations

- One machine and one load per size in each session. The five scan, update, and insert samples are repeats on that same file, so they share cache and allocator state.
- The five later inserts grow the table by five rows. They are not five rebuilds of an identical `N`-row file.
- The load mean mixes the early inserts with the later ones.
- Times include `fsync`. They are wall times for a durable statement, not CPU time. They are not a power-loss measurement.
- The temporary directory is on the local system volume. The disk model was not identified separately from the Apple M5.
- The Milestone 3 table and the Milestone 4 table were collected in different sessions. Comparing them describes the two runs. It is not a controlled before-and-after on one idle machine.
- The example checks that each statement touches the expected number of rows, so a wrong result fails the run instead of reporting a time.

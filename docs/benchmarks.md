# Storage benchmark

This is a release baseline for publishing the whole database file after each statement. It is one run on one machine. The repeated samples show the spread of that run. They are not a confidence interval.

## Reproduce

```bash
cargo run --release --example storage_bench
```

The example opens a fresh file under the process temporary directory for each size. It prints the hardware lines and the measurements below. A 10,000-row run rewrites the file on every insert, so the load alone can take several minutes.

## Setup

Measured on 13 November 2026.

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM, `arm64` |
| OS | macOS (`darwin` 25.2.0) |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo run --release --example storage_bench` |
| Schema | `bench (id INT, email TEXT, note TEXT, active BOOLEAN)` |
| Row shape | `user#####@example.com`, a 64-byte `note`, and a boolean |
| Location | Process temporary directory |

The timer for the load starts after `CREATE TABLE`. Each insert, update, and scan is one SQL statement. Publishing syncs the temporary file, syncs the parent directory, renames the file, and syncs the parent directory again, so these times include that durability work.

One discarded database of 20 rows runs first. Each measured size then uses a new file:

1. Insert `N` rows, one statement at a time, and record the total. The mean is that total divided by `N`. This load is a single build.
2. Record the file length after those `N` rows.
3. Run `SELECT id, email, note, active FROM bench` five times.
4. Run `UPDATE bench SET active = ... WHERE id = 1` five times, flipping the boolean each time. Each update matches one row.
5. Insert five more rows. Those samples see `N`, then `N + 1`, through `N + 4` rows. The file length above is from before these five inserts.

For five samples, the median is the middle value after sorting. The first scan in each series is slower than the later ones; the median is the figure used in the comparison.

## Results

| Rows | File size | Load | Mean insert during load | Scan min / median / max | Update min / median / max | Later insert min / median / max |
| --- | ---: | ---: | ---: | --- | --- | --- |
| 100 | 45,056 bytes (11 pages) | 1,051 ms | 10.5 ms | 0.036 / 0.041 / 0.130 ms | 9.1 / 9.8 / 12.9 ms | 8.8 / 12.0 / 13.1 ms |
| 1,000 | 290,816 bytes (71 pages) | 11,771 ms | 11.8 ms | 0.045 / 0.046 / 0.106 ms | 11.7 / 13.2 / 14.0 ms | 12.8 / 13.1 / 14.0 ms |
| 10,000 | 2,748,416 bytes (671 pages) | 264,912 ms | 26.5 ms | 0.351 / 0.372 / 0.465 ms | 44.8 / 44.9 / 45.2 ms | 43.7 / 43.8 / 44.2 ms |

Page size is 4,096 bytes. The microsecond samples from the run are:

| Rows | Scan (µs) | Update (µs) | Later insert (µs) |
| --- | --- | --- | --- |
| 100 | 130, 52, 39, 36, 41 | 9,615, 9,957, 9,821, 9,139, 12,869 | 9,140, 8,755, 13,125, 13,020, 11,963 |
| 1,000 | 106, 48, 45, 46, 46 | 11,683, 12,877, 13,985, 13,987, 13,184 | 12,835, 14,011, 13,121, 12,799, 13,069 |
| 10,000 | 465, 400, 364, 372, 351 | 44,832, 44,821, 45,118, 45,161, 44,886 | 44,235, 43,731, 43,817, 44,234, 43,764 |

## Full-file rewriting

Rewriting the whole file is a measurable bottleneck, and at a few hundred rows it is still hidden by a fixed publish cost.

A full scan stays under half a millisecond through 10,000 rows (median 0.041 ms, 0.046 ms, and 0.372 ms). A single-row `INSERT` or `UPDATE` at 100 and 1,000 rows costs about 10–13 ms. Those two sizes are far apart in bytes (44 KiB and 284 KiB) and close in mutation time, which matches a fixed cost from creating, syncing, and renaming the file.

At 10,000 rows the file is 2.6 MiB. The median single-row insert is 43.8 ms and the median update is 44.9 ms, about 3.3 times the 1,000-row mutation and about 120 times the scan of the same table. Loading 100 rows took 1.05 s. Loading 1,000 took 11.8 s, close to 10 times longer. Loading 10,000 took 265 s, about 22 times the 1,000-row load, because each statement rewrites every row already stored. The mean insert during that load is 26.5 ms, while an insert once all 10,000 rows exist is 43.8 ms.

## Limitations

- One machine and one load per size. The five scan, update, and insert samples are repeats on that same file, so they share cache and allocator state.
- The five later inserts grow the table by five rows. They are not five rebuilds of an identical `N`-row file.
- The load mean mixes cheap early inserts with expensive later ones.
- Times include `fsync` of the file and the parent directory. They are wall times for a durable statement, not CPU time.
- The temporary directory is on the local system volume. The disk model was not identified separately from the Apple M5.
- No other copy of this benchmark ran in the same process. Background activity on the machine was not controlled.
- The example checks that each statement touches the expected number of rows, so a wrong result fails the run instead of reporting a time.

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

## Milestone 5 index measurements

These numbers are from one release run on the same machine. They are not a claim that `SELECT` got faster. In that run, `SELECT` still scanned the in-memory table. The indexed file was migrated to version 3 before `CREATE INDEX idx_bench_age ON bench(age)`. The plain file stayed version 2 and has no index. Each disk time is one sample and includes `fsync`.

Reproduce the standalone tree with:

```bash
cargo test --release --lib storage::btree::tests::profile_standalone_tree -- --ignored --nocapture
```

The tree is an in-memory page store using the production fanout (254 leaf entries, 203 internal separators). Equality is the mean of one lookup per key. The range is the inclusive prefix from `0` through `count / 10`.

| Keys | Height | Build | Equality | Range |
| --- | --- | --- | --- | --- |
| 100 | 1 | 181 µs | 778 ns | 1 µs, 11 rows |
| 1,000 | 2 | 1,476 µs | 1,025 ns | 3 µs, 101 rows |
| 10,000 | 2 | 15,170 µs | 842 ns | 10 µs, 1,001 rows |
| 100,000 | 3 | 154,714 µs | 729 ns | 46 µs, 10,001 rows |

Reproduce the file measurements with:

```bash
cargo test --release --lib database::tests::profile_index_maintenance -- --ignored --nocapture
```

The schema is `bench (id INT, age INT, email TEXT, note TEXT, active BOOLEAN)`. `N` rows are inserted before the timed statements. Build is `CREATE INDEX` on `age` for the migrated file, and an empty step for the plain file. The timed insert, update, and delete each change one row. The update sets `age` to 42, which is not the value that row already held. `select_rows` was `N` after the extra insert and the delete, which is the full remaining table.

| N | Index build | Insert, plain | Insert, indexed | Update, plain | Update, indexed | Delete, plain | Delete, indexed | Bytes, plain | Bytes, indexed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | 12.2 ms | 12,349 µs | 12,960 µs | 11,785 µs | 12,005 µs | 11,253 µs | 12,983 µs | 16,384 | 24,576 |
| 1,000 | 17.3 ms | 11,938 µs | 12,303 µs | 13,244 µs | 16,128 µs | 12,029 µs | 12,873 µs | 90,112 | 122,880 |

The extra bytes are 8,192 at 100 rows and 32,768 at 1,000 rows. The statement times are dominated by the three checkpoint syncs. They do not show a query improvement, because nothing in that run asked `SELECT` to walk the tree.

## Milestone 6 SQL SELECT

These times are end-to-end `SELECT` latency. They are not standalone B+ tree lookups. Each sample is `Database::execute` of one statement, then a sum of the returned `id` values. `std::hint::black_box` keeps that sum and the row count. The indexed and sequential runs had to return the same ids, and the row count had to match the predicate. A mismatch fails the run.

Two release runs used this harness. The first used an index for every integer range. The second is the current planner: a range that covers every stored key becomes a sequential scan, and `EXPLAIN` reports that path after the timed samples.

Reproduce it with:

```bash
cargo test --release --lib database::tests::profile_index_select -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| OS | macOS (`darwin` 25.2.0) |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --lib database::tests::profile_index_select -- --ignored --nocapture` |
| Schema | `bench (id INT, age INT, note TEXT)` |
| Rows | `id` from 1 through N, `age = id % 100`, `note = 'n'` |
| Load | `INSERT` batches of 500 rows. Not included in the times |
| Indexed file | Migrated to version 3, then `CREATE INDEX idx_bench_age ON bench(age)` |
| Sequential file | Version 2, no index |
| Cold sample | One `SELECT` immediately after `clear_page_cache` |
| Warm sample | Median of the next 9 `SELECT`s on that same open database |

Opening the file already copies the heap into the row vector and, for the indexed file, walks the tree while checking it. The cache clear drops those pages. A sequential `SELECT` still does not read the file: the rows are in memory. An indexed `SELECT` reads B+ tree pages. The cold sample pays for that read and for the page checksum. The warm samples reuse the cache. The heap stays in memory either way.

Speedup is the sequential time divided by the indexed time. A number above 1 means the index plan was faster. `eq` is `WHERE age = 7` (1% of the rows). `range10` is `WHERE age >= 0 AND age <= 9` (10%). `range50` is `WHERE age >= 50` (50%). `all` is `WHERE age >= 0` (every row, because `age` is 0 through 99). In this data the stored ages are 0 through 99, so `all` covers every index key. `range50` does not: keys below 50 remain.

### Before the stored-key guard

Every range used `IndexScan`.

| N | Query | Rows | Selectivity | Indexed cold | Indexed warm | Seq cold | Seq warm | Cold speedup | Warm speedup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1,000 | eq | 10 | 0.0100 | 346 µs | 10 µs | 17 µs | 16 µs | 0.049 | 1.600 |
| 1,000 | range10 | 100 | 0.1000 | 114 µs | 18 µs | 37 µs | 38 µs | 0.325 | 2.111 |
| 1,000 | range50 | 500 | 0.5000 | 216 µs | 36 µs | 26 µs | 25 µs | 0.120 | 0.694 |
| 1,000 | all | 1,000 | 1.0000 | 380 µs | 88 µs | 40 µs | 28 µs | 0.105 | 0.318 |
| 10,000 | eq | 100 | 0.0100 | 311 µs | 12 µs | 90 µs | 84 µs | 0.289 | 7.000 |
| 10,000 | range10 | 1,000 | 0.1000 | 379 µs | 75 µs | 227 µs | 220 µs | 0.599 | 2.933 |
| 10,000 | range50 | 5,000 | 0.5000 | 1,312 µs | 294 µs | 180 µs | 142 µs | 0.137 | 0.483 |
| 10,000 | all | 10,000 | 1.0000 | 2,525 µs | 564 µs | 206 µs | 194 µs | 0.082 | 0.344 |
| 100,000 | eq | 1,000 | 0.0100 | 590 µs | 56 µs | 627 µs | 601 µs | 1.063 | 10.732 |
| 100,000 | range10 | 10,000 | 0.1000 | 2,782 µs | 632 µs | 1,777 µs | 1,730 µs | 0.639 | 2.737 |
| 100,000 | range50 | 50,000 | 0.5000 | 12,350 µs | 3,185 µs | 1,232 µs | 1,218 µs | 0.100 | 0.382 |
| 100,000 | all | 100,000 | 1.0000 | 24,517 µs | 7,003 µs | 1,815 µs | 1,729 µs | 0.074 | 0.247 |

On a warm cache, equality was faster with the index: 1.6x at 1,000 rows, 7.0x at 10,000, and 10.7x at 100,000. The 10% range was also faster when warm (2.1x to 2.9x). The 50% and 100% ranges were slower. Those plans read a large part of the tree, checked every candidate against the heap, and sorted the positions back into insertion order.

The checksums of the returned ids matched across the two files. For equality they were 4,570, 495,700, and 49,957,000. For every row they were 500,500, 50,005,000, and 5,000,050,000.

### After the stored-key guard

`all` is `SeqScan` because every stored age is at least 0. `eq`, `range10`, and `range50` stay `IndexScan`. The cold `all` sample still reads the index's minimum and maximum key before the memory scan, which is why its cold time is higher than the file that has no index. The warm times then sit next to the sequential scan. At 100,000 rows the warm `all` time fell from 7,003 µs to 1,783 µs. The 50% range is unchanged on purpose: keys below 50 exist, so the planner cannot prove that the range returns most of the table without assuming how the keys are distributed.

| N | Query | Path | Rows | Selectivity | Indexed cold | Indexed warm | Seq cold | Seq warm | Cold speedup | Warm speedup |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1,000 | eq | IndexScan | 10 | 0.0100 | 216 µs | 9 µs | 19 µs | 18 µs | 0.088 | 2.000 |
| 1,000 | range10 | IndexScan | 100 | 0.1000 | 176 µs | 23 µs | 42 µs | 41 µs | 0.239 | 1.783 |
| 1,000 | range50 | IndexScan | 500 | 0.5000 | 340 µs | 51 µs | 22 µs | 20 µs | 0.065 | 0.392 |
| 1,000 | all | SeqScan | 1,000 | 1.0000 | 160 µs | 36 µs | 34 µs | 33 µs | 0.212 | 0.917 |
| 10,000 | eq | IndexScan | 100 | 0.0100 | 246 µs | 13 µs | 98 µs | 90 µs | 0.398 | 6.923 |
| 10,000 | range10 | IndexScan | 1,000 | 0.1000 | 422 µs | 81 µs | 247 µs | 246 µs | 0.585 | 3.037 |
| 10,000 | range50 | IndexScan | 5,000 | 0.5000 | 1,475 µs | 300 µs | 162 µs | 150 µs | 0.110 | 0.500 |
| 10,000 | all | SeqScan | 10,000 | 1.0000 | 369 µs | 205 µs | 349 µs | 196 µs | 0.946 | 0.956 |
| 100,000 | eq | IndexScan | 1,000 | 0.0100 | 615 µs | 49 µs | 771 µs | 625 µs | 1.254 | 12.755 |
| 100,000 | range10 | IndexScan | 10,000 | 0.1000 | 3,034 µs | 617 µs | 1,791 µs | 1,783 µs | 0.590 | 2.890 |
| 100,000 | range50 | IndexScan | 50,000 | 0.5000 | 12,721 µs | 3,472 µs | 1,329 µs | 1,205 µs | 0.104 | 0.347 |
| 100,000 | all | SeqScan | 100,000 | 1.0000 | 2,085 µs | 1,783 µs | 1,804 µs | 1,736 µs | 0.865 | 0.974 |

The same checksums matched again. Warm equality is 2.0x, 6.9x, and 12.8x. The warm 10% range is 1.8x to 3.0x. The warm 50% range remains slower, at 0.35x for 100,000 rows.

## Limitations

- One machine and one load per size in each session. The five scan, update, and insert samples are repeats on that same file, so they share cache and allocator state.
- The five later inserts grow the table by five rows. They are not five rebuilds of an identical `N`-row file.
- The load mean mixes the early inserts with the later ones.
- Times include `fsync`. They are wall times for a durable statement, not CPU time. They are not a power-loss measurement.
- The temporary directory is on the local system volume. The disk model was not identified separately from the Apple M5.
- The Milestone 3 table and the Milestone 4 table were collected in different sessions. Comparing them describes the two runs. It is not a controlled before-and-after on one idle machine.
- The example checks that each statement touches the expected number of rows, so a wrong result fails the run instead of reporting a time.
- The Milestone 5 disk times are one sample each. The difference between a plain statement and an indexed statement is smaller than the spread of `fsync`, so it is not a stable overhead measurement.
- The standalone tree times use an in-memory page map. They do not include the pager, the redo log, or `fsync`.
- The Milestone 6 tables are one release run each. The warm time is the median of nine repeats on one open file. The cold time is one sample after clearing the pager cache. A sequential `SELECT` does not read heap pages, because open has already loaded the heap into memory. The later `all` plan still reads the index endpoints on the cold sample before scanning memory.

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

## Milestone 7 nested-loop join

Measured on 14 November 2026. The database is in memory. Each input is `lhs (id INT, note INT)` and `rhs (id INT, note INT)`. Identity rows use `id = note = 0..N-1` on both sides, so an equality join examines `N × N` pairs and returns `N` rows. The duplicate shape uses the same left ids and sets each right id to `id % 50`, which still returns 1,000 rows: keys `0..49` match 20 times each, and keys `50..999` match nothing. The timed statement is `SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id`. Load time is not included. The checksum is the sum of the returned left ids, and the run checks it. `std::hint::black_box` consumes that sum and the row count.

```bash
cargo test --release --test milestone7 profile_nested_loop_join -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --offline --test milestone7 profile_nested_loop_join -- --ignored --nocapture` |
| Algorithm | Nested loop. Every pair is tested. No index is read. |

`Value` and `Vec<Value>` were each 24 bytes. A returned row is two integers, so the result byte count is `matches × 72`. The copy byte count is one input: `N × 72` after the review, and both inputs (`(N + M) × 72`) before it. These counts are the row vectors, not process RSS. A failed pair is not stored. With no `ORDER BY`, a match is projected immediately, so this statement does not keep a wider joined row. `ORDER BY` still does.

The same harness was run immediately before and after that copy change. The tables have equal size, so the change copies one input instead of two. The warm time is the median of five repeats. The cold time is the first sample.

| Shape | N | Pairs | Matches | Before cold | Before warm | After cold | After warm | Copied bytes before | Copied bytes after |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| identity | 100 | 10,000 | 100 | 596 µs | 69 µs | 315 µs | 98 µs | 14,400 | 7,200 |
| identity | 1,000 | 1,000,000 | 1,000 | 5,626 µs | 5,774 µs | 8,248 µs | 6,673 µs | 144,000 | 72,000 |
| identity | 4,000 | 16,000,000 | 4,000 | 90,260 µs | 90,669 µs | 94,148 µs | 93,058 µs | 576,000 | 288,000 |
| duplicate | 1,000 | 1,000,000 | 1,000 | 5,734 µs | 5,735 µs | 5,980 µs | 5,957 µs | 144,000 | 72,000 |

The 4,000-row identity still copies 288,000 bytes after the change because only one of the two equal inputs is copied; before the change that case copied 576,000 bytes. Result bytes stayed 7,200, 72,000, and 288,000. From 1,000 to 4,000 rows the pair count grows by 16× and the later warm time grows from 6,673 µs to 93,058 µs, about 14×. That is the `N × M` term. Halving the copied input does not move the time in a consistent direction: the 4,000-row warm sample went from 90,669 µs to 93,058 µs. Pair evaluation dominates the copy. Nothing in these numbers is an index lookup.

## Milestone 8 index nested-loop join

Measured on 14 November 2026. The database is a file. Rows are loaded, the file is migrated, and the nested-loop samples run before `CREATE INDEX idx_rhs_id ON rhs(id)`. The index samples run after that statement. Index build time is not included. Each input is `lhs (id INT, note INT)` and `rhs (id INT, note INT)`. Identity rows use `id = note = 0..N-1` on both sides, so the join returns `N` rows. The duplicate shape sets each right id to `id % 50`. Keys `0..49` match, and the other left keys match nothing. Both shapes return `N` rows. The timed statement is `SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id`. The checksum is the sum of the returned left ids, and the run checks that both paths produce it. `std::hint::black_box` consumes that sum and the row count.

```bash
cargo test --release --test milestone8 profile_index_nested_loop_join -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --offline --test milestone8 profile_index_nested_loop_join -- --ignored --nocapture` |
| Algorithm | Nested loop, then one equality probe per outer row. No cost comparison. |

The warm time is the median of five repeats. The cold time is the first sample. Speedup is nested warm divided by index warm. Result bytes are the returned rows, `matches × 72`. Both paths copy one input of `N` rows before comparing. These counts are not process RSS.

| Shape | N | Pairs | Matches | Nested cold | Nested warm | Index cold | Index warm | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| identity | 100 | 10,000 | 100 | 428 µs | 111 µs | 196 µs | 76 µs | 1.46× |
| duplicate | 100 | 10,000 | 100 | 193 µs | 112 µs | 262 µs | 98 µs | 1.14× |
| identity | 1,000 | 1,000,000 | 1,000 | 9,490 µs | 8,735 µs | 1,077 µs | 805 µs | 10.85× |
| duplicate | 1,000 | 1,000,000 | 1,000 | 8,493 µs | 6,558 µs | 1,160 µs | 904 µs | 7.25× |
| identity | 4,000 | 16,000,000 | 4,000 | 108,798 µs | 98,952 µs | 3,627 µs | 2,966 µs | 33.36× |
| duplicate | 4,000 | 16,000,000 | 4,000 | 101,350 µs | 97,905 µs | 4,380 µs | 3,750 µs | 26.11× |
| identity | 10,000 | 100,000,000 | 10,000 | 616,276 µs | 622,670 µs | 10,341 µs | 7,982 µs | 78.01× |
| duplicate | 10,000 | 100,000,000 | 10,000 | 639,049 µs | 614,805 µs | 12,746 µs | 11,404 µs | 53.91× |

From 1,000 to 10,000 identity rows the pair count grows by 100× and the nested warm time grows from 8,735 µs to 622,670 µs, about 71×. The index warm time grows from 805 µs to 7,982 µs, about 10×, which tracks the outer row count rather than `N × M`. Both shapes return `N` rows. On this run the index warm time is lower at every measured point, including 100 identity rows (76 µs against 111 µs). An earlier run on this machine measured that same 100-row identity case at 125 µs against 99 µs, a 0.79× speedup. The planner still chooses the index. There is no cost model.

## Milestone 9 in-memory hash join

Measured on 14 November 2026. The database is a file. Rows are loaded and migrated before timing. The statement is `SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id`. Nested-loop samples run first, with the planner forced onto `NestedLoopJoin`. Hash samples run next, before any index exists. Index samples run after `CREATE INDEX idx_rhs_id ON rhs(id)`. Index build time is not included. Cold is the first sample. Warm is the median of the next five. All three paths must return the same row count and the same sum of left ids. `std::hint::black_box` consumes that sum and the row count.

Identity rows use `id = note = 0..N-1` on both sides, so the join returns `N` rows. The duplicate shape sets each right id to `id % 50` and still returns `N` rows. The null shape stores `NULL` for every even id on both sides and returns `N / 2` rows. `skew-outer` is 100 left rows against 10,000 right rows whose ids are `id % 100`, returning 10,000 rows. `skew-inner` is 10,000 left rows whose ids are `id % 100` against 100 right rows, returning 10,000 rows. `skew-outer-wide` is 100 left rows against 40,000 right rows whose ids are `id % 100`. `skew-outer-mid` is 1,000 left rows against 20,000 right rows whose ids are `id % 1000`. Equal sizes build the right table. The skew shapes build the smaller side.

`hash_bytes` is `positions * size_of::<usize>() + groups * (size_of::<usize>() + size_of::<Vec<usize>>()) + groups * (size_of::<u64>() + size_of::<Vec<KeyGroup>>())`. On this machine that is `positions * 8 + groups * 64`. `positions` is the number of non-NULL build keys. `groups` is the number of distinct build keys. A hash collision shares one map entry and adds another key group, so this count is an upper bound on map entries and the payload of one group per key. It does not include allocator slack, the copied build rows, or the probed table, and it is not process RSS.

```bash
cargo test --release --lib profile_hash_join -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --offline --lib profile_hash_join -- --ignored --nocapture` |
| Algorithm | Nested loop, then hash join, then one equality probe per outer row. The index sample forces the index path. The planner chooses hash unless the inner table is empty-probed or more than 64 times the outer table. |

Speedup is the nested warm time divided by the other warm time.

| Shape | Outer | Inner | Chosen | Matches | Build | Hash bytes | Nested warm | Hash warm | Index warm | Hash speedup | Index speedup |
| --- | ---: | ---: | --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| identity | 100 | 100 | HashJoin | 100 | rhs | 7,200 | 124 µs | 34 µs | 94 µs | 3.65× | 1.32× |
| duplicate | 100 | 100 | HashJoin | 100 | rhs | 4,000 | 102 µs | 25 µs | 132 µs | 4.08× | 0.77× |
| nulls | 100 | 100 | HashJoin | 50 | rhs | 3,600 | 150 µs | 33 µs | 64 µs | 4.55× | 2.34× |
| identity | 1,000 | 1,000 | HashJoin | 1,000 | rhs | 72,000 | 8,191 µs | 144 µs | 846 µs | 56.88× | 9.68× |
| duplicate | 1,000 | 1,000 | HashJoin | 1,000 | rhs | 11,200 | 6,811 µs | 102 µs | 952 µs | 66.77× | 7.15× |
| nulls | 1,000 | 1,000 | HashJoin | 500 | rhs | 36,000 | 6,030 µs | 76 µs | 400 µs | 79.34× | 15.07× |
| identity | 4,000 | 4,000 | HashJoin | 4,000 | rhs | 288,000 | 92,453 µs | 465 µs | 2,819 µs | 198.82× | 32.80× |
| duplicate | 4,000 | 4,000 | HashJoin | 4,000 | rhs | 35,200 | 91,343 µs | 343 µs | 3,742 µs | 266.31× | 24.41× |
| nulls | 4,000 | 4,000 | HashJoin | 2,000 | rhs | 144,000 | 89,085 µs | 278 µs | 1,425 µs | 320.45× | 62.52× |
| identity | 10,000 | 10,000 | HashJoin | 10,000 | rhs | 720,000 | 570,614 µs | 1,157 µs | 7,669 µs | 493.18× | 74.41× |
| duplicate | 10,000 | 10,000 | HashJoin | 10,000 | rhs | 83,200 | 589,898 µs | 831 µs | 10,896 µs | 709.87× | 54.14× |
| nulls | 10,000 | 10,000 | HashJoin | 5,000 | rhs | 360,000 | 543,034 µs | 698 µs | 3,780 µs | 777.99× | 143.66× |
| skew-outer | 100 | 10,000 | IndexNestedLoopJoin | 10,000 | lhs | 7,200 | 6,783 µs | 572 µs | 524 µs | 11.86× | 12.94× |
| skew-inner | 10,000 | 100 | HashJoin | 10,000 | rhs | 7,200 | 6,616 µs | 413 µs | 4,835 µs | 16.02× | 1.37× |
| skew-outer-wide | 100 | 40,000 | IndexNestedLoopJoin | 40,000 | lhs | 7,200 | 24,409 µs | 2,289 µs | 2,054 µs | 10.66× | 11.88× |
| skew-outer-mid | 1,000 | 20,000 | HashJoin | 20,000 | lhs | 72,000 | 117,962 µs | 1,177 µs | 1,850 µs | 100.22× | 63.76× |

Hash is faster than the nested loop at every point. It is also faster than the index except when the inner table is much larger than the outer table: 100× is 572 µs against 524 µs, and 400× is 2,289 µs against 2,054 µs. At 20× the hash scan is still faster, 1,177 µs against 1,850 µs. The planner therefore hashes a same-type equality unless the outer table is empty or the inner table has more than 64 times as many rows. There is no cost model and no histogram. The index column is the cost of forcing that path; it is not the path chosen for the equal-size rows.

## Milestone 10 single-table aggregation

Measured on 14 November 2026. The database is in memory, so the times are the query and not `fsync`. Each size builds `samples(id INT, bucket INT, amount INT, flag BOOLEAN)` with one `INSERT` per row. `bucket` is `id % 10`. `amount` is `NULL` when `id` is even and `id` otherwise. `flag` is `FALSE` when `id % 3 = 0` and `TRUE` otherwise. Insert time is not included.

Each query runs six times. Cold is the first sample. Warm is the median of the next five. The harness adds every returned integer into a checksum, counts the rows, and passes both to `std::hint::black_box`. `returned` matched `groups` on every sample.

`bytes` is an estimate of the grouping state, not process RSS. A hash group stores a key vector (`size_of::<Vec<Value>>() + key_cells * size_of::<Value>()`), an accumulator vector (`size_of::<Vec<Value>>() + 16` bytes per aggregate), and one map entry (`size_of::<u64>() + size_of::<Vec<usize>>() + size_of::<usize>()`). On this machine one integer key and one aggregate cost 128 bytes. `AccValue` is 16 bytes: one `i64` and a tag that also represents `Sum(None)`. A scalar aggregate has no key vector and no map entry. Its 40 bytes are one accumulator vector. The estimate does not include allocator slack, empty hash slots, the input table, or the rows returned to the caller. An earlier draft of this section omitted the accumulator vector header, so its grouped sizes were 24 bytes per group smaller.

```bash
cargo test --release --offline --test milestone10 profile_aggregation -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --offline --test milestone10 profile_aggregation -- --ignored --nocapture` |
| Algorithm | Filter, then one hash table of groups, then sort and limit. No spill. |

| Shape | Rows | Groups | Bytes | Cold | Warm | Earlier warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `COUNT(*)` | 1,000 | 1 | 40 | 280 µs | 8 µs | 8 µs |
| few keys | 1,000 | 10 | 1,280 | 46 µs | 40 µs | 37 µs |
| many keys | 1,000 | 1,000 | 128,000 | 294 µs | 254 µs | 203 µs |
| `SUM` with NULLs | 1,000 | 1 | 40 | 14 µs | 9 µs | 8 µs |
| filtered | 1,000 | 10 | 1,280 | 61 µs | 45 µs | 42 µs |
| `COUNT(*)` | 10,000 | 1 | 40 | 30 µs | 26 µs | 26 µs |
| few keys | 10,000 | 10 | 1,280 | 215 µs | 210 µs | 209 µs |
| many keys | 10,000 | 10,000 | 1,280,000 | 1,791 µs | 1,525 µs | 1,438 µs |
| `SUM` with NULLs | 10,000 | 1 | 40 | 42 µs | 33 µs | 30 µs |
| filtered | 10,000 | 10 | 1,280 | 251 µs | 241 µs | 234 µs |
| `COUNT(*)` | 100,000 | 1 | 40 | 246 µs | 204 µs | 189 µs |
| few keys | 100,000 | 10 | 1,280 | 2,015 µs | 1,930 µs | 1,919 µs |
| many keys | 100,000 | 100,000 | 12,800,000 | 16,699 µs | 14,591 µs | 16,067 µs |
| `SUM` with NULLs | 100,000 | 1 | 40 | 304 µs | 267 µs | 242 µs |
| filtered | 100,000 | 10 | 1,280 | 2,313 µs | 2,256 µs | 2,320 µs |

Few keys is `SELECT bucket, COUNT(*) FROM samples GROUP BY bucket`. Many keys is `SELECT id, SUM(amount) FROM samples GROUP BY id`. Filtered is `SELECT bucket, SUM(amount) FROM samples WHERE flag = TRUE GROUP BY bucket`. Even `amount` values are NULL, so the scalar `SUM` adds half of the rows. The earlier warm column is the first release run, taken before this review. The operator is the same. The review build reads one test-only thread-local before hashing a group, so a collision test can put every tuple in one bucket.

From 10,000 to 100,000 rows every shape grows by about 8× to 10×. `COUNT(*)` goes from 26 µs to 204 µs. The 10-group queries go from 210 µs and 241 µs to 1,930 µs and 2,256 µs. The 100,000-group query goes from 1,525 µs to 14,591 µs. Its grouping state grows with the input, from 1,280,000 bytes to 12,800,000 bytes. Against the earlier run, the 100,000-row scalar shapes differ by 15 µs and 25 µs, the 10-group shapes differ by 11 µs and 64 µs, and the 100,000-group shape is 1,476 µs faster. That spread is two runs of the same operator. The 1,000-row samples are too small for a slope.

## Milestone 11 join aggregation

Measured on 14 November 2026 and retaken in the pre-merge review, after a failing aggregate row stopped changing accumulators that earlier aggregates in that row had already updated. Each size builds `users(id INT, name TEXT, bucket INT)` and `orders(user_id INT, amount INT)`. `bucket` is `id % 10`. There are four orders per user. An order whose id is divisible by 3 points at user 0; every other order points at `id % users`. `amount` is `NULL` when the order id is even and the order id otherwise. Every order matches a user, so the match count is the order count. Insert time is not included.

The query is `SELECT u.bucket, COUNT(*), SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.bucket`, or the same query grouped by `u.id`. The `HAVING` form adds `HAVING COUNT(*) >= users * 4 / 10`. Each query runs six times. Cold is the first sample. Warm is the median of the next five. The harness adds every returned `COUNT(*)` into a checksum and passes the rows to `std::hint::black_box`.

Nested-loop samples are forced through the nested-loop executor on an in-memory database. Hash samples use the planner's hash join on an in-memory database. Index samples open a file, migrate it, create `idx_orders_user` on `orders(user_id)`, and force the index probe. The inner table is four times the outer table, which is below the planner's 64× rule, so the forced index is the only reason those samples probe. The hash join builds the smaller side. Here that is the outer table, so it buffers one pair of `usize` positions per match and then emits pairs in outer-then-inner order. That buffer is 16 bytes per match. It is not included in `bytes`.

`bytes` estimates the grouping state after every pair has been accumulated, before `HAVING` drops groups. A group stores a key vector (`size_of::<Vec<Value>>() + size_of::<Value>()`), an accumulator vector (`size_of::<Vec<AccValue>>() + 32` for `COUNT` and `SUM`), and one map entry (`size_of::<u64>() + size_of::<Vec<usize>>() + size_of::<usize>()`). On this machine that is 144 bytes per group. `AccValue` is 16 bytes. The bucket queries accumulate 10 groups even when `HAVING` returns one of them. `pair_bytes` is the joined rows a materialized aggregate would store and the streaming path does not: one 24-byte vector header plus five 24-byte cells per match. It is not process RSS. The materialized samples collect those rows, then group them. The streaming samples hand each pair to the accumulators.

```bash
cargo test --release --offline --lib profile_join_aggregation -- --ignored --nocapture
```

| Item | Value |
| --- | --- |
| Machine | Apple M5, 16 GiB RAM (`hw.memsize` 17,179,869,184), `arm64` |
| Compiler | `rustc 1.100.0 (0ef1bc386 2026-11-09)` |
| Build | `cargo test --release --offline --lib profile_join_aggregation -- --ignored --nocapture` |
| Algorithm | Join, then one hash table of groups, then `HAVING`. No spill. |

| Algorithm | Mode | Users | Orders | Matches | Key | Returned | Groups | Bytes | Pair bytes | Cold | Warm |
| --- | --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Nested loop | stream | 200 | 800 | 800 | bucket | 10 | 10 | 1,440 | 115,200 | 1,679 µs | 1,361 µs |
| Hash | stream | 200 | 800 | 800 | bucket | 10 | 10 | 1,440 | 115,200 | 143 µs | 117 µs |
| Nested loop | stream | 200 | 800 | 800 | id | 200 | 200 | 28,800 | 115,200 | 1,236 µs | 1,196 µs |
| Hash | stream | 200 | 800 | 800 | id | 200 | 200 | 28,800 | 115,200 | 179 µs | 140 µs |
| Nested loop | stream | 200 | 800 | 800 | bucket | 1 | 10 | 1,440 | 115,200 | 1,121 µs | 1,062 µs |
| Hash | stream | 200 | 800 | 800 | bucket | 1 | 10 | 1,440 | 115,200 | 109 µs | 99 µs |
| Index | stream | 200 | 800 | 800 | bucket | 10 | 10 | 1,440 | 115,200 | 553 µs | 289 µs |
| Index | stream | 200 | 800 | 800 | id | 200 | 200 | 28,800 | 115,200 | 318 µs | 290 µs |
| Index | stream | 200 | 800 | 800 | bucket | 1 | 10 | 1,440 | 115,200 | 261 µs | 254 µs |
| Nested loop | stream | 1,000 | 4,000 | 4,000 | bucket | 10 | 10 | 1,440 | 576,000 | 23,401 µs | 22,797 µs |
| Hash | stream | 1,000 | 4,000 | 4,000 | bucket | 10 | 10 | 1,440 | 576,000 | 408 µs | 399 µs |
| Nested loop | stream | 1,000 | 4,000 | 4,000 | id | 1,000 | 1,000 | 144,000 | 576,000 | 23,063 µs | 23,007 µs |
| Hash | stream | 1,000 | 4,000 | 4,000 | id | 1,000 | 1,000 | 144,000 | 576,000 | 527 µs | 506 µs |
| Nested loop | stream | 1,000 | 4,000 | 4,000 | bucket | 1 | 10 | 1,440 | 576,000 | 22,886 µs | 22,877 µs |
| Hash | stream | 1,000 | 4,000 | 4,000 | bucket | 1 | 10 | 1,440 | 576,000 | 398 µs | 412 µs |
| Index | stream | 1,000 | 4,000 | 4,000 | bucket | 10 | 10 | 1,440 | 576,000 | 2,355 µs | 1,349 µs |
| Index | stream | 1,000 | 4,000 | 4,000 | id | 1,000 | 1,000 | 144,000 | 576,000 | 1,431 µs | 1,428 µs |
| Index | stream | 1,000 | 4,000 | 4,000 | bucket | 1 | 10 | 1,440 | 576,000 | 1,189 µs | 1,113 µs |
| Nested loop | materialized | 1,000 | 4,000 | 4,000 | bucket | 10 | 10 | 1,440 | 576,000 | 23,799 µs | 23,190 µs |
| Hash | materialized | 1,000 | 4,000 | 4,000 | bucket | 10 | 10 | 1,440 | 576,000 | 597 µs | 600 µs |
| Nested loop | materialized | 1,000 | 4,000 | 4,000 | id | 1,000 | 1,000 | 144,000 | 576,000 | 23,126 µs | 23,804 µs |
| Hash | materialized | 1,000 | 4,000 | 4,000 | id | 1,000 | 1,000 | 144,000 | 576,000 | 692 µs | 667 µs |
| Nested loop | stream | 4,000 | 16,000 | 16,000 | bucket | 10 | 10 | 1,440 | 2,304,000 | 365,024 µs | 362,340 µs |
| Hash | stream | 4,000 | 16,000 | 16,000 | bucket | 10 | 10 | 1,440 | 2,304,000 | 1,589 µs | 1,575 µs |
| Nested loop | stream | 4,000 | 16,000 | 16,000 | id | 4,000 | 4,000 | 576,000 | 2,304,000 | 362,432 µs | 364,137 µs |
| Hash | stream | 4,000 | 16,000 | 16,000 | id | 4,000 | 4,000 | 576,000 | 2,304,000 | 2,137 µs | 2,028 µs |
| Nested loop | stream | 4,000 | 16,000 | 16,000 | bucket | 1 | 10 | 1,440 | 2,304,000 | 363,090 µs | 370,212 µs |
| Hash | stream | 4,000 | 16,000 | 16,000 | bucket | 1 | 10 | 1,440 | 2,304,000 | 1,647 µs | 1,583 µs |
| Index | stream | 4,000 | 16,000 | 16,000 | bucket | 10 | 10 | 1,440 | 2,304,000 | 6,830 µs | 4,269 µs |
| Index | stream | 4,000 | 16,000 | 16,000 | id | 4,000 | 4,000 | 576,000 | 2,304,000 | 4,863 µs | 4,867 µs |
| Index | stream | 4,000 | 16,000 | 16,000 | bucket | 1 | 10 | 1,440 | 2,304,000 | 4,355 µs | 4,287 µs |
| Nested loop | materialized | 4,000 | 16,000 | 16,000 | bucket | 10 | 10 | 1,440 | 2,304,000 | 372,850 µs | 373,546 µs |
| Hash | materialized | 4,000 | 16,000 | 16,000 | bucket | 10 | 10 | 1,440 | 2,304,000 | 2,327 µs | 2,217 µs |
| Nested loop | materialized | 4,000 | 16,000 | 16,000 | id | 4,000 | 4,000 | 576,000 | 2,304,000 | 371,215 µs | 374,993 µs |
| Hash | materialized | 4,000 | 16,000 | 16,000 | id | 4,000 | 4,000 | 576,000 | 2,304,000 | 2,726 µs | 2,719 µs |

Rows whose `Returned` column is 1 are the `HAVING` samples. The threshold keeps the one bucket that received the extra orders. Its `COUNT(*)` checksum is 320, 1,600, and 6,400 at 200, 1,000, and 4,000 users. The other bucket samples return all 10 groups. Grouping by `id` returns one group per user. Checksums match across the three join algorithms and both execution modes.

At 4,000 users the streaming hash join takes 1,575 µs for 10 groups and 2,028 µs for 4,000 groups. The nested loop takes 362,340 µs and 364,137 µs. Both of those paths stream, so that gap is the join algorithm. The index probe takes 4,269 µs and 4,867 µs, also streaming. Storing the joined rows before grouping adds 642 µs on the 10-group hash path (2,217 µs) and 691 µs on the 4,000-group hash path (2,719 µs). The nested-loop materialized times are 373,546 µs and 374,993 µs. The stored pairs would be 2,304,000 bytes. The grouping state is 1,440 bytes for 10 groups and 576,000 bytes for 4,000 groups. `HAVING` warm times are 1,583 µs for the hash join and 370,212 µs for the nested loop. The filter drops nine groups after they have been accumulated, so the 10-group estimate still applies. The outer-build hash path also keeps 256,000 bytes of match positions at this size. That buffer is separate from `bytes` and from `pair_bytes`.

## Milestone 12 AVG, MIN, MAX

One release run on an in-memory database, 15 November 2026. The comparison fast path is the `i128` product. These queries do not overflow it.

```text
cargo test --release --offline --lib profile_milestone12 -- --ignored --nocapture
```

`milestone12 cpu=Apple M5 mem_bytes=17179869184 rustc=rustc 1.100.0 (0ef1bc386 2026-11-09) acc_bytes=48 value_bytes=32`

`staff` has `id`, `name`, `bucket = id % 10`, `salary = id`, and `active`. `users` has the same ids and buckets. `orders` has four rows per user; even order ids store `NULL` and odd ids store the order id. The timer starts after the inserts. Each shape runs six times. Cold is the first sample. Warm is the median of the other five. A scalar `bytes` value is one accumulator vector. A grouped `bytes` value is the key vector, the accumulator vector, and one hash-map entry per accumulated group, including groups that `HAVING` later drops. Neither figure is process RSS.

`SUM(salary)` of `0 .. rows-1` is `(rows-1)*rows/2`. The printed checksums match that integer: 499,500, 49,995,000, and 4,999,950,000. `AVG` of the same column is `(rows-1)/2`. The checksum adds the reduced numerator to the denominator, which is 1,001, 10,001, and 100,001. `sum_avg` adds those two checksums.

| Shape | Rows | Returned | Groups | Aggs | Bytes | Checksum | Cold | Warm |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `SUM` | 1,000 | 1 | 1 | 1 | 72 | 499,500 | 240 µs | 40 µs |
| `AVG` | 1,000 | 1 | 1 | 1 | 72 | 1,001 | 41 µs | 39 µs |
| `SUM` and `AVG` | 1,000 | 1 | 1 | 2 | 120 | 500,501 | 48 µs | 46 µs |
| 10 groups, four aggregates | 1,000 | 10 | 10 | 4 | 3,120 | 15,076 | 183 µs | 94 µs |
| one group per row | 1,000 | 1,000 | 1,000 | 1 | 168,000 | 1,000,000 | 257 µs | 225 µs |
| `HAVING AVG > rows/2` | 1,000 | 4 | 10 | 1 | 1,680 | 2,044 | 102 µs | 71 µs |
| hash join, 10 groups | 1,000 | 10 | 10 | 3 | 2,640 | 30,050 | 674 µs | 546 µs |
| `SUM` | 10,000 | 1 | 1 | 1 | 72 | 49,995,000 | 267 µs | 228 µs |
| `AVG` | 10,000 | 1 | 1 | 1 | 72 | 10,001 | 252 µs | 221 µs |
| `SUM` and `AVG` | 10,000 | 1 | 1 | 2 | 120 | 50,005,001 | 277 µs | 259 µs |
| 10 groups, four aggregates | 10,000 | 10 | 10 | 4 | 3,120 | 150,084 | 529 µs | 513 µs |
| one group per row | 10,000 | 10,000 | 10,000 | 1 | 1,680,000 | 100,000,000 | 1,771 µs | 1,548 µs |
| `HAVING AVG > rows/2` | 10,000 | 4 | 10 | 1 | 1,680 | 20,044 | 469 µs | 461 µs |
| hash join, 10 groups | 10,000 | 10 | 10 | 3 | 2,640 | 300,050 | 4,156 µs | 4,087 µs |
| `SUM` | 100,000 | 1 | 1 | 1 | 72 | 4,999,950,000 | 3,396 µs | 2,513 µs |
| `AVG` | 100,000 | 1 | 1 | 1 | 72 | 100,001 | 2,351 µs | 2,506 µs |
| `SUM` and `AVG` | 100,000 | 1 | 1 | 2 | 120 | 5,000,050,001 | 2,655 µs | 2,725 µs |
| 10 groups, four aggregates | 100,000 | 10 | 10 | 4 | 3,120 | 1,500,092 | 5,432 µs | 5,060 µs |
| one group per row | 100,000 | 100,000 | 100,000 | 1 | 16,800,000 | 10,000,000,000 | 18,391 µs | 16,969 µs |
| `HAVING AVG > rows/2` | 100,000 | 4 | 10 | 1 | 1,680 | 200,044 | 4,549 µs | 4,494 µs |
| hash join, 10 groups | 100,000 | 10 | 10 | 3 | 2,640 | 3,000,050 | 48,172 µs | 46,507 µs |

At 100,000 rows the scalar `SUM` warm time is 2,513 µs and the scalar `AVG` warm time is 2,506 µs. Running both in one statement is 2,725 µs. The `i128` sum does not show up as its own cost beside the scan on this run. These averages fit in the `i128` cross product, so the 256-bit comparison path is not on this timer. The 1,000-row `SUM` cold sample is 240 µs because it is the first query after the load; its warm time is 40 µs, next to `AVG` at 39 µs. Ten groups with `AVG`, `MIN`, `MAX`, and `MIN` of text take 5,060 µs warm at 100,000 rows. One group per row takes 16,969 µs and the estimate is 16,800,000 bytes. `HAVING` returns 4 of the 10 groups and the byte estimate still counts all 10.

The join builds 4 rows in `orders` per user and keeps 10 groups. Warm time is 546 µs, 4,087 µs, and 46,507 µs at 1,000, 10,000, and 100,000 users. Milestone 11's streaming hash of `COUNT` and `SUM` at 4,000 users and 16,000 matches was 1,575 µs warm. That statement, the row counts, and the aggregates are different, so the two tables are not a before-and-after for `AVG`.

`AccValue` is 48 bytes and `Value` is 32 bytes. Milestone 10's published byte column assumed a 16-byte accumulator. A new run of that profile would print a larger `bytes` number for the same groups. The Milestone 10 table was not remeasured.

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
- The Milestone 7 rows are two release runs on one in-memory database, taken immediately before and after the scan stopped copying both inputs. The warm time is the median of five repeats. The cold time is the first sample. The copied-byte column counts the row vectors that the scan clones. It is not process RSS. The join does not read an index, so the pair count is `N × M` for every shape. The 4,000-row times differ by a few percent between the runs; that spread is larger than the cost of the removed copy.
- The Milestone 8 rows are one release run on a file database, retaken after the sibling-link check. The nested-loop samples run before the index exists. The index samples run after `CREATE INDEX`, with the tree in the pager cache after the cold sample. The warm time is the median of five repeats. The cold time is the first sample. Both paths are checked against the same checksum. On this run the index is faster at every measured point. An earlier run on the same machine measured the 100-row identity join slower on the index path (125 µs against 99 µs).
- The Milestone 9 rows are one release run on a file database, retaken after key groups replaced the bare fingerprint. Nested-loop samples are forced through the nested-loop executor. Hash samples run before the index exists. Index samples force the index path after `CREATE INDEX`. The warm time is the median of five repeats. The cold time is the first sample. `hash_bytes` counts positions, key-group headers, and map entries. It is not process RSS. The planner hashes unless the inner table is more than 64 times the outer table.
- The Milestone 10 table is a second release run on an in-memory database. The warm time is the median of five repeats. The cold time is the first sample. `bytes` counts the key vector, the accumulator vector, and one hash-map entry per group. It is not process RSS. A scalar aggregate has no hash map. The input table and the returned rows are not included. The earlier warm column is the first run of the same operator. The 100,000-group sample moved by 1,476 µs between those runs.
- The Milestone 11 table is the pre-merge review run. Nested-loop and hash samples are in memory. Index samples are on a file after `CREATE INDEX`, with the probe forced because the inner table is only four times the outer table. The warm time is the median of five repeats. The cold time is the first sample. `bytes` is the grouping state before `HAVING`. `pair_bytes` is the joined rows the streaming path does not store. The hash path still stores 16 bytes of match positions per pair when it builds the outer table. None of those figures are process RSS.
- The Milestone 12 table is one release run on an in-memory database. The warm time is the median of five repeats. The cold time is the first sample. Scalar `bytes` is the accumulator vector. Grouped `bytes` also counts key cells and one hash-map entry per accumulated group. `HAVING` still charges the groups it drops. The join is the in-memory hash join. None of those figures are process RSS. The Milestone 10 byte formula in code now uses the 48-byte accumulator; the published Milestone 10 table still shows the earlier 16-byte estimate.
- The Milestone 13 table is the pre-merge review run on an in-memory database. The warm time is the median of five repeats. The cold time is the first sample. `acc_bytes` is the numeric accumulator state. `set_bytes` counts, per group, the `seen` vector header and the `Option` that owns the map, then one hash, one bucket header, and one `Value` per stored cell, plus the inserted text lengths. `Option<DistinctSet>` is 48 bytes, the same as the map. It does not include hash-table control bytes, spare capacity, or process RSS. A plain aggregate has `set_bytes` 0. Low-cardinality `SUM DISTINCT` and `AVG DISTINCT` are not slower than the plain forms on this run because they stop adding after 17 values.
- The Milestone 14 table is the pre-merge review run. Plain shapes are in memory and examine every stored row. The sequential range and the indexed range are the same migrated file and the same rows, measured before and after `CREATE INDEX`. The index examines the 30 keys in `id >= 10 AND id < 40`. The warm time is the median of five repeats. The cold time is the first sample. `bytes` counts one projected row header and one `Value` per output column, plus text lengths. `SELECT DISTINCT` also counts one hash and one bucket slot per unique row. `ORDER BY` / `LIMIT` still charges every unique label, not the three returned rows. It is not process RSS. An earlier run of the same operator on this machine recorded warm `DISTINCT id` at 15,667 µs; this run is 13,725 µs. That spread is run-to-run variation.

## Milestone 13 HAVING aliases and DISTINCT

Release profile `profile_milestone13`, pre-merge review, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `AccValue` is 48 bytes, `Value` is 32 bytes, `DistinctSet` is 48 bytes, and `Option<DistinctSet>` is also 48 bytes. The per-group `seen` vector header is 24 bytes. Each size is one in-memory load. The warm time is the median of the next five samples. The cold time is the first sample. Checksums are the printed wrapping sums, and a wrong result fails the run.

`items` has `id`, a one-character `label` (`a` through `h`), a unique `tag` (`n` plus the id), `bucket = id % 10`, and `amount = id % 17` except when `id` is divisible by 11, where `amount` is NULL. `users` has one row per id. `orders` has four rows per user. Even order ids store NULL. Odd order ids store `id % 17`. The join keeps 10 groups. Its distinct count is the sum of the per-group set sizes, 85 at every measured size.

| Shape | Rows | Returned | Groups | Distinct | Text bytes | Acc bytes | Set bytes | Checksum | Cold | Warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `COUNT(*)` | 1,000 | 1 | 1 | 0 | 0 | 72 | 0 | 1,000 | 378 µs | 37 µs |
| `COUNT(DISTINCT bucket)` | 1,000 | 1 | 1 | 10 | 0 | 72 | 712 | 10 | 63 µs | 38 µs |
| `COUNT(DISTINCT id)` | 1,000 | 1 | 1 | 1,000 | 0 | 72 | 64,072 | 1,000 | 176 µs | 162 µs |
| `COUNT(DISTINCT label)` | 1,000 | 1 | 1 | 8 | 8 | 72 | 592 | 8 | 40 µs | 37 µs |
| `COUNT(DISTINCT tag)` | 1,000 | 1 | 1 | 1,000 | 3,890 | 72 | 67,962 | 1,000 | 187 µs | 184 µs |
| `SUM(amount)` | 1,000 | 1 | 1 | 0 | 0 | 72 | 0 | 7,253 | 40 µs | 37 µs |
| `SUM(DISTINCT amount)` | 1,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 136 | 36 µs | 35 µs |
| `AVG(amount)` | 1,000 | 1 | 1 | 0 | 0 | 72 | 0 | 8,162 | 37 µs | 36 µs |
| `AVG(DISTINCT amount)` | 1,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 9 | 36 µs | 34 µs |
| `HAVING` alias | 1,000 | 10 | 10 | 1,000 | 0 | 1,680 | 64,720 | 1,045 | 257 µs | 182 µs |
| join `COUNT(DISTINCT amount)` | 1,000 | 10 | 10 | 85 | 0 | 1,680 | 6,160 | 130 | 617 µs | 535 µs |
| `COUNT(*)` | 10,000 | 1 | 1 | 0 | 0 | 72 | 0 | 10,000 | 245 µs | 218 µs |
| `COUNT(DISTINCT bucket)` | 10,000 | 1 | 1 | 10 | 0 | 72 | 712 | 10 | 311 µs | 217 µs |
| `COUNT(DISTINCT id)` | 10,000 | 1 | 1 | 10,000 | 0 | 72 | 640,072 | 10,000 | 1,190 µs | 1,121 µs |
| `COUNT(DISTINCT label)` | 10,000 | 1 | 1 | 8 | 8 | 72 | 592 | 8 | 242 µs | 218 µs |
| `COUNT(DISTINCT tag)` | 10,000 | 1 | 1 | 10,000 | 48,890 | 72 | 688,962 | 10,000 | 1,292 µs | 1,291 µs |
| `SUM(amount)` | 10,000 | 1 | 1 | 0 | 0 | 72 | 0 | 72,693 | 240 µs | 223 µs |
| `SUM(DISTINCT amount)` | 10,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 136 | 207 µs | 201 µs |
| `AVG(amount)` | 10,000 | 1 | 1 | 0 | 0 | 72 | 0 | 9,087 | 332 µs | 280 µs |
| `AVG(DISTINCT amount)` | 10,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 9 | 207 µs | 201 µs |
| `HAVING` alias | 10,000 | 10 | 10 | 10,000 | 0 | 1,680 | 640,720 | 10,045 | 1,585 µs | 1,525 µs |
| join `COUNT(DISTINCT amount)` | 10,000 | 10 | 10 | 85 | 0 | 1,680 | 6,160 | 130 | 4,107 µs | 4,008 µs |
| `COUNT(*)` | 100,000 | 1 | 1 | 0 | 0 | 72 | 0 | 100,000 | 2,669 µs | 2,842 µs |
| `COUNT(DISTINCT bucket)` | 100,000 | 1 | 1 | 10 | 0 | 72 | 712 | 10 | 3,218 µs | 2,330 µs |
| `COUNT(DISTINCT id)` | 100,000 | 1 | 1 | 100,000 | 0 | 72 | 6,400,072 | 100,000 | 14,500 µs | 13,642 µs |
| `COUNT(DISTINCT label)` | 100,000 | 1 | 1 | 8 | 8 | 72 | 592 | 8 | 3,205 µs | 2,431 µs |
| `COUNT(DISTINCT tag)` | 100,000 | 1 | 1 | 100,000 | 588,890 | 72 | 6,988,962 | 100,000 | 17,443 µs | 16,443 µs |
| `SUM(amount)` | 100,000 | 1 | 1 | 0 | 0 | 72 | 0 | 727,233 | 3,544 µs | 2,575 µs |
| `SUM(DISTINCT amount)` | 100,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 136 | 2,344 µs | 2,203 µs |
| `AVG(amount)` | 100,000 | 1 | 1 | 0 | 0 | 72 | 0 | 20,978 | 2,460 µs | 2,507 µs |
| `AVG(DISTINCT amount)` | 100,000 | 1 | 1 | 17 | 0 | 72 | 1,160 | 9 | 2,141 µs | 2,180 µs |
| `HAVING` alias | 100,000 | 10 | 10 | 100,000 | 0 | 1,680 | 6,400,720 | 100,045 | 15,883 µs | 15,711 µs |
| join `COUNT(DISTINCT amount)` | 100,000 | 10 | 10 | 85 | 0 | 1,680 | 6,160 | 130 | 48,624 µs | 44,821 µs |

At 100,000 rows, warm `COUNT(*)` is 2,842 µs. Warm `COUNT(DISTINCT bucket)` over 10 values is 2,330 µs. Warm `COUNT(DISTINCT id)` over 100,000 values is 13,642 µs, and the set estimate is 6,400,072 bytes. Warm `COUNT(DISTINCT label)` over 8 one-byte strings is 2,431 µs. Warm `COUNT(DISTINCT tag)` over 100,000 strings, 588,890 inserted text bytes, is 16,443 µs and the set estimate is 6,988,962 bytes. Warm `SUM(amount)` is 2,575 µs and warm `SUM(DISTINCT amount)` over 17 values is 2,203 µs. Warm `AVG(amount)` is 2,507 µs and warm `AVG(DISTINCT amount)` is 2,180 µs. The `HAVING total > rows/20` alias query keeps all 10 groups, each holding its share of the 100,000 ids, in 15,711 µs warm. The hash join of 100,000 users and 400,000 orders, 10 groups and 85 distinct memberships, is 44,821 µs warm. The distinct set on that join is 6,160 bytes. The earlier table on this machine recorded 13,982 µs for `COUNT(DISTINCT id)` and 63,413 µs for the join. This run's distinct shape is 13,642 µs. The join sample is 44,821 µs here, and plain `COUNT(*)` is 2,842 µs against 2,271 µs earlier. Those two moved by more than the distinct-set change, in opposite directions, so that spread is run-to-run variation. The Milestone 12 join time is a different statement and is not a before-and-after for this one. `set_bytes` is 24 bytes per group higher than that earlier table because this run counts the `seen` vector header.

## Milestone 14 SELECT DISTINCT

Release profile `profile_milestone14`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes and a row vector header is 24 bytes. A distinct map entry is 40 bytes: one `u64`, one bucket header, and one position. Each size is one in-memory load. The warm time is the median of the next five samples. The cold time is the first sample. Checksums are the printed wrapping sums of the returned cells, and a wrong result fails the run.

`items` has `id`, a one-character `label` (`a` through `h`), a unique `tag` of 24 spaces plus the id, `bucket = id % 10`, and `flag` true on even ids. Plain and `DISTINCT` forms read that same in-memory table and examine every stored row. The pair shape is `bucket, label` and has 40 unique pairs. `WHERE flag = TRUE` keeps buckets 0, 2, 4, 6, and 8. `ORDER BY label LIMIT 3` returns `a`, `b`, and `c` after the set has stored all 8 labels. The range is `id >= 10 AND id < 40` on one migrated file. `EXPLAIN` reports `SeqScan` before `CREATE INDEX idx_items_id` and `IndexScan` after it. Both return the same 10 buckets. The sequential scan examines every stored row. The index scan examines 30 keys. `COUNT(*)` for that predicate is 30.

| Shape | Rows | Examined | Returned | Unique | Width | Text bytes | Bytes | Checksum | Cold | Warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| plain `bucket` | 1,000 | 1,000 | 1,000 | 1,000 | 1 | 0 | 56,000 | 4,500 | 249 µs | 21 µs |
| `DISTINCT bucket` | 1,000 | 1,000 | 10 | 10 | 1 | 0 | 960 | 45 | 56 µs | 51 µs |
| plain `id` | 1,000 | 1,000 | 1,000 | 1,000 | 1 | 0 | 56,000 | 499,500 | 21 µs | 19 µs |
| `DISTINCT id` | 1,000 | 1,000 | 1,000 | 1,000 | 1 | 0 | 96,000 | 499,500 | 192 µs | 208 µs |
| plain pair | 1,000 | 1,000 | 1,000 | 1,000 | 2 | 1,000 | 89,000 | 5,500 | 45 µs | 31 µs |
| `DISTINCT` pair | 1,000 | 1,000 | 40 | 40 | 2 | 40 | 5,160 | 220 | 79 µs | 77 µs |
| plain `tag` | 1,000 | 1,000 | 1,000 | 1,000 | 1 | 26,890 | 82,890 | 26,890 | 32 µs | 30 µs |
| `DISTINCT tag` | 1,000 | 1,000 | 1,000 | 1,000 | 1 | 26,890 | 122,890 | 26,890 | 201 µs | 195 µs |
| `DISTINCT` where | 1,000 | 1,000 | 5 | 5 | 1 | 0 | 480 | 20 | 66 µs | 36 µs |
| `DISTINCT` order limit | 1,000 | 1,000 | 3 | 8 | 1 | 8 | 776 | 3 | 102 µs | 66 µs |
| file seq range | 1,000 | 1,000 | 10 | 10 | 1 | 0 | 960 | 45 | 39 µs | 37 µs |
| file index range | 1,000 | 30 | 10 | 10 | 1 | 0 | 960 | 45 | 18 µs | 16 µs |
| plain `bucket` | 10,000 | 10,000 | 10,000 | 10,000 | 1 | 0 | 560,000 | 45,000 | 212 µs | 129 µs |
| `DISTINCT bucket` | 10,000 | 10,000 | 10 | 10 | 1 | 0 | 960 | 45 | 365 µs | 362 µs |
| plain `id` | 10,000 | 10,000 | 10,000 | 10,000 | 1 | 0 | 560,000 | 49,995,000 | 128 µs | 124 µs |
| `DISTINCT id` | 10,000 | 10,000 | 10,000 | 10,000 | 1 | 0 | 960,000 | 49,995,000 | 1,496 µs | 1,429 µs |
| plain pair | 10,000 | 10,000 | 10,000 | 10,000 | 2 | 10,000 | 890,000 | 55,000 | 285 µs | 228 µs |
| `DISTINCT` pair | 10,000 | 10,000 | 40 | 40 | 2 | 40 | 5,160 | 220 | 528 µs | 513 µs |
| plain `tag` | 10,000 | 10,000 | 10,000 | 10,000 | 1 | 278,890 | 838,890 | 278,890 | 228 µs | 208 µs |
| `DISTINCT tag` | 10,000 | 10,000 | 10,000 | 10,000 | 1 | 278,890 | 1,238,890 | 278,890 | 1,468 µs | 1,440 µs |
| `DISTINCT` where | 10,000 | 10,000 | 5 | 5 | 1 | 0 | 480 | 20 | 254 µs | 234 µs |
| `DISTINCT` order limit | 10,000 | 10,000 | 3 | 8 | 1 | 8 | 776 | 3 | 462 µs | 449 µs |
| file seq range | 10,000 | 10,000 | 10 | 10 | 1 | 0 | 960 | 45 | 205 µs | 198 µs |
| file index range | 10,000 | 30 | 10 | 10 | 1 | 0 | 960 | 45 | 13 µs | 11 µs |
| plain `bucket` | 100,000 | 100,000 | 100,000 | 100,000 | 1 | 0 | 5,600,000 | 450,000 | 2,192 µs | 1,219 µs |
| `DISTINCT bucket` | 100,000 | 100,000 | 10 | 10 | 1 | 0 | 960 | 45 | 3,283 µs | 3,295 µs |
| plain `id` | 100,000 | 100,000 | 100,000 | 100,000 | 1 | 0 | 5,600,000 | 4,999,950,000 | 1,356 µs | 1,201 µs |
| `DISTINCT id` | 100,000 | 100,000 | 100,000 | 100,000 | 1 | 0 | 9,600,000 | 4,999,950,000 | 14,365 µs | 13,725 µs |
| plain pair | 100,000 | 100,000 | 100,000 | 100,000 | 2 | 100,000 | 8,900,000 | 550,000 | 2,420 µs | 2,172 µs |
| `DISTINCT` pair | 100,000 | 100,000 | 40 | 40 | 2 | 40 | 5,160 | 220 | 5,078 µs | 5,097 µs |
| plain `tag` | 100,000 | 100,000 | 100,000 | 100,000 | 1 | 2,888,890 | 8,488,890 | 2,888,890 | 2,422 µs | 2,278 µs |
| `DISTINCT tag` | 100,000 | 100,000 | 100,000 | 100,000 | 1 | 2,888,890 | 12,488,890 | 2,888,890 | 15,341 µs | 15,142 µs |
| `DISTINCT` where | 100,000 | 100,000 | 5 | 5 | 1 | 0 | 480 | 20 | 2,437 µs | 2,306 µs |
| `DISTINCT` order limit | 100,000 | 100,000 | 3 | 8 | 1 | 8 | 776 | 3 | 4,644 µs | 4,533 µs |
| file seq range | 100,000 | 100,000 | 10 | 10 | 1 | 0 | 960 | 45 | 2,020 µs | 1,849 µs |
| file index range | 100,000 | 30 | 10 | 10 | 1 | 0 | 960 | 45 | 12 µs | 11 µs |

At 100,000 rows, warm plain `bucket` is 1,219 µs and warm `DISTINCT bucket` over 10 values is 3,295 µs. Warm plain `id` is 1,201 µs and warm `DISTINCT id` over 100,000 values is 13,725 µs, with a 9,600,000-byte estimate. Warm plain `tag` is 2,278 µs and warm `DISTINCT tag` is 15,142 µs. The hash table is the extra cost: a plain scan only copies the projection. Low-cardinality `DISTINCT` still hashes every input row, so it is slower than the plain scan on this run. `WHERE`, `ORDER BY`, and `LIMIT` do not remove that scan. On the same file, the sequential range examines 100,000 rows and is 1,849 µs warm. After `CREATE INDEX` the same statement examines 30 keys and is 11 µs warm. Both return the same 10 buckets. `SELECT DISTINCT` does not add a skip scan.

## Milestone 16 SELECT DISTINCT JOIN

Release profile `profile_milestone16`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes and a row vector header is 24 bytes. A hash-join position pair is 16 bytes. The warm time is the median of the next five samples. The cold time is the first sample. A wrong row count fails the run. Checksums are the printed wrapping sums of the returned cells.

Each size is a keyed join, `users.id = orders.user_id`, not a Cartesian product. A 10,000 by 10,000 nested loop was not run. On the equal input every order matches one user, `amount` is NULL on even ids and `id % 5` on odd ids, `name` is one of `a` through `h`, and `tag` is the id printed in 64 characters. The duplicate input uses the same row counts and sets `user_id = id % 10`, so ten users own every order. The skewed input has `max(n / 100, 1)` users and `n` orders.

Equal sizes build the inner table, so the hash position buffer is empty. The planner selects `HashJoin` for those equalities. `NestedLoopJoin` and `IndexNestedLoopJoin` on the equal input are forced, so those rows compare the operators rather than the planner. The skewed index is the planner's choice: the inner table is more than 64 times the outer table and `orders.user_id` is indexed. The skewed hash row is the same data with no index. It builds the outer table and keeps one position pair per match. Nested loop and index nested loop do not keep that pair buffer. `Bytes` is the projected-row estimate for a plain select and the distinct-set estimate for `SELECT DISTINCT`. `ORDER BY` / `LIMIT` still charges all 24 unique pairs, not the five returned rows. Neither figure is process RSS.

| Strategy | Shape | Users | Orders | Matches | Returned | Unique | Bytes | Pair bytes | Cold | Warm |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HashJoin | plain `name` | 100 | 100 | 100 | 100 | 100 | 5,700 | 0 | 106 µs | 30 µs |
| HashJoin | `DISTINCT name` | 100 | 100 | 100 | 8 | 8 | 776 | 0 | 37 µs | 33 µs |
| HashJoin | plain ids | 100 | 100 | 100 | 100 | 100 | 8,800 | 0 | 28 µs | 27 µs |
| HashJoin | `DISTINCT` ids | 100 | 100 | 100 | 100 | 100 | 12,800 | 0 | 45 µs | 45 µs |
| HashJoin | `DISTINCT amount` | 100 | 100 | 100 | 6 | 6 | 576 | 0 | 30 µs | 28 µs |
| HashJoin | `DISTINCT tag` | 100 | 100 | 100 | 100 | 100 | 16,000 | 0 | 44 µs | 43 µs |
| HashJoin | `DISTINCT` order limit | 100 | 100 | 100 | 5 | 24 | 3,096 | 0 | 66 µs | 40 µs |
| HashJoin | duplicate plain `name` | 100 | 100 | 100 | 100 | 100 | 5,700 | 0 | 24 µs | 22 µs |
| HashJoin | duplicate `DISTINCT name` | 100 | 100 | 100 | 8 | 8 | 776 | 0 | 27 µs | 27 µs |
| HashJoin | skewed `DISTINCT name` | 1 | 100 | 100 | 1 | 1 | 97 | 1,600 | 22 µs | 19 µs |
| NestedLoopJoin | `DISTINCT name` | 100 | 100 | 100 | 8 | 8 | 776 | 0 | 111 µs | 109 µs |
| NestedLoopJoin | `DISTINCT` ids | 100 | 100 | 100 | 100 | 100 | 12,800 | 0 | 123 µs | 122 µs |
| IndexNestedLoopJoin | `DISTINCT name` | 100 | 100 | 100 | 8 | 8 | 776 | 0 | 210 µs | 107 µs |
| IndexNestedLoopJoin | `DISTINCT` ids | 100 | 100 | 100 | 100 | 100 | 12,800 | 0 | 125 µs | 134 µs |
| IndexNestedLoopJoin | skewed `DISTINCT name` | 1 | 100 | 100 | 1 | 1 | 97 | 0 | 87 µs | 26 µs |
| HashJoin | plain `name` | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 57,000 | 0 | 256 µs | 210 µs |
| HashJoin | `DISTINCT name` | 1,000 | 1,000 | 1,000 | 8 | 8 | 776 | 0 | 256 µs | 225 µs |
| HashJoin | plain ids | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 88,000 | 0 | 238 µs | 186 µs |
| HashJoin | `DISTINCT` ids | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 128,000 | 0 | 442 µs | 393 µs |
| HashJoin | `DISTINCT amount` | 1,000 | 1,000 | 1,000 | 6 | 6 | 576 | 0 | 215 µs | 213 µs |
| HashJoin | `DISTINCT tag` | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 160,000 | 0 | 383 µs | 371 µs |
| HashJoin | `DISTINCT` order limit | 1,000 | 1,000 | 1,000 | 5 | 24 | 3,096 | 0 | 234 µs | 226 µs |
| HashJoin | duplicate plain `name` | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 57,000 | 0 | 127 µs | 118 µs |
| HashJoin | duplicate `DISTINCT name` | 1,000 | 1,000 | 1,000 | 8 | 8 | 776 | 0 | 153 µs | 153 µs |
| HashJoin | skewed `DISTINCT name` | 10 | 1,000 | 1,000 | 8 | 8 | 776 | 16,000 | 141 µs | 123 µs |
| NestedLoopJoin | `DISTINCT name` | 1,000 | 1,000 | 1,000 | 8 | 8 | 776 | 0 | 7,698 µs | 6,911 µs |
| NestedLoopJoin | `DISTINCT` ids | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 128,000 | 0 | 6,703 µs | 6,604 µs |
| IndexNestedLoopJoin | `DISTINCT name` | 1,000 | 1,000 | 1,000 | 8 | 8 | 776 | 0 | 1,519 µs | 1,169 µs |
| IndexNestedLoopJoin | `DISTINCT` ids | 1,000 | 1,000 | 1,000 | 1,000 | 1,000 | 128,000 | 0 | 1,250 µs | 1,183 µs |
| IndexNestedLoopJoin | skewed `DISTINCT name` | 10 | 1,000 | 1,000 | 8 | 8 | 776 | 0 | 493 µs | 161 µs |
| HashJoin | plain `name` | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 228,000 | 0 | 715 µs | 629 µs |
| HashJoin | `DISTINCT name` | 4,000 | 4,000 | 4,000 | 8 | 8 | 776 | 0 | 769 µs | 718 µs |
| HashJoin | plain ids | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 352,000 | 0 | 620 µs | 579 µs |
| HashJoin | `DISTINCT` ids | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 512,000 | 0 | 1,284 µs | 1,174 µs |
| HashJoin | `DISTINCT amount` | 4,000 | 4,000 | 4,000 | 6 | 6 | 576 | 0 | 602 µs | 600 µs |
| HashJoin | `DISTINCT tag` | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 640,000 | 0 | 1,275 µs | 1,140 µs |
| HashJoin | `DISTINCT` order limit | 4,000 | 4,000 | 4,000 | 5 | 24 | 3,096 | 0 | 671 µs | 661 µs |
| HashJoin | duplicate plain `name` | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 228,000 | 0 | 387 µs | 348 µs |
| HashJoin | duplicate `DISTINCT name` | 4,000 | 4,000 | 4,000 | 8 | 8 | 776 | 0 | 547 µs | 457 µs |
| HashJoin | skewed `DISTINCT name` | 40 | 4,000 | 4,000 | 8 | 8 | 776 | 64,000 | 396 µs | 370 µs |
| NestedLoopJoin | `DISTINCT name` | 4,000 | 4,000 | 4,000 | 8 | 8 | 776 | 0 | 102,725 µs | 101,808 µs |
| IndexNestedLoopJoin | `DISTINCT name` | 4,000 | 4,000 | 4,000 | 8 | 8 | 776 | 0 | 4,368 µs | 3,320 µs |
| IndexNestedLoopJoin | `DISTINCT` ids | 4,000 | 4,000 | 4,000 | 4,000 | 4,000 | 512,000 | 0 | 3,589 µs | 3,511 µs |
| IndexNestedLoopJoin | skewed `DISTINCT name` | 40 | 4,000 | 4,000 | 8 | 8 | 776 | 0 | 1,586 µs | 538 µs |
| HashJoin | plain `name` | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 570,000 | 0 | 1,406 µs | 1,299 µs |
| HashJoin | `DISTINCT name` | 10,000 | 10,000 | 10,000 | 8 | 8 | 776 | 0 | 1,511 µs | 1,435 µs |
| HashJoin | plain ids | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 880,000 | 0 | 1,213 µs | 1,195 µs |
| HashJoin | `DISTINCT` ids | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 1,280,000 | 0 | 2,752 µs | 2,598 µs |
| HashJoin | `DISTINCT amount` | 10,000 | 10,000 | 10,000 | 6 | 6 | 576 | 0 | 1,377 µs | 1,402 µs |
| HashJoin | `DISTINCT tag` | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 1,600,000 | 0 | 2,640 µs | 2,681 µs |
| HashJoin | `DISTINCT` order limit | 10,000 | 10,000 | 10,000 | 5 | 24 | 3,096 | 0 | 1,550 µs | 1,555 µs |
| HashJoin | duplicate plain `name` | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 570,000 | 0 | 870 µs | 852 µs |
| HashJoin | duplicate `DISTINCT name` | 10,000 | 10,000 | 10,000 | 8 | 8 | 776 | 0 | 1,120 µs | 1,088 µs |
| HashJoin | skewed `DISTINCT name` | 100 | 10,000 | 10,000 | 8 | 8 | 776 | 160,000 | 954 µs | 918 µs |
| IndexNestedLoopJoin | `DISTINCT name` | 10,000 | 10,000 | 10,000 | 8 | 8 | 776 | 0 | 10,679 µs | 8,573 µs |
| IndexNestedLoopJoin | `DISTINCT` ids | 10,000 | 10,000 | 10,000 | 10,000 | 10,000 | 1,280,000 | 0 | 9,574 µs | 9,535 µs |
| IndexNestedLoopJoin | skewed `DISTINCT name` | 100 | 10,000 | 10,000 | 8 | 8 | 776 | 0 | 3,126 µs | 1,063 µs |

At 10,000 by 10,000, warm hash plain `name` is 1,299 µs and warm `DISTINCT name` over 8 values is 1,435 µs. The distinct set is 776 bytes. Warm hash plain ids is 1,195 µs and warm `DISTINCT` ids over 10,000 pairs is 2,598 µs, with a 1,280,000-byte estimate. Warm `DISTINCT tag` over 10,000 strings, 640,000 text bytes, is 2,681 µs and the estimate is 1,600,000 bytes. Warm `DISTINCT amount` over 6 values, half of them NULL, is 1,402 µs. Warm `ORDER BY name, amount LIMIT 5` is 1,555 µs and still stores 24 pairs. The duplicate-key hash, ten distinct `user_id` values, is 852 µs warm for the plain name list and 1,088 µs warm for `DISTINCT name`. Forced nested loop of the 4,000 by 4,000 name query is 101,808 µs warm against 718 µs for the hash join of the same statement. Forced index nested loop of the 10,000 by 10,000 id query is 9,535 µs warm against 2,598 µs for hash. The planner still uses hash for that equal input. On the skewed 100 by 10,000 input the hash join is 918 µs warm and holds a 160,000-byte position buffer. The index probe the planner selects there is 1,063 µs warm and does not hold that buffer.

## Milestone 18 LEFT JOIN

Release profile `profile_milestone18`, re-run 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes and a row vector header is 24 bytes. The warm time is the median of the next five samples. The cold time is the first sample. A wrong row count fails the run. Checksums are the printed wrapping sums of the returned cells.

Every plan is `LeftNestedLoopJoin`. The loop compares each left row with each right row, so the examined count is `users * orders`. It is not an O(N) or O(N log N) join. `LIMIT` does not stop the comparisons. `matched` counts `ON` results that are TRUE. `null-extended` counts left rows with no TRUE `ON` result, before `WHERE`. A 10,000 by 10,000 run was not taken.

`matched` gives every order `user_id = id`, so a user matches only when an order has that same id. `none` sets every `user_id` to -1. `few` points every order at user 0. `duplicates` uses `user_id = id % 10`. `where` is the matched input with `amount > orders / 2`. `order_limit` is the matched input with `ORDER BY amount ASC LIMIT 5`. Bytes estimate the rows held before the result is returned: projected pairs for the unsorted shapes, and the full four-column joined rows for the sort. That estimate is not process RSS.

| Shape | Users | Orders | Examined | Matched | Null-extended | Returned | Bytes | Cold | Warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matched | 100 | 100 | 10,000 | 100 | 0 | 100 | 8,800 | 162 µs | 114 µs |
| none | 100 | 100 | 10,000 | 0 | 100 | 100 | 8,800 | 111 µs | 110 µs |
| few | 100 | 100 | 10,000 | 100 | 99 | 199 | 17,512 | 114 µs | 112 µs |
| duplicates | 100 | 100 | 10,000 | 100 | 90 | 190 | 16,720 | 111 µs | 111 µs |
| where | 100 | 100 | 10,000 | 100 | 0 | 49 | 4,312 | 125 µs | 111 µs |
| order limit | 100 | 100 | 10,000 | 100 | 0 | 5 | 15,200 | 139 µs | 112 µs |
| matched | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 88,000 | 8,537 µs | 6,817 µs |
| none | 1,000 | 1,000 | 1,000,000 | 0 | 1,000 | 1,000 | 88,000 | 6,389 µs | 6,490 µs |
| few | 1,000 | 1,000 | 1,000,000 | 1,000 | 999 | 1,999 | 175,912 | 6,525 µs | 6,513 µs |
| duplicates | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 175,120 | 6,437 µs | 6,517 µs |
| where | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 43,912 | 6,417 µs | 6,516 µs |
| order limit | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 5 | 152,000 | 6,406 µs | 6,449 µs |
| matched | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 352,000 | 101,294 µs | 100,684 µs |
| none | 4,000 | 4,000 | 16,000,000 | 0 | 4,000 | 4,000 | 352,000 | 100,705 µs | 100,338 µs |
| few | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,999 | 7,999 | 703,912 | 99,428 µs | 99,781 µs |
| duplicates | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 703,120 | 101,003 µs | 100,507 µs |
| where | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 175,912 | 100,776 µs | 100,624 µs |
| order limit | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 5 | 608,000 | 101,710 µs | 101,379 µs |
| matched | 100 | 4,000 | 400,000 | 100 | 0 | 100 | 8,800 | 2,623 µs | 2,562 µs |
| none | 100 | 4,000 | 400,000 | 0 | 100 | 100 | 8,800 | 2,568 µs | 2,525 µs |
| duplicates | 100 | 4,000 | 400,000 | 4,000 | 90 | 4,090 | 359,920 | 2,580 µs | 2,679 µs |
| where | 100 | 4,000 | 400,000 | 100 | 0 | 0 | 0 | 2,539 µs | 2,533 µs |
| order limit | 100 | 4,000 | 400,000 | 100 | 0 | 5 | 15,200 | 2,496 µs | 2,518 µs |
| matched | 4,000 | 100 | 400,000 | 100 | 3,900 | 4,000 | 352,000 | 2,606 µs | 2,693 µs |
| none | 4,000 | 100 | 400,000 | 0 | 4,000 | 4,000 | 352,000 | 2,662 µs | 2,622 µs |
| duplicates | 4,000 | 100 | 400,000 | 100 | 3,990 | 4,090 | 359,920 | 2,669 µs | 2,669 µs |
| where | 4,000 | 100 | 400,000 | 100 | 3,900 | 49 | 4,312 | 2,681 µs | 2,627 µs |
| order limit | 4,000 | 100 | 400,000 | 100 | 3,900 | 5 | 608,000 | 2,684 µs | 2,720 µs |
| matched | 1,000 | 4,000 | 4,000,000 | 1,000 | 0 | 1,000 | 88,000 | 25,560 µs | 25,146 µs |
| none | 1,000 | 4,000 | 4,000,000 | 0 | 1,000 | 1,000 | 88,000 | 25,077 µs | 25,278 µs |
| duplicates | 1,000 | 4,000 | 4,000,000 | 4,000 | 990 | 4,990 | 439,120 | 25,783 µs | 25,525 µs |
| where | 1,000 | 4,000 | 4,000,000 | 1,000 | 0 | 0 | 0 | 25,483 µs | 25,530 µs |
| order limit | 1,000 | 4,000 | 4,000,000 | 1,000 | 0 | 5 | 152,000 | 25,275 µs | 25,349 µs |
| matched | 4,000 | 1,000 | 4,000,000 | 1,000 | 3,000 | 4,000 | 352,000 | 25,218 µs | 25,663 µs |
| none | 4,000 | 1,000 | 4,000,000 | 0 | 4,000 | 4,000 | 352,000 | 25,700 µs | 25,561 µs |
| duplicates | 4,000 | 1,000 | 4,000,000 | 1,000 | 3,990 | 4,990 | 439,120 | 25,772 µs | 25,333 µs |
| where | 4,000 | 1,000 | 4,000,000 | 1,000 | 3,000 | 499 | 43,912 | 25,234 µs | 25,411 µs |
| order limit | 4,000 | 1,000 | 4,000,000 | 1,000 | 3,000 | 5 | 608,000 | 25,410 µs | 25,507 µs |

At 4,000 by 4,000 the six shapes take about 101 ms warm and still examine all 16,000,000 pairs. The 4,000,000-pair runs take about 25 ms warm, about four times the 1,000 by 1,000 matched run and about a quarter of the square 4,000 run. The 100 by 4,000 and 4,000 by 100 runs examine the same 400,000 pairs and take about 2.6 ms warm in either orientation. Copying the smaller input does not change that cost.

`where` on 100 by 4,000 returns 0 rows. Every user matched an order, those amounts are 0 through 99, and `amount > 2000` removes them. It does not add a null-extended row. `where` on 4,000 by 100 keeps 49 matches and drops the 3,900 null-extended rows. The 4,000 by 100 sort still holds all 4,000 joined rows, 608,000 bytes, and returns 5. The nested loop is the bottleneck. Neither a hash table nor an index is used.

## Milestone 19 hash LEFT JOIN

Release profile `profile_milestone19`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes and a row vector header is 24 bytes. The warm time is the median of the next five samples. The cold time is the first sample. A wrong row count or a checksum mismatch between the hash path and `LeftNestedLoopJoin` fails the run. Both paths produced the printed checksum.

The hash path builds the right table. Before that, it checks that every `ON` cell is NULL or the column's declared type. A mismatched cell uses `LeftNestedLoopJoin` for that execution, so a type error is not skipped. Schema-valid rows stay on the hash path. `examined` counts exact-key hits, including hits that the residual `ON` later rejects. It is not `users * orders`, and it is not a claim that the join is linear. The nested column is the same statement forced through `LeftNestedLoopJoin`, so that examined count is `users * orders`. `LIMIT` does not stop either scan. `matched` counts `ON` results that are TRUE. `null-extended` counts left rows with no TRUE `ON` result, before `WHERE`. Speedup is nested warm divided by hash warm.

`matched` gives every order `user_id = id`. `none` sets every `user_id` to -1. `few` points every order at user 0. `duplicates` uses `user_id = id % 10`. `residual` is the matched input with `ON u.id = o.user_id AND o.amount > orders / 2`. `where` puts that same threshold in `WHERE`. `order_limit` is the matched input with `ORDER BY amount ASC LIMIT 5`.

Hash bytes are `8 * positions + 64 * groups` on this machine: one word per build position, one group header (`usize` plus `Vec<usize>`), and one map entry (`u64` plus `Vec` of groups) per distinct non-NULL key. Colliding fingerprints share a map entry, so the map term is then an upper bound. The figure excludes the hash map's bucket array, allocator slack, the copied input, and process RSS. Text keys are borrowed from the row and are not copied into the table. Copy bytes are the smaller input, or the right table when the two sides have the same number of rows. These two-column rows are 88 bytes each. The hash path stores no pair-position buffer. Match status is one boolean. The null-extended right row is one reused vector. `ORDER BY` still materializes the joined rows before sorting, the same way the nested path does. On `order_limit` that buffer is `(matched + null-extended) * 152` bytes: 15,200; 152,000; 608,000; and 1,520,000 for 100, 1,000, 4,000, and 10,000 fully matched left rows, and the same 608,000 and 1,520,000 when the larger left side is 4,000 or 10,000. That estimate is not RSS. Rust's `HashMap` still aborts the process if allocation fails.

| Shape | Users | Orders | Hash examined | Nested examined | Matched | Null-extended | Returned | Hash bytes | Copy bytes | Hash cold | Hash warm | Nested cold | Nested warm | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 7,200 | 8,800 | 33 µs | 23 µs | 78 µs | 79 µs | 3.4× |
| few | 100 | 100 | 100 | 10,000 | 100 | 99 | 199 | 864 | 8,800 | 19 µs | 19 µs | 73 µs | 72 µs | 3.8× |
| none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 864 | 8,800 | 16 µs | 15 µs | 73 µs | 73 µs | 4.9× |
| duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 1,440 | 8,800 | 20 µs | 18 µs | 75 µs | 76 µs | 4.2× |
| residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 7,200 | 8,800 | 30 µs | 23 µs | 229 µs | 167 µs | 7.3× |
| where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 7,200 | 8,800 | 24 µs | 21 µs | 76 µs | 76 µs | 3.6× |
| order limit | 100 | 100 | 100 | 10,000 | 100 | 0 | 5 | 7,200 | 8,800 | 53 µs | 24 µs | 79 µs | 79 µs | 3.3× |
| matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 72,000 | 88,000 | 137 µs | 127 µs | 6,458 µs | 6,435 µs | 50.7× |
| few | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 999 | 1,999 | 8,064 | 88,000 | 110 µs | 103 µs | 6,139 µs | 6,130 µs | 59.5× |
| none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 8,064 | 88,000 | 84 µs | 81 µs | 6,529 µs | 6,717 µs | 82.9× |
| duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 8,640 | 88,000 | 119 µs | 112 µs | 6,873 µs | 6,424 µs | 57.4× |
| residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 72,000 | 88,000 | 141 µs | 138 µs | 14,480 µs | 14,184 µs | 102.8× |
| where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 72,000 | 88,000 | 130 µs | 127 µs | 7,077 µs | 6,777 µs | 53.4× |
| order limit | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 5 | 72,000 | 88,000 | 167 µs | 150 µs | 6,492 µs | 6,466 µs | 43.1× |
| matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 288,000 | 352,000 | 526 µs | 489 µs | 101,673 µs | 104,518 µs | 213.7× |
| few | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,999 | 7,999 | 32,064 | 352,000 | 439 µs | 406 µs | 102,663 µs | 102,419 µs | 252.3× |
| none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 32,064 | 352,000 | 319 µs | 301 µs | 105,941 µs | 103,993 µs | 345.5× |
| duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 32,640 | 352,000 | 496 µs | 434 µs | 100,894 µs | 107,314 µs | 247.3× |
| residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 288,000 | 352,000 | 546 µs | 538 µs | 230,594 µs | 231,784 µs | 430.8× |
| where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 288,000 | 352,000 | 508 µs | 490 µs | 105,201 µs | 102,395 µs | 209.0× |
| order limit | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 5 | 288,000 | 352,000 | 611 µs | 574 µs | 101,435 µs | 102,736 µs | 179.0× |
| matched | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 10,000 | 720,000 | 880,000 | 1,371 µs | 1,222 µs | 633,526 µs | 656,625 µs | 537.3× |
| few | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 9,999 | 19,999 | 80,064 | 880,000 | 1,033 µs | 955 µs | 651,224 µs | 637,961 µs | 668.0× |
| none | 10,000 | 10,000 | 0 | 100,000,000 | 0 | 10,000 | 10,000 | 80,064 | 880,000 | 720 µs | 724 µs | 652,621 µs | 643,457 µs | 888.8× |
| duplicates | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 9,990 | 19,990 | 80,640 | 880,000 | 1,110 µs | 1,042 µs | 632,001 µs | 638,652 µs | 612.9× |
| residual | 10,000 | 10,000 | 10,000 | 100,000,000 | 4,999 | 5,001 | 10,000 | 720,000 | 880,000 | 1,458 µs | 1,334 µs | 1,393,199 µs | 1,390,726 µs | 1,042.5× |
| where | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 4,999 | 720,000 | 880,000 | 1,286 µs | 1,206 µs | 654,363 µs | 637,130 µs | 528.3× |
| order limit | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 5 | 720,000 | 880,000 | 1,959 µs | 1,469 µs | 631,098 µs | 652,332 µs | 444.1× |
| matched | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 100 | 288,000 | 8,800 | 357 µs | 396 µs | 3,267 µs | 3,073 µs | 7.8× |
| none | 100 | 4,000 | 0 | 400,000 | 0 | 100 | 100 | 32,064 | 8,800 | 148 µs | 110 µs | 2,640 µs | 2,575 µs | 23.4× |
| duplicates | 100 | 4,000 | 4,000 | 400,000 | 4,000 | 90 | 4,090 | 32,640 | 8,800 | 256 µs | 228 µs | 2,741 µs | 2,709 µs | 11.9× |
| where | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 0 | 288,000 | 8,800 | 235 µs | 229 µs | 2,590 µs | 2,576 µs | 11.2× |
| order limit | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 5 | 288,000 | 8,800 | 245 µs | 233 µs | 2,550 µs | 2,576 µs | 11.1× |
| matched | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 4,000 | 7,200 | 8,800 | 188 µs | 203 µs | 2,613 µs | 2,714 µs | 13.4× |
| none | 4,000 | 100 | 0 | 400,000 | 0 | 4,000 | 4,000 | 864 | 8,800 | 147 µs | 140 µs | 2,698 µs | 2,687 µs | 19.2× |
| duplicates | 4,000 | 100 | 100 | 400,000 | 100 | 3,990 | 4,090 | 1,440 | 8,800 | 151 µs | 148 µs | 2,710 µs | 2,682 µs | 18.1× |
| where | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 49 | 7,200 | 8,800 | 136 µs | 143 µs | 2,699 µs | 2,622 µs | 18.3× |
| order limit | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 5 | 7,200 | 8,800 | 280 µs | 277 µs | 2,770 µs | 2,775 µs | 10.0× |
| matched | 1,000 | 10,000 | 1,000 | 10,000,000 | 1,000 | 0 | 1,000 | 720,000 | 88,000 | 653 µs | 640 µs | 63,641 µs | 66,058 µs | 103.2× |
| none | 1,000 | 10,000 | 0 | 10,000,000 | 0 | 1,000 | 1,000 | 80,064 | 88,000 | 284 µs | 243 µs | 65,704 µs | 66,530 µs | 273.8× |
| duplicates | 1,000 | 10,000 | 10,000 | 10,000,000 | 10,000 | 990 | 10,990 | 80,640 | 88,000 | 673 µs | 593 µs | 65,520 µs | 66,994 µs | 113.0× |
| where | 1,000 | 10,000 | 1,000 | 10,000,000 | 1,000 | 0 | 0 | 720,000 | 88,000 | 746 µs | 672 µs | 65,255 µs | 65,385 µs | 97.3× |
| order limit | 1,000 | 10,000 | 1,000 | 10,000,000 | 1,000 | 0 | 5 | 720,000 | 88,000 | 772 µs | 671 µs | 63,525 µs | 66,091 µs | 98.5× |
| matched | 10,000 | 1,000 | 1,000 | 10,000,000 | 1,000 | 9,000 | 10,000 | 72,000 | 88,000 | 446 µs | 438 µs | 63,204 µs | 66,560 µs | 152.0× |
| none | 10,000 | 1,000 | 0 | 10,000,000 | 0 | 10,000 | 10,000 | 8,064 | 88,000 | 381 µs | 375 µs | 63,012 µs | 62,890 µs | 167.7× |
| duplicates | 10,000 | 1,000 | 1,000 | 10,000,000 | 1,000 | 9,990 | 10,990 | 8,640 | 88,000 | 460 µs | 406 µs | 63,157 µs | 63,317 µs | 156.0× |
| where | 10,000 | 1,000 | 1,000 | 10,000,000 | 1,000 | 9,000 | 499 | 72,000 | 88,000 | 357 µs | 345 µs | 63,726 µs | 64,405 µs | 186.7× |
| order limit | 10,000 | 1,000 | 1,000 | 10,000,000 | 1,000 | 9,000 | 5 | 72,000 | 88,000 | 683 µs | 628 µs | 63,281 µs | 65,892 µs | 104.9× |

At 10,000 by 10,000 the matched hash join is 1,222 µs warm and examines 10,000 key hits. The nested loop of the same statement is 656,625 µs warm and examines 100,000,000 pairs, 537.3× slower. The unmatched shape is 888.8× because the hash probe finds no key and still emits one null-extended row per left row. The residual shape is 1,042.5×: the hash still examines 10,000 candidates, and the nested loop evaluates the extra comparison on every pair. A duplicate key that matches every left row still walks that whole bucket, so this is not a linear worst case under a forced collision or extreme multiplicity. Hashing the larger right side is the cost of keeping left order without a pair buffer. On 100 by 4,000 matched, the hash table is 288,000 bytes for 4,000 right keys while the copied input is only the 100 left rows, 8,800 bytes, and the speedup is 7.8×. On 4,000 by 100 the same 400,000 nested pairs become a 7,200-byte hash of 100 right keys and the speedup is 13.4×. `where` on 100 by 4,000 and 1,000 by 10,000 returns 0 rows: every left row had an `ON` match, and `WHERE` removed those pairs without adding a null-extended row. Checksums for the square matched runs are 5,050; 500,500; 8,002,000; and 50,005,000. The 100 by 4,000 `where` checksum is 0. The 4,000 by 100 `where` checksum is 3,724.

## Milestone 20 aggregation over LEFT JOIN

Release profile `profile_milestone20`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes, one aggregate accumulator is 48 bytes, and a row vector header is 24 bytes. The warm time is the median of the next five samples. The cold time is the first sample. A checksum mismatch between `HashLeftJoin` and the same statement forced through `LeftNestedLoopJoin` fails the run. Both paths produced the printed checksum.

`plain` is the join without an aggregate. `count_star` is `COUNT(*)`. `count_amount` is `COUNT(o.amount)`. `group` is `GROUP BY u.name` with `COUNT(*)`. User names are eight repeating letters, so that query has at most eight groups. `multi` is `SUM`, `AVG`, `MIN`, and `MAX` of `amount` with no `GROUP BY`. `matched` sets `user_id = id` and `amount = id`. `none` sets every `user_id` to -1. `duplicates` uses `user_id = id % 10`. `nulls` is the matched key with a NULL amount. `where` puts `amount > orders / 2` in `WHERE`. `residual` puts that comparison in `ON`. The 10,000 by 10,000 `plain` join is not in this run. The four aggregate forms are.

`examined` is the hash path's exact-key hits, including hits a residual `ON` later rejects. It is not `users * orders`. The nested column examines every pair. `matched` counts `ON` results that are TRUE. `null-extended` counts left rows with no TRUE `ON` result, before `WHERE`. `inputs` is the logical rows that reach the aggregate, or the returned rows for `plain`. `groups` is the result row count. A scalar aggregate with no surviving input still has one group. `GROUP BY` over no surviving input has zero. Aggregate-state bytes are `groups * (24 + 32 + aggregates * 48)`: one vector header, one key cell, and one 48-byte accumulator per aggregate. A scalar query has no group key, so that 32-byte term is an upper bound. The figure excludes the hash table, the copied input, allocator slack, and process RSS. The joined pairs are not stored. Speedup is nested warm divided by hash warm.

| Kind | Shape | Users | Orders | Hash examined | Nested examined | Matched | Null-extended | Inputs | Groups | Acc bytes | Hash cold | Hash warm | Nested cold | Nested warm | Speedup | Checksum |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| plain | matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 100 | 0 | 51 µs | 32 µs | 111 µs | 109 µs | 3.4× | 5,050 |
| count_star | matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 104 | 46 µs | 28 µs | 107 µs | 106 µs | 3.8× | 100 |
| count_amount | matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 104 | 30 µs | 29 µs | 109 µs | 107 µs | 3.7× | 100 |
| group | matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 8 | 832 | 49 µs | 34 µs | 106 µs | 107 µs | 3.1× | 108 |
| multi | matched | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 248 | 46 µs | 37 µs | 113 µs | 113 µs | 3.1× | 5,150 |
| plain | none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 100 | 0 | 23 µs | 20 µs | 100 µs | 99 µs | 5.0× | 100 |
| count_star | none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 1 | 104 | 21 µs | 20 µs | 99 µs | 99 µs | 5.0× | 100 |
| count_amount | none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 1 | 104 | 19 µs | 18 µs | 96 µs | 96 µs | 5.3× | 0 |
| group | none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 8 | 832 | 30 µs | 26 µs | 102 µs | 101 µs | 3.9× | 108 |
| multi | none | 100 | 100 | 0 | 10,000 | 0 | 100 | 100 | 1 | 248 | 28 µs | 25 µs | 100 µs | 98 µs | 3.9× | 0 |
| plain | duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 190 | 0 | 25 µs | 23 µs | 94 µs | 94 µs | 4.1× | 5,140 |
| count_star | duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 1 | 104 | 24 µs | 23 µs | 95 µs | 96 µs | 4.2× | 190 |
| count_amount | duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 1 | 104 | 23 µs | 22 µs | 93 µs | 93 µs | 4.2× | 100 |
| group | duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 8 | 832 | 34 µs | 33 µs | 106 µs | 105 µs | 3.2× | 198 |
| multi | duplicates | 100 | 100 | 100 | 10,000 | 100 | 90 | 190 | 1 | 248 | 33 µs | 32 µs | 103 µs | 103 µs | 3.2× | 5,150 |
| plain | nulls | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 100 | 0 | 25 µs | 24 µs | 93 µs | 93 µs | 3.9× | 100 |
| count_star | nulls | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 104 | 25 µs | 22 µs | 87 µs | 87 µs | 4.0× | 100 |
| count_amount | nulls | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 104 | 22 µs | 21 µs | 84 µs | 84 µs | 4.0× | 0 |
| group | nulls | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 8 | 832 | 30 µs | 30 µs | 95 µs | 95 µs | 3.2× | 108 |
| multi | nulls | 100 | 100 | 100 | 10,000 | 100 | 0 | 100 | 1 | 248 | 30 µs | 29 µs | 91 µs | 91 µs | 3.1× | 0 |
| plain | where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 49 | 0 | 26 µs | 24 µs | 89 µs | 89 µs | 3.7× | 3,724 |
| count_star | where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 1 | 104 | 25 µs | 24 µs | 88 µs | 88 µs | 3.7× | 49 |
| count_amount | where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 1 | 104 | 25 µs | 24 µs | 87 µs | 85 µs | 3.5× | 49 |
| group | where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 8 | 832 | 29 µs | 28 µs | 89 µs | 89 µs | 3.2× | 57 |
| multi | where | 100 | 100 | 100 | 10,000 | 100 | 0 | 49 | 1 | 248 | 33 µs | 31 µs | 92 µs | 91 µs | 2.9× | 3,901 |
| plain | residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 100 | 0 | 27 µs | 24 µs | 166 µs | 167 µs | 7.0× | 3,775 |
| count_star | residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 1 | 104 | 25 µs | 24 µs | 165 µs | 166 µs | 6.9× | 100 |
| count_amount | residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 1 | 104 | 25 µs | 24 µs | 166 µs | 165 µs | 6.9× | 49 |
| group | residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 8 | 832 | 43 µs | 31 µs | 173 µs | 173 µs | 5.6× | 108 |
| multi | residual | 100 | 100 | 100 | 10,000 | 49 | 51 | 100 | 1 | 248 | 33 µs | 32 µs | 172 µs | 169 µs | 5.3× | 3,901 |
| plain | matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1,000 | 0 | 148 µs | 135 µs | 6,702 µs | 6,355 µs | 47.1× | 500,500 |
| count_star | matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 104 | 142 µs | 135 µs | 6,348 µs | 6,286 µs | 46.6× | 1,000 |
| count_amount | matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 104 | 141 µs | 135 µs | 6,405 µs | 6,436 µs | 47.7× | 1,000 |
| group | matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 8 | 832 | 173 µs | 163 µs | 6,345 µs | 6,334 µs | 38.9× | 1,008 |
| multi | matched | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 248 | 160 µs | 152 µs | 6,262 µs | 6,313 µs | 41.5× | 501,500 |
| plain | none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 1,000 | 0 | 84 µs | 78 µs | 6,213 µs | 6,278 µs | 80.5× | 1,000 |
| count_star | none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 1 | 104 | 85 µs | 82 µs | 6,198 µs | 6,248 µs | 76.2× | 1,000 |
| count_amount | none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 1 | 104 | 65 µs | 64 µs | 6,202 µs | 6,231 µs | 97.4× | 0 |
| group | none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 8 | 832 | 115 µs | 108 µs | 6,454 µs | 6,357 µs | 58.9× | 1,008 |
| multi | none | 1,000 | 1,000 | 0 | 1,000,000 | 0 | 1,000 | 1,000 | 1 | 248 | 87 µs | 80 µs | 6,206 µs | 6,210 µs | 77.6× | 0 |
| plain | duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 1,990 | 0 | 123 µs | 110 µs | 6,271 µs | 6,343 µs | 57.7× | 501,490 |
| count_star | duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 1 | 104 | 118 µs | 114 µs | 6,237 µs | 6,358 µs | 55.8× | 1,990 |
| count_amount | duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 1 | 104 | 103 µs | 99 µs | 6,396 µs | 6,275 µs | 63.4× | 1,000 |
| group | duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 8 | 832 | 169 µs | 166 µs | 6,394 µs | 6,334 µs | 38.2× | 1,998 |
| multi | duplicates | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 990 | 1,990 | 1 | 248 | 127 µs | 123 µs | 6,466 µs | 6,304 µs | 51.3× | 501,500 |
| plain | nulls | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1,000 | 0 | 133 µs | 126 µs | 6,457 µs | 6,386 µs | 50.7× | 1,000 |
| count_star | nulls | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 104 | 138 µs | 134 µs | 6,461 µs | 6,355 µs | 47.4× | 1,000 |
| count_amount | nulls | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 104 | 119 µs | 116 µs | 6,248 µs | 6,350 µs | 54.7× | 0 |
| group | nulls | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 8 | 832 | 171 µs | 165 µs | 6,430 µs | 6,473 µs | 39.2× | 1,008 |
| multi | nulls | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 1,000 | 1 | 248 | 133 µs | 129 µs | 6,386 µs | 6,362 µs | 49.3× | 0 |
| plain | where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 499 | 0 | 132 µs | 127 µs | 6,395 µs | 6,409 µs | 50.5× | 374,749 |
| count_star | where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 1 | 104 | 136 µs | 129 µs | 6,311 µs | 6,421 µs | 49.8× | 499 |
| count_amount | where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 1 | 104 | 133 µs | 133 µs | 6,264 µs | 6,404 µs | 48.2× | 499 |
| group | where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 8 | 832 | 151 µs | 148 µs | 6,263 µs | 6,298 µs | 42.6× | 507 |
| multi | where | 1,000 | 1,000 | 1,000 | 1,000,000 | 1,000 | 0 | 499 | 1 | 248 | 145 µs | 143 µs | 6,285 µs | 6,454 µs | 45.1× | 376,501 |
| plain | residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 1,000 | 0 | 141 µs | 135 µs | 14,778 µs | 14,542 µs | 107.7× | 375,250 |
| count_star | residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 1 | 104 | 149 µs | 143 µs | 15,016 µs | 14,473 µs | 101.2× | 1,000 |
| count_amount | residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 1 | 104 | 143 µs | 136 µs | 14,440 µs | 14,675 µs | 107.9× | 499 |
| group | residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 8 | 832 | 181 µs | 176 µs | 14,418 µs | 14,690 µs | 83.5× | 1,008 |
| multi | residual | 1,000 | 1,000 | 1,000 | 1,000,000 | 499 | 501 | 1,000 | 1 | 248 | 160 µs | 151 µs | 15,079 µs | 14,823 µs | 98.2× | 376,501 |
| plain | matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 4,000 | 0 | 583 µs | 488 µs | 102,126 µs | 102,074 µs | 209.2× | 8,002,000 |
| count_star | matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 104 | 530 µs | 519 µs | 101,132 µs | 105,378 µs | 203.0× | 4,000 |
| count_amount | matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 104 | 535 µs | 529 µs | 105,534 µs | 110,331 µs | 208.6× | 4,000 |
| group | matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 8 | 832 | 644 µs | 634 µs | 104,717 µs | 109,448 µs | 172.6× | 4,008 |
| multi | matched | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 248 | 581 µs | 587 µs | 109,894 µs | 109,460 µs | 186.5× | 8,006,000 |
| plain | none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 4,000 | 0 | 515 µs | 332 µs | 141,173 µs | 112,498 µs | 338.8× | 4,000 |
| count_star | none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 1 | 104 | 329 µs | 376 µs | 107,863 µs | 109,187 µs | 290.4× | 4,000 |
| count_amount | none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 1 | 104 | 243 µs | 226 µs | 104,434 µs | 103,794 µs | 459.3× | 0 |
| group | none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 8 | 832 | 435 µs | 397 µs | 101,505 µs | 101,427 µs | 255.5× | 4,008 |
| multi | none | 4,000 | 4,000 | 0 | 16,000,000 | 0 | 4,000 | 4,000 | 1 | 248 | 268 µs | 258 µs | 102,031 µs | 110,488 µs | 428.2× | 0 |
| plain | duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 7,990 | 0 | 974 µs | 421 µs | 109,381 µs | 109,853 µs | 260.9× | 8,005,990 |
| count_star | duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 1 | 104 | 493 µs | 435 µs | 104,053 µs | 109,156 µs | 250.9× | 7,990 |
| count_amount | duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 1 | 104 | 408 µs | 416 µs | 105,879 µs | 109,330 µs | 262.8× | 4,000 |
| group | duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 8 | 832 | 772 µs | 654 µs | 112,128 µs | 111,976 µs | 171.2× | 7,998 |
| multi | duplicates | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 3,990 | 7,990 | 1 | 248 | 545 µs | 575 µs | 120,298 µs | 110,316 µs | 191.9× | 8,006,000 |
| plain | nulls | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 4,000 | 0 | 557 µs | 502 µs | 103,585 µs | 105,023 µs | 209.2× | 4,000 |
| count_star | nulls | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 104 | 526 µs | 516 µs | 102,359 µs | 105,137 µs | 203.8× | 4,000 |
| count_amount | nulls | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 104 | 459 µs | 447 µs | 103,619 µs | 104,354 µs | 233.5× | 0 |
| group | nulls | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 8 | 832 | 647 µs | 619 µs | 106,583 µs | 112,148 µs | 181.2× | 4,008 |
| multi | nulls | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 4,000 | 1 | 248 | 586 µs | 493 µs | 113,084 µs | 106,260 µs | 215.5× | 0 |
| plain | where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 1,999 | 0 | 559 µs | 510 µs | 108,150 µs | 104,061 µs | 204.0× | 5,998,999 |
| count_star | where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 1 | 104 | 525 µs | 525 µs | 104,766 µs | 105,523 µs | 201.0× | 1,999 |
| count_amount | where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 1 | 104 | 781 µs | 526 µs | 106,256 µs | 106,128 µs | 201.8× | 1,999 |
| group | where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 8 | 832 | 648 µs | 580 µs | 109,973 µs | 112,408 µs | 193.8× | 2,007 |
| multi | where | 4,000 | 4,000 | 4,000 | 16,000,000 | 4,000 | 0 | 1,999 | 1 | 248 | 569 µs | 569 µs | 126,939 µs | 111,726 µs | 196.4× | 6,006,001 |
| plain | residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 4,000 | 0 | 615 µs | 607 µs | 672,123 µs | 308,305 µs | 507.9× | 6,001,000 |
| count_star | residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 1 | 104 | 604 µs | 562 µs | 246,692 µs | 234,098 µs | 416.5× | 4,000 |
| count_amount | residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 1 | 104 | 591 µs | 544 µs | 243,313 µs | 239,512 µs | 440.3× | 1,999 |
| group | residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 8 | 832 | 692 µs | 649 µs | 233,063 µs | 229,442 µs | 353.5× | 4,008 |
| multi | residual | 4,000 | 4,000 | 4,000 | 16,000,000 | 1,999 | 2,001 | 4,000 | 1 | 248 | 606 µs | 565 µs | 229,568 µs | 231,596 µs | 409.9× | 6,006,001 |
| count_star | matched | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 10,000 | 1 | 104 | 1,462 µs | 1,308 µs | 640,773 µs | 653,345 µs | 499.5× | 10,000 |
| count_amount | matched | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 10,000 | 1 | 104 | 1,758 µs | 1,393 µs | 704,503 µs | 667,798 µs | 479.4× | 10,000 |
| group | matched | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 10,000 | 8 | 832 | 1,719 µs | 1,667 µs | 701,759 µs | 673,588 µs | 404.1× | 10,008 |
| multi | matched | 10,000 | 10,000 | 10,000 | 100,000,000 | 10,000 | 0 | 10,000 | 1 | 248 | 1,714 µs | 1,444 µs | 658,048 µs | 669,125 µs | 463.4× | 50,015,000 |
| count_star | none | 10,000 | 10,000 | 0 | 100,000,000 | 0 | 10,000 | 10,000 | 1 | 104 | 755 µs | 721 µs | 652,647 µs | 663,952 µs | 920.9× | 10,000 |
| count_amount | none | 10,000 | 10,000 | 0 | 100,000,000 | 0 | 10,000 | 10,000 | 1 | 104 | 563 µs | 548 µs | 678,312 µs | 663,659 µs | 1211.1× | 0 |
| group | none | 10,000 | 10,000 | 0 | 100,000,000 | 0 | 10,000 | 10,000 | 8 | 832 | 1,010 µs | 995 µs | 648,193 µs | 675,839 µs | 679.2× | 10,008 |
| multi | none | 10,000 | 10,000 | 0 | 100,000,000 | 0 | 10,000 | 10,000 | 1 | 248 | 890 µs | 620 µs | 650,953 µs | 671,627 µs | 1083.3× | 0 |
| count_star | matched | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 100 | 1 | 104 | 261 µs | 226 µs | 2,634 µs | 2,588 µs | 11.5× | 100 |
| group | matched | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 100 | 8 | 832 | 235 µs | 238 µs | 2,544 µs | 2,531 µs | 10.6× | 108 |
| multi | matched | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 100 | 1 | 248 | 239 µs | 229 µs | 2,460 µs | 2,499 µs | 10.9× | 5,150 |
| count_star | none | 100 | 4,000 | 0 | 400,000 | 0 | 100 | 100 | 1 | 104 | 96 µs | 87 µs | 2,545 µs | 2,533 µs | 29.1× | 100 |
| group | none | 100 | 4,000 | 0 | 400,000 | 0 | 100 | 100 | 8 | 832 | 99 µs | 93 µs | 2,543 µs | 2,575 µs | 27.7× | 108 |
| multi | none | 100 | 4,000 | 0 | 400,000 | 0 | 100 | 100 | 1 | 248 | 100 µs | 90 µs | 2,508 µs | 2,539 µs | 28.2× | 0 |
| count_star | duplicates | 100 | 4,000 | 4,000 | 400,000 | 4,000 | 90 | 4,090 | 1 | 104 | 253 µs | 224 µs | 2,669 µs | 2,635 µs | 11.8× | 4,090 |
| group | duplicates | 100 | 4,000 | 4,000 | 400,000 | 4,000 | 90 | 4,090 | 8 | 832 | 318 µs | 313 µs | 2,772 µs | 2,710 µs | 8.7× | 4,098 |
| multi | duplicates | 100 | 4,000 | 4,000 | 400,000 | 4,000 | 90 | 4,090 | 1 | 248 | 277 µs | 272 µs | 2,688 µs | 2,690 µs | 9.9× | 8,006,000 |
| count_star | where | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 0 | 1 | 104 | 225 µs | 219 µs | 2,540 µs | 2,529 µs | 11.5× | 0 |
| group | where | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 0 | 0 | 0 | 239 µs | 221 µs | 2,490 µs | 2,623 µs | 11.9× | 0 |
| multi | where | 100 | 4,000 | 100 | 400,000 | 100 | 0 | 0 | 1 | 248 | 263 µs | 238 µs | 2,701 µs | 2,574 µs | 10.8× | 0 |
| count_star | matched | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 4,000 | 1 | 104 | 201 µs | 193 µs | 2,614 µs | 2,884 µs | 14.9× | 4,000 |
| group | matched | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 4,000 | 8 | 832 | 303 µs | 315 µs | 3,257 µs | 2,852 µs | 9.1× | 4,008 |
| multi | matched | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 4,000 | 1 | 248 | 156 µs | 160 µs | 2,968 µs | 2,696 µs | 16.9× | 5,150 |
| count_star | none | 4,000 | 100 | 0 | 400,000 | 0 | 4,000 | 4,000 | 1 | 104 | 170 µs | 155 µs | 2,637 µs | 2,911 µs | 18.8× | 4,000 |
| group | none | 4,000 | 100 | 0 | 400,000 | 0 | 4,000 | 4,000 | 8 | 832 | 271 µs | 281 µs | 2,832 µs | 2,900 µs | 10.3× | 4,008 |
| multi | none | 4,000 | 100 | 0 | 400,000 | 0 | 4,000 | 4,000 | 1 | 248 | 127 µs | 122 µs | 2,965 µs | 2,685 µs | 22.0× | 0 |
| count_star | duplicates | 4,000 | 100 | 100 | 400,000 | 100 | 3,990 | 4,090 | 1 | 104 | 169 µs | 163 µs | 2,732 µs | 2,729 µs | 16.7× | 4,090 |
| group | duplicates | 4,000 | 100 | 100 | 400,000 | 100 | 3,990 | 4,090 | 8 | 832 | 274 µs | 269 µs | 2,992 µs | 2,945 µs | 10.9× | 4,098 |
| multi | duplicates | 4,000 | 100 | 100 | 400,000 | 100 | 3,990 | 4,090 | 1 | 248 | 134 µs | 130 µs | 2,806 µs | 2,848 µs | 21.9× | 5,150 |
| count_star | where | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 49 | 1 | 104 | 146 µs | 175 µs | 2,895 µs | 2,606 µs | 14.9× | 49 |
| group | where | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 49 | 8 | 832 | 139 µs | 152 µs | 2,608 µs | 2,600 µs | 17.1× | 57 |
| multi | where | 4,000 | 100 | 100 | 400,000 | 100 | 3,900 | 49 | 1 | 248 | 160 µs | 160 µs | 2,805 µs | 3,015 µs | 18.8× | 3,901 |

At 10,000 by 10,000, matched `COUNT(*)` is 1,308 µs warm on the hash path and 653,345 µs warm on the nested loop, 499.5×. It examines 10,000 key hits against 100,000,000 pairs and the checksum is 10,000. `COUNT(o.amount)` is 479.4×, `GROUP BY` name is 404.1×, and the four-aggregate query is 463.4×. The unmatched `COUNT(*)` is 920.9×: the hash probe finds no key, and each of the 10,000 null-extended rows still increments `COUNT(*)`. `COUNT(o.amount)` on that input checksums to 0. Square matched `COUNT(*)` speedups are 3.8×, 46.6×, 203.0×, and 499.5× at 100, 1,000, 4,000, and 10,000. The nested loop still examines every pair. A duplicate key that matches every left row still walks that bucket, so this is not a linear worst case.

`where` on 100 by 4,000 feeds 0 logical rows. `COUNT(*)` returns one row whose checksum is 0, and `GROUP BY` returns no groups. Every user had an `ON` match, and `WHERE` removed those pairs without adding a null-extended row. `residual` on 4,000 by 4,000 keeps 1,999 matches and 2,001 null-extended rows, 4,000 aggregate inputs, and `COUNT(*)` is 416.5×. The hash table is unchanged from Milestone 19: the right table is the build side, and aggregation does not add a pair buffer. Checksums for square matched `COUNT(*)` are 100, 1,000, 4,000, and 10,000. Square matched `SUM`/`AVG`/`MIN`/`MAX` checksums are 5,150; 501,500; 8,006,000; and 50,015,000.

## Milestone 21 INNER JOIN schema check

Release profile `profile_milestone21`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes and a row vector header is 24 bytes. The statement is `SELECT u.id, o.amount FROM users u JOIN orders o ON u.id = o.user_id`. User ids are `0..users`. An order id `n` stores `user_id = n % user_mod` and `amount = NULL` when `n` is even, otherwise `n % 5`. `equal` uses `user_mod = users`. `duplicates` uses `user_mod = 10`. `build_outer` uses `users = rows / 100` and `orders = rows`, so the hash table is built from the outer table. `build_inner` uses `orders = rows / 100`. The index rows are the same data with `CREATE INDEX` on `orders(user_id)` and the test forcing the index probe.

`check` is the production path, which walks each input once for row width and then once per `ON` column before hashing or probing. `skip` is the same binary with that walk disabled. Both returned the same checksum and the same row count. The checksum is an independent sum of the matched user id and the non-NULL amount. It is not taken from the hash table. Square `equal` checksums are 5,050; 500,500; 8,002,000; and 50,005,000. `hits` is that match count. `hash_bytes` is the existing position-and-group estimate for the hash table. It is not process RSS, and the schema walk does not add a pair buffer. The walk allocates the column-index list and the declared-type vectors. Overhead is `check` warm minus `skip` warm. A negative number means the checked sample was faster. That is ordinary timer noise, not a speedup from the extra walk.

| Strategy | Shape | Users | Orders | Hits | Hash bytes | Check cold | Check warm | Skip cold | Skip warm | Overhead |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HashJoin | equal | 100 | 100 | 100 | 7,200 | 52 µs | 31 µs | 28 µs | 29 µs | 2 µs |
| HashJoin | duplicates | 100 | 100 | 100 | 1,440 | 25 µs | 24 µs | 22 µs | 22 µs | 2 µs |
| HashJoin | build_outer | 1 | 100 | 100 | 72 | 31 µs | 16 µs | 16 µs | 17 µs | -1 µs |
| HashJoin | build_inner | 100 | 1 | 1 | 72 | 13 µs | 12 µs | 11 µs | 11 µs | 1 µs |
| IndexNestedLoopJoin | equal | 100 | 100 | 100 | 0 | 226 µs | 117 µs | 110 µs | 111 µs | 6 µs |
| IndexNestedLoopJoin | duplicates | 100 | 100 | 100 | 0 | 225 µs | 143 µs | 146 µs | 142 µs | 1 µs |
| HashJoin | equal | 1,000 | 1,000 | 1,000 | 72,000 | 271 µs | 241 µs | 237 µs | 235 µs | 6 µs |
| HashJoin | duplicates | 1,000 | 1,000 | 1,000 | 8,640 | 167 µs | 151 µs | 129 µs | 131 µs | 20 µs |
| HashJoin | build_outer | 10 | 1,000 | 1,000 | 720 | 127 µs | 95 µs | 93 µs | 110 µs | -15 µs |
| HashJoin | build_inner | 1,000 | 10 | 10 | 720 | 35 µs | 34 µs | 32 µs | 32 µs | 2 µs |
| IndexNestedLoopJoin | equal | 1,000 | 1,000 | 1,000 | 0 | 1,624 µs | 1,232 µs | 1,129 µs | 992 µs | 240 µs |
| IndexNestedLoopJoin | duplicates | 1,000 | 1,000 | 1,000 | 0 | 1,914 µs | 1,442 µs | 1,419 µs | 1,268 µs | 174 µs |
| HashJoin | equal | 4,000 | 4,000 | 4,000 | 288,000 | 625 µs | 559 µs | 660 µs | 529 µs | 30 µs |
| HashJoin | duplicates | 4,000 | 4,000 | 4,000 | 32,640 | 413 µs | 362 µs | 336 µs | 328 µs | 34 µs |
| HashJoin | build_outer | 40 | 4,000 | 4,000 | 2,880 | 289 µs | 242 µs | 243 µs | 242 µs | 0 µs |
| HashJoin | build_inner | 4,000 | 40 | 40 | 2,880 | 92 µs | 90 µs | 81 µs | 85 µs | 5 µs |
| IndexNestedLoopJoin | equal | 4,000 | 4,000 | 4,000 | 0 | 4,205 µs | 3,160 µs | 2,920 µs | 2,912 µs | 248 µs |
| IndexNestedLoopJoin | duplicates | 4,000 | 4,000 | 4,000 | 0 | 5,500 µs | 4,529 µs | 4,227 µs | 4,294 µs | 235 µs |
| HashJoin | equal | 10,000 | 10,000 | 10,000 | 720,000 | 1,304 µs | 1,201 µs | 1,156 µs | 1,171 µs | 30 µs |
| HashJoin | duplicates | 10,000 | 10,000 | 10,000 | 80,640 | 842 µs | 778 µs | 748 µs | 751 µs | 27 µs |
| HashJoin | build_outer | 100 | 10,000 | 10,000 | 7,200 | 621 µs | 579 µs | 564 µs | 551 µs | 28 µs |
| HashJoin | build_inner | 10,000 | 100 | 100 | 7,200 | 268 µs | 243 µs | 260 µs | 234 µs | 9 µs |
| IndexNestedLoopJoin | equal | 10,000 | 10,000 | 10,000 | 0 | 9,994 µs | 7,837 µs | 7,817 µs | 7,835 µs | 2 µs |
| IndexNestedLoopJoin | duplicates | 10,000 | 10,000 | 10,000 | 0 | 13,558 µs | 11,551 µs | 11,470 µs | 11,418 µs | 133 µs |

On the 10,000 by 10,000 hash join the schema walk added 30 µs to a 1,201 µs warm run. The same shape's index probe differed by 2 µs. Several smaller shapes were faster with the walk enabled, including -15 µs on the 10 by 1,000 hash join. The 240 µs and 248 µs index gaps at 1,000 and 4,000 do not continue at 10,000, so they are not a stable cost of the walk. The hash table size is unchanged from the earlier estimate. Valid rows stay on the planned path.

## Milestone 22 DISTINCT aggregates over LEFT JOIN

Release profile `profile_milestone22`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes, `AccValue` is 48 bytes, and `HashMap<u64, Vec<Value>>` is 48 bytes. Both queries run on the same inserted tables. The ordinary query is `COUNT(*)`, `COUNT(o.amount)`, `SUM(o.amount)`, `AVG(o.amount)`. The distinct query is `COUNT(DISTINCT o.amount)`, `SUM(DISTINCT o.amount)`, `AVG(DISTINCT o.amount)`. User ids are `0..rows` and names are one letter. An order's `user_id` and `amount` depend on the shape: `matched` uses `id % rows` and `id % 97`, with NULL amounts when `id % 11 == 0`; `few` uses amounts `id % 3`; `duplicates` uses `user_id = id % 10` and amounts `id % 5`, with NULL when `id % 4 == 0`; `none` stores `user_id = -1`; `nulls` matches every user and stores NULL amounts. There is no `GROUP BY`, so the result is one scalar group. `hits` counts orders whose `user_id` is in `0..rows`. `null-extended` counts left rows with no such order. `inputs` is `hits + null-extended`. The nested loop examines `users × orders` pairs. The hash path builds the right table; this profile does not count its probes. Nested times are omitted where the run was not performed. Checksums are independent sums of the result cells, including a locally reduced average. They matched the hash path and, where it ran, the nested path.

`ordinary_acc_bytes` is four accumulator values. `distinct_bytes` is three accumulator values, three stored copies of each unique amount, and three map headers. It is not process RSS and does not include hash-bucket growth. A `HashMap` that cannot grow aborts the process.

| Shape | Rows | Hits | Null-extended | Inputs | Uniques | Ordinary acc | Distinct bytes | Ord hash cold | Ord hash warm | Dist hash cold | Dist hash warm | Ord nested cold | Ord nested warm | Dist nested cold | Dist nested warm | Ord checksum | Dist checksum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matched | 100 | 100 | 0 | 100 | 89 | 192 | 8,832 | 127 µs | 54 µs | 112 µs | 72 µs | 103 µs | 123 µs | 165 µs | 159 µs | 8,802 | 8,698 |
| few | 100 | 100 | 0 | 100 | 3 | 192 | 576 | 43 µs | 32 µs | 37 µs | 35 µs | 96 µs | 97 µs | 118 µs | 116 µs | 498 | 8 |
| duplicates | 100 | 100 | 90 | 190 | 5 | 192 | 768 | 34 µs | 30 µs | 32 µs | 31 µs | 98 µs | 97 µs | 98 µs | 98 µs | 418 | 18 |
| none | 100 | 0 | 100 | 100 | 0 | 192 | 288 | 28 µs | 26 µs | 22 µs | 22 µs | 93 µs | 92 µs | 90 µs | 90 µs | 100 | 0 |
| nulls | 100 | 100 | 0 | 100 | 0 | 192 | 288 | 31 µs | 29 µs | 28 µs | 27 µs | 95 µs | 95 µs | 108 µs | 111 µs | 100 | 0 |
| matched | 1,000 | 1,000 | 0 | 1,000 | 97 | 192 | 9,600 | 189 µs | 172 µs | 248 µs | 280 µs | 7,972 µs | 7,342 µs | 6,745 µs | 6,729 µs | 59,132 | 4,802 |
| few | 1,000 | 1,000 | 0 | 1,000 | 3 | 192 | 576 | 159 µs | 161 µs | 182 µs | 176 µs | 6,508 µs | 6,766 µs | 7,001 µs | 6,682 µs | 4,998 | 8 |
| duplicates | 1,000 | 1,000 | 990 | 1,990 | 5 | 192 | 768 | 141 µs | 133 µs | 130 µs | 140 µs | 6,919 µs | 6,701 µs | 6,548 µs | 6,609 µs | 4,243 | 18 |
| none | 1,000 | 0 | 1,000 | 1,000 | 0 | 192 | 288 | 103 µs | 91 µs | 75 µs | 74 µs | 6,598 µs | 6,641 µs | 6,443 µs | 6,809 µs | 1,000 | 0 |
| nulls | 1,000 | 1,000 | 0 | 1,000 | 0 | 192 | 288 | 169 µs | 153 µs | 127 µs | 124 µs | 6,468 µs | 6,693 µs | 6,630 µs | 6,644 µs | 1,000 | 0 |
| matched | 4,000 | 4,000 | 0 | 4,000 | 97 | 192 | 9,600 | 972 µs | 590 µs | 702 µs | 713 µs | 104,680 µs | 104,735 µs | 104,839 µs | 105,611 µs | 196,205 | 4,802 |
| few | 4,000 | 4,000 | 0 | 4,000 | 3 | 192 | 576 | 622 µs | 618 µs | 826 µs | 684 µs | 105,377 µs | 106,485 µs | 106,006 µs | 107,262 µs | 19,998 | 8 |
| duplicates | 4,000 | 4,000 | 3,990 | 7,990 | 5 | 192 | 768 | 512 µs | 494 µs | 516 µs | 476 µs | 105,928 µs | 104,828 µs | 106,829 µs | 104,166 µs | 16,993 | 18 |
| none | 4,000 | 0 | 4,000 | 4,000 | 0 | 192 | 288 | 328 µs | 307 µs | 256 µs | 276 µs | 107,627 µs | 105,326 µs | 104,151 µs | 104,733 µs | 4,000 | 0 |
| nulls | 4,000 | 4,000 | 0 | 4,000 | 0 | 192 | 288 | 552 µs | 521 µs | 547 µs | 476 µs | 104,510 µs | 105,195 µs | 104,886 µs | 104,527 µs | 4,000 | 0 |
| matched | 10,000 | 10,000 | 0 | 10,000 | 97 | 192 | 9,600 | 1,796 µs | 1,499 µs | 1,712 µs | 1,770 µs | 662,741 µs | 659,336 µs | 670,233 µs | 669,491 µs | 499,599 | 4,802 |
| few | 10,000 | 10,000 | 0 | 10,000 | 3 | 192 | 576 | 1,603 µs | 1,518 µs | 1,916 µs | 1,779 µs | — | — | — | — | 49,998 | 8 |
| duplicates | 10,000 | 10,000 | 9,990 | 19,990 | 5 | 192 | 768 | 1,246 µs | 1,257 µs | 1,157 µs | 1,200 µs | — | — | — | — | 42,493 | 18 |
| none | 10,000 | 0 | 10,000 | 10,000 | 0 | 192 | 288 | 818 µs | 762 µs | 691 µs | 643 µs | — | — | — | — | 10,000 | 0 |
| nulls | 10,000 | 10,000 | 0 | 10,000 | 0 | 192 | 288 | 1,552 µs | 1,404 µs | 1,205 µs | 1,232 µs | — | — | — | — | 10,000 | 0 |

At 10,000 by 10,000 matched rows, the ordinary hash warm time is 1,499 µs and the nested warm time is 659,336 µs, 439.9×. The distinct hash warm time is 1,770 µs and the nested warm time is 669,491 µs, 378.2×. The nested loop still examines 100,000,000 pairs. Square matched ordinary checksums are 8,802; 59,132; 196,205; and 499,599. Square matched distinct checksums are 8,698; 4,802; 4,802; and 4,802. The distinct checksum stops changing once every `id % 97` residue has been seen.

The distinct query is not uniformly slower than the ordinary query. At 10,000 duplicates the distinct hash warm time is 1,200 µs and the ordinary hash warm time is 1,257 µs. At 10,000 unmatched rows (`none`) those times are 643 µs and 762 µs. Three distinct amounts and 97 distinct amounts do not separate the 10,000 hash times by more than a few hundred microseconds, while the distinct-set estimate grows from 576 bytes to 9,600 bytes. NULL amounts (`nulls`) still match every left row, so that hash run stays near the matched run. These samples do not support a claim that distinct admission dominates the join.

## Milestone 23 SELECT DISTINCT over LEFT JOIN

Release profile `profile_milestone23`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes. The three queries share each inserted table. `plain` is `SELECT u.name, o.amount`. `distinct` is `SELECT DISTINCT u.name, o.amount`. `limit` is that distinct query with `ORDER BY u.name ASC, o.amount ASC LIMIT 10`. User ids are `0..users`. An order follows the Milestone 22 shapes, with `user_id = id % users` for `matched` and `few`. `hits` counts ON-true orders. `null-extended` counts left rows with no such order. `inputs` is the logical row count. `nested candidates` is `users × orders`, the comparisons the nested loop performs. The hash path builds the right table and does not publish a probe counter. Nested times are omitted where the run was not performed. `distinct bytes` estimates one stored tuple per unique projection, its hash entry, and one text byte per name. `join bytes` is 0: the distinct path does not buffer pairs. Neither figure is process RSS. A `HashMap` that cannot grow aborts the process. Checksums are independent sums of the projected cells. They matched the hash path and, where it ran, the nested path.

| Shape | Users | Orders | Hits | Null-extended | Inputs | Uniques | Distinct bytes | Plain hash warm | Distinct hash warm | Limit hash warm | Plain nested warm | Distinct nested warm | Limit nested warm | Plain checksum | Distinct checksum | Limit checksum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matched | 100 | 100 | 100 | 0 | 100 | 98 | 12,642 | 32 µs | 35 µs | 42 µs | 105 µs | 95 µs | 100 µs | 4,361 | 4,359 | 450 |
| few | 100 | 100 | 100 | 0 | 100 | 24 | 3,096 | 21 µs | 27 µs | 31 µs | 95 µs | 91 µs | 90 µs | 199 | 48 | 19 |
| duplicates | 100 | 100 | 100 | 90 | 190 | 18 | 2,322 | 21 µs | 29 µs | 32 µs | 83 µs | 91 µs | 94 µs | 340 | 38 | 23 |
| none | 100 | 100 | 0 | 100 | 100 | 8 | 1,032 | 16 µs | 22 µs | 24 µs | 80 µs | 85 µs | 87 µs | 100 | 8 | 8 |
| nulls | 100 | 100 | 100 | 0 | 100 | 8 | 1,032 | 21 µs | 25 µs | 27 µs | 83 µs | 85 µs | 132 µs | 100 | 8 | 8 |
| matched | 1,000 | 1,000 | 1,000 | 0 | 1,000 | 734 | 94,686 | 141 µs | 244 µs | 305 µs | 7,181 µs | 7,521 µs | 7,345 µs | 43,690 | 35,461 | 55 |
| few | 1,000 | 1,000 | 1,000 | 0 | 1,000 | 24 | 3,096 | 137 µs | 167 µs | 179 µs | 7,192 µs | 7,225 µs | 7,207 µs | 1,999 | 48 | 19 |
| duplicates | 1,000 | 1,000 | 1,000 | 990 | 1,990 | 18 | 2,322 | 146 µs | 189 µs | 190 µs | 6,936 µs | 7,197 µs | 7,175 µs | 3,490 | 38 | 23 |
| none | 1,000 | 1,000 | 0 | 1,000 | 1,000 | 8 | 1,032 | 85 µs | 116 µs | 119 µs | 6,927 µs | 7,058 µs | 7,309 µs | 1,000 | 8 | 8 |
| nulls | 1,000 | 1,000 | 1,000 | 0 | 1,000 | 8 | 1,032 | 158 µs | 178 µs | 163 µs | 7,155 µs | 7,266 µs | 7,318 µs | 1,000 | 8 | 8 |
| matched | 4,000 | 4,000 | 4,000 | 0 | 4,000 | 784 | 101,136 | 588 µs | 740 µs | 875 µs | 114,306 µs | 115,135 µs | 124,793 µs | 177,784 | 38,032 | 55 |
| few | 4,000 | 4,000 | 4,000 | 0 | 4,000 | 24 | 3,096 | 590 µs | 724 µs | 775 µs | 114,713 µs | 114,591 µs | 114,234 µs | 7,999 | 48 | 19 |
| duplicates | 4,000 | 4,000 | 4,000 | 3,990 | 7,990 | 18 | 2,322 | 515 µs | 696 µs | 798 µs | 116,857 µs | 113,329 µs | 114,072 µs | 13,990 | 38 | 23 |
| none | 4,000 | 4,000 | 0 | 4,000 | 4,000 | 8 | 1,032 | 305 µs | 423 µs | 441 µs | 114,712 µs | 114,846 µs | 116,407 µs | 4,000 | 8 | 8 |
| nulls | 4,000 | 4,000 | 4,000 | 0 | 4,000 | 8 | 1,032 | 532 µs | 736 µs | 652 µs | 114,905 µs | 116,133 µs | 115,238 µs | 4,000 | 8 | 8 |
| matched | 10,000 | 10,000 | 10,000 | 0 | 10,000 | 784 | 101,136 | 1,400 µs | 1,810 µs | 1,845 µs | 715,496 µs | 731,256 µs | 738,961 µs | 446,000 | 38,032 | 55 |
| few | 10,000 | 10,000 | 10,000 | 0 | 10,000 | 24 | 3,096 | 1,344 µs | 1,608 µs | 1,632 µs | — | — | — | 19,999 | 48 | 19 |
| duplicates | 10,000 | 10,000 | 10,000 | 9,990 | 19,990 | 18 | 2,322 | 1,236 µs | 1,713 µs | 1,748 µs | — | — | — | 34,990 | 38 | 23 |
| none | 10,000 | 10,000 | 0 | 10,000 | 10,000 | 8 | 1,032 | 739 µs | 1,025 µs | 1,031 µs | — | — | — | 10,000 | 8 | 8 |
| nulls | 10,000 | 10,000 | 10,000 | 0 | 10,000 | 8 | 1,032 | 1,259 µs | 1,711 µs | 1,605 µs | — | — | — | 10,000 | 8 | 8 |
| matched | 100 | 4,000 | 4,000 | 0 | 4,000 | 784 | 101,136 | 251 µs | 542 µs | 574 µs | 3,123 µs | 3,413 µs | 3,615 µs | 177,784 | 38,032 | 55 |
| matched | 4,000 | 100 | 100 | 3,900 | 4,000 | 98 | 12,642 | 211 µs | 340 µs | 385 µs | 3,121 µs | 3,020 µs | 3,091 µs | 8,261 | 4,359 | 450 |

At 10,000 by 10,000 matched rows, plain hash warm is 1,400 µs and nested warm is 715,496 µs, 511.1×. Distinct hash warm is 1,810 µs and nested warm is 731,256 µs, 404.0×. The plain hash cold time is 2,275 µs and the distinct hash cold time is 2,195 µs. The nested loop examines 100,000,000 pairs. Square matched plain checksums are 4,361; 43,690; 177,784; and 446,000. Square matched distinct checksums are 4,359; 35,461; 38,032; and 38,032. The distinct checksum stops changing once all eight names and the `id % 97` residues, including NULL, have been seen: 784 tuples.

Distinct admission is visible and small next to the join. At 10,000 matched rows the distinct hash warm time is 410 µs above the plain query. Twenty-four unique tuples and 784 unique tuples differ by 202 µs at that size, while the estimate grows from 3,096 bytes to 101,136 bytes. The unmatched `none` hash run is the cheapest distinct sample at 10,000, 1,025 µs. `ORDER BY` plus `LIMIT 10` stays near the full distinct time because the scan still admits every tuple before the sort. These samples do not support a claim that the distinct set dominates a hash left join.

## Milestone 24 index nested-loop LEFT JOIN

Release profile `profile_milestone24`, rerun 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes. Hash times are taken before `CREATE INDEX`. Index times force the probe on that index, including sizes the 64× rule would hash. Nested times force the loop. A zero nested time was not run. `probes` counts left rows with a non-NULL integer key: one B+ tree lookup each. `entries` counts right rows stored under those keys, before the residual `ON`. Every returned entry is one `ON` comparison, so the `ON` evaluation count equals `entries`. `nested candidates` is `users × orders`, which is a different count. `hash bytes` estimates the right-side hash table. `index bytes` is 0: the probe does not keep a pair buffer or a hash table. Neither figure is process RSS, and neither includes the on-disk tree. Checksums matched across hash, index, and, where it ran, the nested loop.

| Shape | Users | Orders | Probes | Entries | Hash bytes | Plain hash warm | Plain index warm | Plain nested warm | Distinct hash warm | Distinct index warm | Plain checksum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| matched | 100 | 100 | 100 | 100 | 7,200 | 26 µs | 111 µs | 125 µs | 37 µs | 80 µs | 4,361 |
| none | 100 | 100 | 100 | 0 | 864 | 17 µs | 68 µs | 85 µs | 22 µs | 73 µs | 100 |
| duplicates | 100 | 100 | 100 | 100 | 1,440 | 48 µs | 140 µs | 86 µs | 71 µs | 164 µs | 390 |
| selective | 100 | 100 | 100 | 1 | 928 | 38 µs | 131 µs | 185 µs | 48 µs | 142 µs | 100 |
| residual | 100 | 100 | 100 | 100 | 7,200 | 66 µs | 131 µs | 370 µs | 84 µs | 165 µs | 4,354 |
| matched | 1,000 | 1,000 | 1,000 | 1,000 | 72,000 | 339 µs | 887 µs | 7,430 µs | 589 µs | 1,243 µs | 43,690 |
| none | 1,000 | 1,000 | 1,000 | 0 | 8,064 | 99 µs | 1,188 µs | 6,766 µs | 136 µs | 1,230 µs | 1,000 |
| duplicates | 1,000 | 1,000 | 1,000 | 1,000 | 8,640 | 128 µs | 1,403 µs | 6,956 µs | 218 µs | 1,341 µs | 3,990 |
| selective | 1,000 | 1,000 | 1,000 | 10 | 8,128 | 109 µs | 1,149 µs | 6,891 µs | 142 µs | 1,152 µs | 1,089 |
| residual | 1,000 | 1,000 | 1,000 | 1,000 | 72,000 | 175 µs | 953 µs | 15,201 µs | 311 µs | 1,024 µs | 43,630 |
| matched | 4,000 | 4,000 | 4,000 | 4,000 | 288,000 | 589 µs | 3,144 µs | 107,887 µs | 813 µs | 3,247 µs | 177,784 |
| none | 4,000 | 4,000 | 4,000 | 0 | 32,064 | 382 µs | 4,316 µs | 108,536 µs | 471 µs | 4,314 µs | 4,000 |
| duplicates | 4,000 | 4,000 | 4,000 | 4,000 | 32,640 | 505 µs | 4,770 µs | 108,299 µs | 769 µs | 5,148 µs | 15,990 |
| selective | 4,000 | 4,000 | 4,000 | 40 | 32,128 | 407 µs | 5,090 µs | 107,820 µs | 520 µs | 4,601 µs | 4,366 |
| residual | 4,000 | 4,000 | 4,000 | 4,000 | 288,000 | 707 µs | 3,041 µs | 244,810 µs | 836 µs | 3,313 µs | 177,556 |
| matched | 10,000 | 10,000 | 10,000 | 10,000 | 720,000 | 1,333 µs | 8,492 µs | 685,743 µs | 1,682 µs | 9,463 µs | 446,000 |
| none | 10,000 | 10,000 | 10,000 | 0 | 80,064 | 779 µs | 12,160 µs | — | 1,075 µs | 12,361 µs | 10,000 |
| duplicates | 10,000 | 10,000 | 10,000 | 10,000 | 80,640 | 1,122 µs | 12,360 µs | — | 1,700 µs | 13,130 µs | 39,990 |
| selective | 10,000 | 10,000 | 10,000 | 100 | 80,128 | 770 µs | 11,717 µs | — | 1,122 µs | 12,035 µs | 10,909 |
| residual | 10,000 | 10,000 | 10,000 | 10,000 | 720,000 | 1,409 µs | 8,901 µs | — | 1,838 µs | 10,033 µs | 445,432 |
| matched | 100 | 10,000 | 100 | 10,000 | 86,400 | 681 µs | 750 µs | 7,035 µs | 1,136 µs | 1,161 µs | 446,000 |
| matched | 10,000 | 100 | 10,000 | 100 | 7,200 | 525 µs | 5,812 µs | 6,923 µs | 1,002 µs | 6,055 µs | 14,261 |

At 10,000 by 10,000 matched rows, plain hash warm is 1,333 µs, plain index warm is 8,492 µs, and plain nested warm is 685,743 µs. The nested loop is 514.4× the hash and 80.8× the index. The index is 6.4× the hash. Plain hash cold is 1,364 µs and plain index cold is 10,539 µs. The aggregate warm times are 1,460 µs, 8,702 µs, and 690,447 µs. The distinct `ORDER BY` plus `LIMIT 10` warm times are 1,728 µs, 9,064 µs, and 687,710 µs. The index makes 10,000 probes and visits 10,000 entries, and those 10,000 entries are the `ON` comparisons. The nested loop examines 100,000,000 pairs. Square matched plain checksums are 4,361; 43,690; 177,784; and 446,000.

The index is not uniformly faster than the hash. Equal sizes stay on the hash: at 10,000 by 10,000 the hash warm time is lower for every shape in this run. At 10,000 by 100, outside the size rule, hash warm is 525 µs and index warm is 5,812 µs. That is 10,000 probes visiting 100 entries.

The 100 by 10,000 case is inside the size rule: 100 probes visit 10,000 entries. One sample in the table has hash warm 681 µs and index warm 750 µs. Seven fresh databases, each reporting the median of five warm runs, gave hash medians 656, 706, 708, 781, 834, 906, and 1,109 µs, and index medians 755, 771, 804, 809, 827, 834, and 882 µs. The median of those medians is 781 µs for the hash and 809 µs for the index. The index was faster on 4 of the 7 databases and slower on 3. The ranges overlap. This does not support treating the index as faster than the hash at 100 by 10,000. It does support keeping the hash when the right table is not much larger than the left, and it does not support replacing every hash left join with an index probe.

## Milestone 25 indexed UPDATE and DELETE

Release profile `profile_milestone25`, rerun 15 Nov 2026 after the heap equality check. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes. Each size loads `id` from 0 through `rows - 1`, name `n{id}`, `active` TRUE, and `bucket = id % 10`, then creates `idx_users_bucket` and `idx_users_id` in that catalog order. The index database uses the mutation planner, including the heap walk that requires every row with the chosen integer to have a posting. The scan database runs the same SQL with the heap scan forced. The checksum is `SELECT id, name, active, bucket` after the untimed statement and, where the workload has one, its restore. The two databases matched on every row below. Loading the tables is outside the timer.

Below 100,000 rows there are six timed samples. At 100,000 there are three. Cold is the first sample. Warm is the median of the rest: the middle of five, or the upper of two. `point`, `duplicates`, and `residual` assign `name`. The untimed statement assigns `'n'`. Timed samples alternate `'m'` and `'n'`, so every timed sample changes a cell and writes the WAL. `key` sets `id = -1` and then restores `id = 1` outside the timer. `delete_one` deletes `id = 1` and reinserts that row outside the timer. `delete_many` deletes `bucket = 1` (10% of the rows) and reinserts those rows outside the timer. `miss` matches nothing.

`probe`, `validate`, `filter`, `stage`, and `apply` come from that untimed index execution, not from the timed median. `probe` is the B+ tree lookup. `validate` is the heap walk that builds the equality set and compares it with the index. `filter` is the original `WHERE` on that set. `stage` builds the replacement. `apply` is only the in-memory `rewrite_all` or `retain_indexes`. `discover` in the profile log is `probe + validate + filter`. `staged_bytes` is `modified * 8 + staged_rows * 4 * 32`: one `u64` per matched row id, plus the staged image at 32 bytes per cell. `UPDATE` stages every heap row. `DELETE` stages the survivors. The equality walk does not copy rows. It keeps one `usize` per matching position until the comparison finishes. None of these figures is process RSS.

`wal` is WAL write, WAL sync, database write, database sync, and WAL clear on the indexed database. `publish` is `Editor::apply`, which includes the heap compare and index maintenance. `compare` is the `same_values` time inside that apply. Index maintenance is not a separate timer; for an `UPDATE`, `publish - compare` is the remainder and includes the changed record plus any index edit. A `DELETE` takes the unlink path, so `compare` and `compares` stay 0 and `publish` is the unlink plus index maintenance. `compares` is the last indexed sample. An `UPDATE` compares every heap row, including a miss that writes nothing.

| Workload | Rows | Index cold | Index warm | Scan cold | Scan warm | WAL warm | Publish warm | Compare warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| point | 100 | 11,575 µs | 12,008 µs | 11,050 µs | 10,048 µs | 11,443 µs | 277 µs | 102 µs |
| miss | 100 | 99 µs | 83 µs | 87 µs | 79 µs | 0 µs | 30 µs | 21 µs |
| duplicates | 100 | 10,815 µs | 12,013 µs | 9,837 µs | 10,984 µs | 11,365 µs | 294 µs | 91 µs |
| residual | 100 | 10,804 µs | 11,000 µs | 11,112 µs | 11,088 µs | 10,489 µs | 230 µs | 92 µs |
| key | 100 | 11,921 µs | 9,022 µs | 8,924 µs | 10,108 µs | 8,515 µs | 285 µs | 89 µs |
| delete_one | 100 | 12,963 µs | 12,022 µs | 12,841 µs | 11,947 µs | 11,249 µs | 417 µs | 0 µs |
| delete_many | 100 | 12,933 µs | 12,288 µs | 13,039 µs | 11,996 µs | 11,500 µs | 435 µs | 0 µs |
| point | 1,000 | 12,757 µs | 12,926 µs | 11,952 µs | 11,836 µs | 10,820 µs | 956 µs | 743 µs |
| miss | 1,000 | 984 µs | 456 µs | 452 µs | 450 µs | 0 µs | 176 µs | 113 µs |
| duplicates | 1,000 | 9,810 µs | 11,078 µs | 11,048 µs | 12,911 µs | 9,126 µs | 1,680 µs | 654 µs |
| residual | 1,000 | 12,598 µs | 12,914 µs | 12,055 µs | 12,205 µs | 11,354 µs | 887 µs | 683 µs |
| key | 1,000 | 13,046 µs | 13,991 µs | 13,032 µs | 13,900 µs | 11,386 µs | 995 µs | 664 µs |
| delete_one | 1,000 | 11,989 µs | 13,113 µs | 11,455 µs | 13,012 µs | 11,355 µs | 620 µs | 0 µs |
| delete_many | 1,000 | 13,020 µs | 13,920 µs | 12,993 µs | 13,170 µs | 11,358 µs | 1,547 µs | 0 µs |
| point | 10,000 | 15,913 µs | 17,022 µs | 16,817 µs | 17,919 µs | 9,980 µs | 4,941 µs | 4,489 µs |
| miss | 10,000 | 2,890 µs | 2,872 µs | 3,022 µs | 2,913 µs | 0 µs | 1,141 µs | 732 µs |
| duplicates | 10,000 | 29,091 µs | 28,016 µs | 25,872 µs | 27,976 µs | 11,002 µs | 11,475 µs | 4,284 µs |
| residual | 10,000 | 16,026 µs | 16,800 µs | 17,033 µs | 17,080 µs | 9,695 µs | 4,921 µs | 4,466 µs |
| key | 10,000 | 17,104 µs | 17,974 µs | 15,804 µs | 16,959 µs | 10,548 µs | 5,245 µs | 4,598 µs |
| delete_one | 10,000 | 13,059 µs | 14,170 µs | 15,027 µs | 15,097 µs | 10,499 µs | 906 µs | 0 µs |
| delete_many | 10,000 | 24,876 µs | 23,931 µs | 23,988 µs | 24,059 µs | 10,768 µs | 9,218 µs | 0 µs |
| point | 100,000 | 75,998 µs | 76,878 µs | 75,191 µs | 74,061 µs | 10,749 µs | 47,188 µs | 43,834 µs |
| miss | 100,000 | 31,276 µs | 31,297 µs | 31,990 µs | 32,726 µs | 0 µs | 12,093 µs | 7,976 µs |
| duplicates | 100,000 | 187,950 µs | 187,826 µs | 183,442 µs | 183,723 µs | 14,774 µs | 115,422 µs | 43,599 µs |
| residual | 100,000 | 74,597 µs | 77,898 µs | 79,551 µs | 76,550 µs | 10,781 µs | 47,205 µs | 43,657 µs |
| key | 100,000 | 76,440 µs | 77,103 µs | 76,836 µs | 75,282 µs | 10,756 µs | 46,413 µs | 42,052 µs |
| delete_one | 100,000 | 32,944 µs | 34,327 µs | 34,809 µs | 34,083 µs | 9,561 µs | 2,971 µs | 0 µs |
| delete_many | 100,000 | 306,258 µs | 298,810 µs | 295,015 µs | 294,386 µs | 13,669 µs | 243,863 µs | 0 µs |

| Workload | Rows | Probe | Validate | Filter | Stage | Apply | Staged bytes | Checksum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| point | 100 | 75 µs | 0 µs | 0 µs | 21 µs | 4 µs | 12,808 | 5,789 |
| miss | 100 | 56 µs | 0 µs | 0 µs | 6 µs | 4 µs | 12,800 | 5,790 |
| duplicates | 100 | 47 µs | 0 µs | 1 µs | 10 µs | 4 µs | 12,880 | 5,771 |
| residual | 100 | 47 µs | 0 µs | 1 µs | 8 µs | 4 µs | 12,808 | 5,789 |
| key | 100 | 44 µs | 0 µs | 1 µs | 7 µs | 4 µs | 12,808 | 5,788 |
| delete_one | 100 | 54 µs | 0 µs | 1 µs | 5 µs | 12 µs | 12,680 | 5,785 |
| delete_many | 100 | 59 µs | 1 µs | 1 µs | 5 µs | 10 µs | 11,600 | 5,281 |
| point | 1,000 | 83 µs | 2 µs | 1 µs | 71 µs | 40 µs | 128,008 | 508,889 |
| miss | 1,000 | 76 µs | 1 µs | 0 µs | 56 µs | 38 µs | 128,000 | 508,890 |
| duplicates | 1,000 | 59 µs | 1 µs | 2 µs | 47 µs | 16 µs | 128,800 | 508,601 |
| residual | 1,000 | 82 µs | 1 µs | 1 µs | 71 µs | 33 µs | 128,008 | 508,889 |
| key | 1,000 | 118 µs | 1 µs | 1 µs | 58 µs | 36 µs | 128,008 | 508,888 |
| delete_one | 1,000 | 95 µs | 3 µs | 1 µs | 29 µs | 117 µs | 127,880 | 508,885 |
| delete_many | 1,000 | 90 µs | 3 µs | 6 µs | 28 µs | 100 µs | 116,000 | 458,701 |
| point | 10,000 | 59 µs | 13 µs | 0 µs | 278 µs | 182 µs | 1,280,008 | 50,098,889 |
| miss | 10,000 | 64 µs | 16 µs | 0 µs | 247 µs | 186 µs | 1,280,000 | 50,098,890 |
| duplicates | 10,000 | 292 µs | 23 µs | 37 µs | 379 µs | 198 µs | 1,288,000 | 50,095,001 |
| residual | 10,000 | 75 µs | 15 µs | 0 µs | 290 µs | 175 µs | 1,280,008 | 50,098,889 |
| key | 10,000 | 61 µs | 10 µs | 0 µs | 276 µs | 184 µs | 1,280,008 | 50,098,888 |
| delete_one | 10,000 | 62 µs | 15 µs | 0 µs | 61 µs | 532 µs | 1,279,880 | 50,098,885 |
| delete_many | 10,000 | 290 µs | 16 µs | 17 µs | 86 µs | 452 µs | 1,160,000 | 45,096,001 |
| point | 100,000 | 100 µs | 254 µs | 0 µs | 3,627 µs | 1,938 µs | 12,800,008 | 5,001,088,889 |
| miss | 100,000 | 108 µs | 309 µs | 0 µs | 2,345 µs | 2,044 µs | 12,800,000 | 5,001,088,890 |
| duplicates | 100,000 | 3,171 µs | 442 µs | 277 µs | 3,854 µs | 1,860 µs | 12,880,000 | 5,001,040,001 |
| residual | 100,000 | 555 µs | 307 µs | 1 µs | 2,704 µs | 2,571 µs | 12,800,008 | 5,001,088,889 |
| key | 100,000 | 98 µs | 173 µs | 0 µs | 2,695 µs | 2,049 µs | 12,800,008 | 5,001,088,888 |
| delete_one | 100,000 | 126 µs | 448 µs | 1 µs | 523 µs | 5,570 µs | 12,799,880 | 5,001,088,885 |
| delete_many | 100,000 | 3,133 µs | 503 µs | 290 µs | 948 µs | 5,165 µs | 11,600,000 | 4,501,050,001 |

At 100,000 rows a point update is 76,878 µs indexed and 74,061 µs scanned. The probe is 100 µs and the heap equality check is 254 µs. The WAL warm time is 10,749 µs. Publish warm is 47,188 µs, of which the heap compare is 43,834 µs, and that compare visits all 100,000 rows. Staging the replacement is 3,627 µs. A miss writes no WAL: indexed warm 31,297 µs, scanned warm 32,726 µs, probe 108 µs, validate 309 µs. Changing the indexed key is 77,103 µs indexed and 75,282 µs scanned. The index edit sits in the few milliseconds of publish that remain after the compare.

Deleting one row at 100,000 is 34,327 µs indexed and 34,083 µs scanned. The value compare is 0. Publish warm is 2,971 µs, the WAL warm time is 9,561 µs, and the equality check is 448 µs. Deleting the 10,000 rows with `bucket = 1` is 298,810 µs indexed and 294,386 µs scanned. Publish warm is 243,863 µs and the WAL warm time is 13,669 µs. The probe for those 10,000 candidates is 3,133 µs and the equality check is 503 µs. Updating the same 10,000 rows is 187,826 µs indexed and 183,723 µs scanned.

The equality check is visible and small next to publish and the WAL. At 100,000 rows it stays under 510 µs on every workload in this run. The index and the forced scan stay within a few percent. This run does not support a claim that indexed candidate selection makes `UPDATE` or `DELETE` faster. The statement still stages a full replacement, and the WAL plus the publish walk dominate both the probe and the completeness check. The staged image at 100,000 rows is 12,800,008 bytes for one changed row and 11,600,000 bytes after deleting 10,000 rows. Those figures are the estimate above, not process RSS.

## Milestone 26 selective UPDATE publishing

Release profile `profile_milestone26`, 15 Nov 2026. Apple M5, 17179869184 bytes of memory, `rustc 1.100.0 (0ef1bc386 2026-11-09)`. `Value` is 32 bytes. The table is the Milestone 25 table: `id` from 0 through `rows - 1`, name `n{id}`, `active` TRUE, `bucket = id % 10`, then `idx_users_bucket` and `idx_users_id`. One database is built per size and copied. The selective database publishes only the matched row ids. The full database runs the same SQL through the full-rewrite guard, which still builds a replacement of every row and compares every stored value. Checksums of `SELECT id, name, active, bucket` matched after the untimed statement. Loading is outside the timer.

Below 100,000 rows there are six timed samples. At 100,000 there are three. Cold is the first sample. Warm is the median of the rest. `one`, `ten`, `many`, and `duplicates` assign `name`, alternating `'m'` and `'n'` after an untimed `'n'`, so every timed sample writes. `fixed` assigns `active = FALSE` before the timer, so its cold sample assigns `FALSE` again and can skip the write. Its warm samples alternate `TRUE` and `FALSE` and do write. `key` sets `id = -1` and restores `id = 1` outside the timer. `grow` assigns a 200-character name and restores `n1`. `shrink` starts from that 200-character name, assigns `n`, and restores the long name outside the timer. `miss` matches nothing. `ten` is `id < 10`, a heap scan of exactly 10 rows. `many` is `active = TRUE`, a heap scan of every row. `duplicates` is `bucket = 1`, about 10% of the rows, through the index.

`probe`, `validate`, `filter`, `stage`, and `apply` are the untimed selective execution. `discover` in the log is `probe + validate + filter`. `staged_bytes` is `modified * 8 + staged_rows * 4 * 32`. On the selective path `staged_rows` is the match count. The full guard stages every heap row, so a one-row update of 100,000 rows is 12,800,008 bytes there and 136 bytes here. A miss is 12,800,000 bytes there and 0 here. An update of every row is 13,600,000 bytes on both. None of this is process RSS. The pre-statement undo still clones the table on both paths and is not included.

`wal` is WAL write, WAL sync, database write, database sync, and WAL clear. `publish` is the editor apply. `compare` is `same_values` inside that apply. `compares` is the last sample. WAL bytes and the heap, index, and other page counts were the same on both paths for every workload in this run. Other pages are page 0 and any catalog page. Frames are the sum. A name change dirties no index page. A key change does. Growing the text onto a new heap page adds a heap page. Shrinking it back fits the old slot and does not.

| Workload | Rows | Selective cold | Selective warm | Full cold | Full warm | WAL sel | WAL full | Publish sel | Publish full | Compare sel | Compare full |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| one | 100 | 10,653 µs | 10,988 µs | 11,028 µs | 12,016 µs | 10,602 µs | 11,433 µs | 185 µs | 341 µs | 39 µs | 129 µs |
| miss | 100 | 53 µs | 24 µs | 69 µs | 66 µs | 0 µs | 0 µs | 0 µs | 23 µs | 0 µs | 16 µs |
| ten | 100 | 10,935 µs | 10,986 µs | 10,024 µs | 11,944 µs | 10,605 µs | 11,327 µs | 225 µs | 352 µs | 53 µs | 130 µs |
| many | 100 | 11,695 µs | 11,839 µs | 12,814 µs | 11,271 µs | 11,206 µs | 10,638 µs | 360 µs | 359 µs | 95 µs | 98 µs |
| key | 100 | 11,954 µs | 11,919 µs | 12,960 µs | 12,911 µs | 11,424 µs | 12,295 µs | 230 µs | 292 µs | 38 µs | 90 µs |
| fixed | 100 | 151 µs | 11,774 µs | 290 µs | 10,968 µs | 11,385 µs | 10,511 µs | 167 µs | 227 µs | 37 µs | 89 µs |
| grow | 100 | 9,949 µs | 11,833 µs | 9,969 µs | 10,933 µs | 11,235 µs | 10,502 µs | 257 µs | 233 µs | 34 µs | 84 µs |
| shrink | 100 | 11,062 µs | 11,164 µs | 12,012 µs | 11,125 µs | 10,776 µs | 10,582 µs | 229 µs | 280 µs | 46 µs | 125 µs |
| duplicates | 100 | 12,868 µs | 11,978 µs | 11,064 µs | 11,962 µs | 11,445 µs | 11,252 µs | 269 µs | 303 µs | 72 µs | 95 µs |
| one | 1,000 | 12,345 µs | 10,062 µs | 11,044 µs | 11,852 µs | 9,481 µs | 10,214 µs | 193 µs | 863 µs | 38 µs | 660 µs |
| miss | 1,000 | 137 µs | 122 µs | 688 µs | 662 µs | 0 µs | 0 µs | 1 µs | 270 µs | 0 µs | 200 µs |
| ten | 1,000 | 12,668 µs | 11,938 µs | 11,062 µs | 12,032 µs | 11,408 µs | 10,592 µs | 212 µs | 888 µs | 51 µs | 678 µs |
| many | 1,000 | 15,531 µs | 14,939 µs | 15,205 µs | 15,003 µs | 10,979 µs | 11,212 µs | 2,895 µs | 2,628 µs | 836 µs | 789 µs |
| key | 1,000 | 12,965 µs | 12,040 µs | 13,249 µs | 13,041 µs | 11,088 µs | 11,323 µs | 503 µs | 1,009 µs | 55 µs | 671 µs |
| fixed | 1,000 | 427 µs | 11,938 µs | 2,028 µs | 12,931 µs | 11,203 µs | 11,412 µs | 283 µs | 860 µs | 45 µs | 657 µs |
| grow | 1,000 | 12,999 µs | 12,040 µs | 13,090 µs | 13,063 µs | 11,297 µs | 11,350 µs | 285 µs | 996 µs | 35 µs | 731 µs |
| shrink | 1,000 | 10,936 µs | 11,943 µs | 13,109 µs | 12,937 µs | 11,360 µs | 11,359 µs | 191 µs | 890 µs | 37 µs | 688 µs |
| duplicates | 1,000 | 14,641 µs | 13,956 µs | 14,094 µs | 13,998 µs | 11,591 µs | 11,130 µs | 1,480 µs | 1,697 µs | 479 µs | 657 µs |
| one | 10,000 | 12,487 µs | 12,001 µs | 19,360 µs | 18,855 µs | 10,897 µs | 10,798 µs | 246 µs | 5,575 µs | 31 µs | 5,029 µs |
| miss | 10,000 | 536 µs | 527 µs | 2,872 µs | 2,733 µs | 0 µs | 0 µs | 3 µs | 1,085 µs | 0 µs | 754 µs |
| ten | 10,000 | 12,822 µs | 11,884 µs | 18,484 µs | 18,161 µs | 10,691 µs | 10,502 µs | 236 µs | 5,460 µs | 31 µs | 4,950 µs |
| many | 10,000 | 34,975 µs | 29,110 µs | 30,083 µs | 28,774 µs | 10,714 µs | 9,453 µs | 13,674 µs | 13,249 µs | 4,065 µs | 4,144 µs |
| key | 10,000 | 12,556 µs | 12,076 µs | 22,726 µs | 20,085 µs | 10,840 µs | 11,448 µs | 447 µs | 5,751 µs | 38 µs | 5,108 µs |
| fixed | 10,000 | 864 µs | 12,794 µs | 8,483 µs | 18,179 µs | 11,721 µs | 10,412 µs | 232 µs | 5,462 µs | 29 µs | 4,949 µs |
| grow | 10,000 | 11,555 µs | 12,016 µs | 22,682 µs | 19,069 µs | 10,633 µs | 10,883 µs | 378 µs | 5,786 µs | 34 µs | 5,201 µs |
| shrink | 10,000 | 11,641 µs | 12,075 µs | 23,606 µs | 18,977 µs | 10,699 µs | 11,101 µs | 339 µs | 5,539 µs | 41 µs | 5,005 µs |
| duplicates | 10,000 | 26,582 µs | 25,908 µs | 28,391 µs | 27,065 µs | 11,063 µs | 10,828 µs | 10,286 µs | 11,093 µs | 3,468 µs | 4,166 µs |
| one | 100,000 | 18,804 µs | 18,006 µs | 74,766 µs | 72,221 µs | 10,549 µs | 11,276 µs | 1,080 µs | 43,555 µs | 28 µs | 40,209 µs |
| miss | 100,000 | 6,490 µs | 6,065 µs | 29,031 µs | 29,485 µs | 0 µs | 0 µs | 27 µs | 11,352 µs | 0 µs | 8,017 µs |
| ten | 100,000 | 19,178 µs | 18,140 µs | 74,075 µs | 72,658 µs | 10,503 µs | 11,020 µs | 1,061 µs | 43,448 µs | 32 µs | 40,109 µs |
| many | 100,000 | 206,349 µs | 208,142 µs | 211,218 µs | 211,072 µs | 16,004 µs | 16,336 µs | 139,142 µs | 133,290 µs | 41,205 µs | 41,449 µs |
| key | 100,000 | 18,305 µs | 19,039 µs | 75,112 µs | 72,829 µs | 11,384 µs | 11,283 µs | 1,229 µs | 43,543 µs | 30 µs | 40,041 µs |
| fixed | 100,000 | 7,307 µs | 19,057 µs | 61,696 µs | 72,169 µs | 11,752 µs | 10,908 µs | 1,046 µs | 43,536 µs | 28 µs | 40,213 µs |
| grow | 100,000 | 18,846 µs | 19,177 µs | 73,438 µs | 73,929 µs | 11,501 µs | 12,009 µs | 1,301 µs | 43,849 µs | 33 µs | 40,411 µs |
| shrink | 100,000 | 19,787 µs | 20,141 µs | 73,795 µs | 72,878 µs | 12,300 µs | 10,330 µs | 1,102 µs | 43,800 µs | 30 µs | 40,470 µs |
| duplicates | 100,000 | 156,462 µs | 155,849 µs | 179,198 µs | 177,187 µs | 15,883 µs | 15,310 µs | 98,527 µs | 108,111 µs | 33,401 µs | 41,497 µs |

| Workload | Rows | Access | Candidates | Modified | Probe | Validate | Filter | Staged bytes | Compares sel | Compares full | WAL bytes | Heap | Index | Frames | Checksum |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| one | 100 | IndexLookup | 1 | 1 | 13 µs | 0 µs | 0 µs | 136 | 1 | 100 | 8,296 | 1 | 0 | 2 | 5,789 |
| miss | 100 | IndexLookup | 0 | 0 | 2 µs | 0 µs | 0 µs | 0 | 0 | 100 | 0 | 0 | 0 | 0 | 5,790 |
| ten | 100 | HeapScan | 100 | 10 | 0 µs | 0 µs | 4 µs | 1,360 | 10 | 100 | 8,296 | 1 | 0 | 2 | 5,780 |
| many | 100 | HeapScan | 100 | 100 | 0 µs | 0 µs | 18 µs | 13,600 | 100 | 100 | 12,396 | 2 | 0 | 3 | 5,600 |
| key | 100 | IndexLookup | 1 | 1 | 3 µs | 0 µs | 1 µs | 136 | 1 | 100 | 12,396 | 1 | 1 | 3 | 5,788 |
| fixed | 100 | IndexLookup | 1 | 1 | 3 µs | 0 µs | 1 µs | 136 | 1 | 100 | 8,296 | 1 | 0 | 2 | 5,789 |
| grow | 100 | IndexLookup | 1 | 1 | 3 µs | 0 µs | 1 µs | 136 | 1 | 100 | 12,396 | 2 | 0 | 3 | 5,988 |
| shrink | 100 | IndexLookup | 1 | 1 | 31 µs | 0 µs | 0 µs | 136 | 1 | 100 | 8,296 | 1 | 0 | 2 | 5,789 |
| duplicates | 100 | IndexLookup | 10 | 10 | 4 µs | 0 µs | 2 µs | 1,360 | 10 | 100 | 12,396 | 2 | 0 | 3 | 5,771 |
| one | 1,000 | IndexLookup | 1 | 1 | 2 µs | 0 µs | 0 µs | 136 | 1 | 1,000 | 8,296 | 1 | 0 | 2 | 508,889 |
| miss | 1,000 | IndexLookup | 0 | 0 | 4 µs | 1 µs | 0 µs | 0 | 0 | 1,000 | 0 | 0 | 0 | 0 | 508,890 |
| ten | 1,000 | HeapScan | 1,000 | 10 | 0 µs | 0 µs | 18 µs | 1,360 | 10 | 1,000 | 8,296 | 1 | 0 | 2 | 508,880 |
| many | 1,000 | HeapScan | 1,000 | 1,000 | 0 µs | 0 µs | 154 µs | 136,000 | 1,000 | 1,000 | 61,596 | 14 | 0 | 15 | 506,000 |
| key | 1,000 | IndexLookup | 1 | 1 | 10 µs | 1 µs | 1 µs | 136 | 1 | 1,000 | 16,496 | 1 | 2 | 4 | 508,888 |
| fixed | 1,000 | IndexLookup | 1 | 1 | 4 µs | 2 µs | 0 µs | 136 | 1 | 1,000 | 8,296 | 1 | 0 | 2 | 508,889 |
| grow | 1,000 | IndexLookup | 1 | 1 | 3 µs | 2 µs | 1 µs | 136 | 1 | 1,000 | 12,396 | 2 | 0 | 3 | 509,088 |
| shrink | 1,000 | IndexLookup | 1 | 1 | 79 µs | 1 µs | 0 µs | 136 | 1 | 1,000 | 8,296 | 1 | 0 | 2 | 508,889 |
| duplicates | 1,000 | IndexLookup | 100 | 100 | 11 µs | 4 µs | 5 µs | 13,600 | 100 | 1,000 | 61,596 | 14 | 0 | 15 | 508,601 |
| one | 10,000 | IndexLookup | 1 | 1 | 3 µs | 11 µs | 0 µs | 136 | 1 | 10,000 | 8,296 | 1 | 0 | 2 | 50,098,889 |
| miss | 10,000 | IndexLookup | 0 | 0 | 2 µs | 11 µs | 0 µs | 0 | 0 | 10,000 | 0 | 0 | 0 | 0 | 50,098,890 |
| ten | 10,000 | HeapScan | 10,000 | 10 | 0 µs | 0 µs | 74 µs | 1,360 | 10 | 10,000 | 8,296 | 1 | 0 | 2 | 50,098,880 |
| many | 10,000 | HeapScan | 10,000 | 10,000 | 0 µs | 0 µs | 812 µs | 1,360,000 | 10,000 | 10,000 | 574,096 | 139 | 0 | 140 | 50,060,000 |
| key | 10,000 | IndexLookup | 1 | 1 | 2 µs | 10 µs | 0 µs | 136 | 1 | 10,000 | 16,496 | 1 | 2 | 4 | 50,098,888 |
| fixed | 10,000 | IndexLookup | 1 | 1 | 2 µs | 10 µs | 0 µs | 136 | 1 | 10,000 | 8,296 | 1 | 0 | 2 | 50,098,889 |
| grow | 10,000 | IndexLookup | 1 | 1 | 2 µs | 10 µs | 0 µs | 136 | 1 | 10,000 | 12,396 | 2 | 0 | 3 | 50,099,088 |
| shrink | 10,000 | IndexLookup | 1 | 1 | 62 µs | 12 µs | 0 µs | 136 | 1 | 10,000 | 8,296 | 1 | 0 | 2 | 50,098,889 |
| duplicates | 10,000 | IndexLookup | 1,000 | 1,000 | 34 µs | 14 µs | 15 µs | 136,000 | 1,000 | 10,000 | 574,096 | 139 | 0 | 140 | 50,095,001 |
| one | 100,000 | IndexLookup | 1 | 1 | 6 µs | 136 µs | 0 µs | 136 | 1 | 100,000 | 8,296 | 1 | 0 | 2 | 5,001,088,889 |
| miss | 100,000 | IndexLookup | 0 | 0 | 5 µs | 165 µs | 0 µs | 0 | 0 | 100,000 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| ten | 100,000 | HeapScan | 100,000 | 10 | 0 µs | 0 µs | 886 µs | 1,360 | 10 | 100,000 | 8,296 | 1 | 0 | 2 | 5,001,088,880 |
| many | 100,000 | HeapScan | 100,000 | 100,000 | 0 µs | 0 µs | 6,731 µs | 13,600,000 | 100,000 | 100,000 | 5,772,896 | 1,407 | 0 | 1,408 | 5,000,600,000 |
| key | 100,000 | IndexLookup | 1 | 1 | 5 µs | 198 µs | 0 µs | 136 | 1 | 100,000 | 20,596 | 1 | 3 | 5 | 5,001,088,888 |
| fixed | 100,000 | IndexLookup | 1 | 1 | 5 µs | 166 µs | 0 µs | 136 | 1 | 100,000 | 8,296 | 1 | 0 | 2 | 5,001,088,889 |
| grow | 100,000 | IndexLookup | 1 | 1 | 5 µs | 189 µs | 0 µs | 136 | 1 | 100,000 | 12,396 | 2 | 0 | 3 | 5,001,089,088 |
| shrink | 100,000 | IndexLookup | 1 | 1 | 102 µs | 175 µs | 0 µs | 136 | 1 | 100,000 | 8,296 | 1 | 0 | 2 | 5,001,088,889 |
| duplicates | 100,000 | IndexLookup | 10,000 | 10,000 | 903 µs | 409 µs | 243 µs | 1,360,000 | 10,000 | 100,000 | 5,768,796 | 1,406 | 0 | 1,407 | 5,001,040,001 |

At 100,000 rows a one-row name update is 18,006 µs selective and 72,221 µs on the full-rewrite guard. The compare is 28 µs against one row, versus 40,209 µs against all 100,000. Publish is 1,080 µs versus 43,555 µs. WAL time is 10,549 µs versus 11,276 µs, and both logs are 8,296 bytes: page 0 and one heap page. WAL sync on those warm samples is 3,609 µs and 4,124 µs. The durable write did not get smaller. The CPU compare and the full-image publish did. The equality check is 136 µs. Staging the matched row is 136 bytes, against 12,800,008 bytes for the guard. The undo clone is extra on both and is not in that figure.

A miss at 100,000 rows is 6,065 µs selective and 29,485 µs full. Both write 0 WAL bytes. The selective path compares 0 rows. The full path compares 100,000 and spends 8,017 µs in that compare and 11,352 µs in publish. The selective validate walk is 165 µs. The statement is not free: the equality check and the rest of execution remain.

Ten rows at 100,000, found by a heap scan, are 18,140 µs versus 72,658 µs. The scan's filter is 886 µs. The publish compare is 32 µs versus 40,109 µs. WAL bytes stay 8,296 because those ten ids share one heap page. Changing the indexed key is 19,039 µs versus 72,829 µs, with 20,596 WAL bytes, one heap page, and three index pages on both paths. Growing a name to 200 characters is 19,177 µs versus 73,929 µs, 12,396 WAL bytes, and two heap pages: the record moved to a new page. Shrinking it is 20,141 µs versus 72,878 µs, 8,296 WAL bytes, and one heap page. Neither growth nor shrinkage fell back to a full-table image. A boolean update has the same shape as the one-row name update once the warm samples actually change the bit. Its cold sample is the no-op described above.

Updating every row at 100,000 is 208,142 µs selective and 211,072 µs full. Both compare 100,000 rows, dirty 1,407 heap pages, and write 5,772,896 WAL bytes. WAL sync is 6,000 µs and 5,760 µs. This path still rewrites the table. The 10,000 duplicate-key rows are 155,849 µs versus 177,187 µs. WAL bytes match at 5,768,796, heap pages match at 1,406, and the compare drops from 41,497 µs to 33,401 µs. That is less CPU, not less durable I/O.

At 100 rows the warm times sit inside the WAL and neither path is consistently faster. At 1,000 rows a one-row update is 10,062 µs versus 11,852 µs, and the compare is 38 µs versus 660 µs. The gap is real and still smaller than one sync. At 10,000 rows the one-row update is 12,001 µs versus 18,855 µs. Updating every row at 10,000 is 29,110 µs versus 28,774 µs, the same 574,096 WAL bytes, so the full-table case does not get faster.

The selective path reduces the rows decoded during publish and the bytes staged for the replacement. It does not reduce WAL bytes when the same pages are dirty, and it does not remove the pre-statement undo clone or the equality walk. An update of every row still rewrites those heap pages. Injected errors are not a power-loss test.

## Milestone 27 selective UPDATE undo

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. `Value` is 32 bytes and `Vec<Value>` is 24 bytes. The ignored test is `profile_milestone27`. It copies one database per size and runs three paths: the row journal, the Milestone 26 full-table undo snapshot, and the full-rewrite publish guard. The journal and the snapshot use the same selective publish. The rewrite guard is checked for the same affected-row count, checksum, WAL bytes, and page counts. It is not timed. Samples are 6 below 100,000 rows and 3 at 100,000. Cold is the first sample. Warm is the median of the rest. Rollback is one injected failure, not a median. Injected errors are not a power-loss test.

`undo_bytes` is `entries * 8 + entries * columns * 32`. `snapshot_bytes` is that cell array for every heap row, plus `rows * 8` for the row-id vector and `rows * 24` for the per-row `Vec` headers. Neither figure is process RSS. Neither includes the heap bytes of a `TEXT` value or the row-id map. Allocation counts were not instrumented, and peak RSS was not measured. A zero-match update stores no journal entries. An update of every row stores one entry per row. On this schema the all-row journal is `rows * 136` bytes and the snapshot estimate is `rows * 160` bytes. The 2,400,000 byte gap at 100,000 rows is those `Vec` headers. The real snapshot also clones the position map, which this estimate leaves out.

| Rows | Workload | Entries | Undo bytes | Snapshot bytes | WAL bytes | Heap | Index | Frames | Checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 0 | 0 | 16,000,000 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| 100,000 | one | 1 | 136 | 16,000,000 | 8,296 | 1 | 0 | 2 | 5,001,088,889 |
| 100,000 | ten | 10 | 1,360 | 16,000,000 | 8,296 | 1 | 0 | 2 | 5,001,088,880 |
| 100,000 | duplicates | 10,000 | 1,360,000 | 16,000,000 | 5,768,796 | 1,406 | 0 | 1,407 | 5,001,040,001 |
| 100,000 | key | 1 | 136 | 16,000,000 | 20,596 | 1 | 3 | 5 | 5,001,088,888 |
| 100,000 | grow | 1 | 136 | 16,000,000 | 12,396 | 2 | 0 | 3 | 5,001,089,088 |
| 100,000 | shrink | 1 | 136 | 16,000,000 | 8,296 | 1 | 0 | 2 | 5,001,088,889 |
| 100,000 | relocate | 1 | 136 | 16,000,000 | 12,396 | 2 | 0 | 3 | 5,001,092,088 |
| 100,000 | many | 100,000 | 13,600,000 | 16,000,000 | 5,772,896 | 1,407 | 0 | 1,408 | 5,000,600,000 |
| 100,000 | noop | 1 | 136 | 16,000,000 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| 100,000 | rollback | 1 | 136 | 16,000,000 | 0 durable | 0 | 0 | 0 | 5,001,088,890 restored |

The journal and the snapshot wrote the same WAL and the same heap and index pages on every workload, at every size. Checksums matched across the journal, the snapshot, and the full-rewrite guard. Rollback left the database file unchanged. Its encoded log was 8,264 bytes and was not synced. The checksum above is the 100,000-row database before that failed statement. The retry then matches the one-row update, checksum 5,001,088,889. The indexed-key update dirties fewer index pages on the smaller trees: 1 page at 100 rows (12,396 WAL bytes) and 2 pages at 1,000 and 10,000 (16,496 WAL bytes).

Warm latency, journal then snapshot, in microseconds:

| Rows | Workload | Journal | Snapshot | Undo capture | Snapshot clone | Discover | Validate | Stage | Apply |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 2,452 | 6,215 | 0 | 2,725 | 160 | 155 | 0 | 0 |
| 100,000 | one | 14,943 | 20,115 | 0 | 2,329 | 151 | 146 | 0 | 0 |
| 100,000 | ten | 15,815 | 17,366 | 3 | 2,332 | 909 | 0 | 0 | 1 |
| 100,000 | duplicates | 153,223 | 155,685 | 782 | 2,691 | 1,227 | 287 | 1,150 | 499 |
| 100,000 | key | 15,944 | 19,200 | 0 | 3,024 | 171 | 165 | 0 | 0 |
| 100,000 | grow | 16,166 | 20,042 | 1 | 2,523 | 165 | 159 | 0 | 0 |
| 100,000 | shrink | 16,027 | 20,070 | 0 | 2,618 | 273 | 159 | 0 | 0 |
| 100,000 | relocate | 14,996 | 20,821 | 0 | 2,529 | 189 | 182 | 0 | 0 |
| 100,000 | many | 211,227 | 207,646 | 5,975 | 2,668 | 6,499 | 0 | 0 | 4,203 |
| 100,000 | noop | 3,335 | 7,110 | 1 | 2,334 | 521 | 511 | 0 | 0 |
| 100,000 | rollback | 3,502 | 7,584 | 5 | 2,433 | 147 | 142 | 0 | 0 |

Discover, validate, stage, apply, and the two capture times are the untimed statement. Rollback's total is that one sample. At 100,000 rows a miss is 2,452 µs with the journal and 6,215 µs with the snapshot. Both write 0 WAL bytes. The snapshot clone is 2,725 µs and the journal stores nothing. A no-op assignment is 3,335 µs versus 7,110 µs, again with 0 WAL bytes, and the journal is 136 bytes against a 16,000,000 byte snapshot estimate. The precommit failure is 3,502 µs versus 7,584 µs. A one-row name change is 14,943 µs versus 20,115 µs, and both logs are 8,296 bytes. The durable write did not get smaller. Ten rows stay on that same 8,296 byte log.

Updating every row is 211,227 µs with the journal and 207,646 µs with the snapshot. Both dirty 1,407 heap pages and write 5,772,896 WAL bytes. Capturing 100,000 images took 5,975 µs. Cloning the table took 2,668 µs. The journal is smaller by the estimate above and slower to build, and the statement does not get faster. The 10,000 duplicate-key rows are 153,223 µs versus 155,685 µs with the same 5,768,796 WAL bytes. Growing a name onto another page and relocating a 3,200 character name both dirty 2 heap pages and write 12,396 WAL bytes. The indexed-key change writes 20,596 bytes and 3 index pages on both paths.

At 100 rows the warm updates sit inside the WAL, about 10–12 ms, and neither undo path is consistently faster. A miss is 7 µs versus 11 µs. At 1,000 rows a miss is 45 µs versus 126 µs and a no-op is 43 µs versus 98 µs. A one-row update is 11,926 µs versus 11,027 µs, inside the noise of one sync. At 10,000 rows a miss is 165 µs versus 537 µs, a no-op is 250 µs versus 590 µs, and a one-row update is 10,978 µs versus 12,063 µs. Updating every row at 10,000 is 30,042 µs versus 29,847 µs with the same 574,096 WAL bytes.

The journal removes the full-table clone from a selective `UPDATE` that does not match every row. It does not reduce WAL bytes when the same pages are dirty, and it does not remove the equality walk. An update of every row still journals the table and still rewrites those heap pages. `DELETE` and `INSERT` still snapshot the table. Peak RSS was not measured, so this run does not claim a smaller process.

## Milestone 28 selective DELETE undo

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. `Value` is 32 bytes, `Vec<Value>` is 24 bytes, and `usize` is 8 bytes. The ignored test is `profile_milestone28`. It copies one database per size and runs the row journal against the previous full-table undo snapshot. Publish is the existing unlink path on both. Samples are 6 below 100,000 rows and 3 at 100,000 for a miss, a one-row delete, a ten-row delete, and a duplicate-key delete. Cold is the first sample. Warm is the median of the rest. Deleting half the rows, every row, one row after an indexed key change, one wide text value, and the precommit failure are one sample each, so cold and warm match. Injected errors are not a power-loss test.

`undo_bytes` is `entries * 8 + entries * 8 + entries * columns * 32`: one `usize` position, one row id, and the value slots. `snapshot_bytes` is the Milestone 27 estimate, `rows * 160` on this schema. Neither figure is process RSS. Neither includes the heap bytes of a `TEXT` value or the row-id map. Allocation counts were not instrumented, and peak RSS was not measured. A miss stores no journal entries. Deleting every row stores one entry per row. On this schema that journal is `rows * 144` bytes and the snapshot estimate is `rows * 160`. The 1,600,000 byte gap at 100,000 rows is the snapshot's per-row `Vec` header, which the journal does not keep for survivors.

| Rows | Workload | Entries | Undo bytes | Snapshot bytes | WAL bytes | Heap | Index | Frames | Checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 0 | 0 | 16,000,000 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| 100,000 | one | 1 | 144 | 16,000,000 | 36,996 | 1 | 6 | 9 | 5,001,088,887 |
| 100,000 | ten | 10 | 1,440 | 16,000,000 | 94,396 | 1 | 20 | 23 | 5,001,088,770 |
| 100,000 | duplicates | 10,000 | 1,440,000 | 16,000,000 | 7,749,096 | 1,407 | 403 | 1,890 | 4,501,070,001 |
| 100,000 | half | 50,000 | 7,200,000 | 16,000,000 | 6,314,096 | 1 | 42 | 1,540 | 3,750,550,000 |
| 100,000 | all | 100,000 | 14,400,000 | 16,000,000 | 12,275,496 | 0 | 1 | 2,994 | 0 |
| 100,000 | updated | 1 | 144 | 16,000,000 | 45,196 | 1 | 7 | 11 | 5,001,088,887 |
| 100,000 | wide | 1 | 144 | 16,000,000 | 61,596 | 1 | 8 | 15 | 5,001,088,887 |
| 100,000 | rollback | 1 | 144 | 16,000,000 | 0 durable | 0 | 0 | 0 | 5,001,088,890 restored |

The journal and the snapshot wrote the same WAL and the same heap, index, and frame counts on every workload, at every size. Checksums matched. Rollback left the logical table unchanged. Its encoded log was 49,264 bytes and was not a synced commit. The checksum above is the 100,000-row database before that failed statement. The retry then matches the one-row delete, checksum 5,001,088,885. A delete of every row frees the heap pages, so the heap counter is 0 and those pages are included in the 2,994 frames as free pages. The half delete likewise frees pages: one heap page remains in the counter, 42 index pages are dirty, and the frame count is 1,540.

Warm latency, journal then snapshot, in microseconds. `positions` is the in-memory row-id map rebuild on the untimed statement. `apply` includes that rebuild and the survivor move.

| Rows | Workload | Journal | Snapshot | Undo capture | Snapshot clone | Discover | Validate | Apply | Positions |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 23,765 | 29,774 | 0 | 2,679 | 318 | 312 | 23 | 0 |
| 100,000 | one | 31,000 | 35,448 | 28 | 2,580 | 468 | 448 | 2,809 | 1,074 |
| 100,000 | ten | 38,919 | 43,107 | 36 | 2,644 | 1,995 | 0 | 2,721 | 1,012 |
| 100,000 | duplicates | 3,438,808 | 3,477,783 | 1,237 | 3,049 | 1,864 | 515 | 3,024 | 745 |
| 100,000 | half | 20,420,738 | 20,388,899 | 3,757 | 2,989 | 1,126 | 0 | 2,102 | 513 |
| 100,000 | all | 1,454,122 | 1,454,278 | 7,298 | 2,695 | 1,356 | 0 | 1,627 | 0 |
| 100,000 | updated | 30,758 | 35,885 | 1 | 2,728 | 455 | 358 | 2,570 | 867 |
| 100,000 | wide | 31,382 | 35,399 | 1 | 2,766 | 427 | 316 | 2,752 | 910 |
| 100,000 | rollback | 23,417 | 23,639 | 44 | 2,584 | 206 | 199 | 2,537 | 885 |

At 100,000 rows a miss is 23,765 µs with the journal and 29,774 µs with the snapshot. Both write 0 WAL bytes. The snapshot clone is 2,679 µs and the journal stores nothing. Publish still builds a table image, about 12,200 µs on the journal and 12,933 µs on the snapshot, because a miss does not change row ids and the editor compares the stored rows. A one-row delete is 31,000 µs versus 35,448 µs, and both logs are 36,996 bytes: one heap page, six index pages, and nine frames. Rebuilding the row-id map for the survivors is 1,074 µs. The durable write did not get smaller. The wide-text delete journals the original short name in the byte estimate, 144 bytes, while the 200-character value is real heap memory outside that estimate. Its log is 61,596 bytes on both paths. Deleting the row after an indexed key change is 30,758 µs versus 35,885 µs, with 45,196 WAL bytes and seven index pages.

Deleting 10,000 duplicate keys is 3,438,808 µs versus 3,477,783 µs. Both write 7,749,096 WAL bytes. Capturing the journal is 1,237 µs and the clone is 3,049 µs. Publish is about 3.35 seconds on both, which is the unlink, not the undo. Deleting half the rows is 20,420,738 µs versus 20,388,899 µs. Publish is about 20.3 seconds on both. The journal is larger to capture, 3,757 µs versus 2,989 µs, and the statement does not get faster. Deleting every row is 1,454,122 µs versus 1,454,278 µs, both writing 12,275,496 WAL bytes. Capturing 100,000 images took 7,298 µs. Cloning the table took 2,695 µs. The journal is smaller by the estimate above and slower to build.

At 100 rows the warm deletes sit inside the WAL, about 9–13 ms, and neither undo path is consistently faster. A miss is 26 µs versus 29 µs. At 1,000 rows a miss is 505 µs versus 275 µs, inside the noise of a statement that writes nothing, and the clone is 63 µs. A one-row delete is 12,056 µs versus 11,999 µs. At 10,000 rows a miss is 2,248 µs versus 2,581 µs and a one-row delete is 13,005 µs versus 11,999 µs, the same 28,796 WAL bytes. Deleting every row at 10,000 is 66,102 µs versus 66,058 µs with the same 1,205,496 WAL bytes.

The journal removes the full-table clone from a `DELETE` that does not remove every row. It does not reduce WAL bytes when the same pages are dirty, and it does not remove the equality walk or the per-row unlink. A miss still pays for a table image. `INSERT` still snapshots the table. Peak RSS was not measured, so this run does not claim a smaller process.

## Milestone 29 selective DELETE publication

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. `Value` is 32 bytes, `Vec<Value>` is 24 bytes, and `usize` is 8 bytes. The ignored test is `profile_milestone29`. It copies one database per size and runs selective publication against the previous full table image. Both sides use the Milestone 28 row journal. Samples are 6 below 100,000 rows and 3 at 100,000 for a miss, a one-row delete, a ten-row delete, and a duplicate-key delete. Cold is the first sample. Warm is the median of the rest. Deleting half the rows, every row, one row after an indexed key change, one wide text value, and the precommit failure are one sample each, so cold and warm match. Injected errors are not a power-loss test.

`id_bytes` is the deleted row-id list, `count * 8`. `image_bytes` is the Milestone 27 survivor estimate, `survivors * 160` on this schema, which is what the full-image path clones. Neither figure is process RSS. Neither includes the heap bytes of a `TEXT` value. Allocation counts were not instrumented, and peak RSS was not measured. A miss has an empty id list and still decodes every stored row. Deleting every row has no survivors, so `image_bytes` is 0 and `id_bytes` is `rows * 8`.

The durable results below are the selective path. The full-image path wrote the same WAL, the same heap, index, and frame counts, and the same checksum on every workload, at every size.

| Rows | Workload | Id bytes | Image bytes | WAL bytes | Heap | Index | Frames | Checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 0 | 16,000,000 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| 100,000 | one | 8 | 15,999,840 | 36,996 | 1 | 6 | 9 | 5,001,088,887 |
| 100,000 | ten | 80 | 15,998,400 | 94,396 | 1 | 20 | 23 | 5,001,088,770 |
| 100,000 | duplicates | 80,000 | 14,400,000 | 7,749,096 | 1,407 | 403 | 1,890 | 4,501,070,001 |
| 100,000 | half | 400,000 | 8,000,000 | 6,314,096 | 1 | 42 | 1,540 | 3,750,550,000 |
| 100,000 | all | 800,000 | 0 | 12,275,496 | 0 | 1 | 2,994 | 0 |
| 100,000 | updated | 8 | 15,999,840 | 45,196 | 1 | 7 | 11 | 5,001,088,887 |
| 100,000 | wide | 8 | 15,999,840 | 61,596 | 1 | 8 | 15 | 5,001,088,887 |
| 100,000 | rollback | 8 | 15,999,840 | 0 durable | 0 | 0 | 0 | 5,001,088,890 restored |

Those WAL byte counts, page counts, and checksums are the Milestone 28 numbers. Rollback's encoded log was 49,264 bytes and was not a synced commit. The retry then matches the one-row delete, checksum 5,001,088,885. A delete of every row frees the heap pages, so the heap counter is 0 and those pages are included in the 2,994 frames as free pages.

Warm latency, selective publication then the full image, in microseconds. `prepare` is the in-memory audit, or the table-image clone on the full path. `publish` is the editor. `compare` is the stored-row decode. A real delete compares nothing. A miss compares every row.

| Rows | Workload | Selective | Full image | Prepare | Full prepare | Publish | Full publish | Compare | WAL bytes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 15,751 | 21,733 | 1,854 | 4,388 | 12,674 | 12,024 | 8,028 | 0 |
| 100,000 | one | 19,952 | 28,929 | 1,900 | 5,075 | 1,517 | 3,650 | 0 | 36,996 |
| 100,000 | ten | 21,855 | 36,821 | 1,771 | 4,596 | 2,919 | 11,548 | 0 | 94,396 |
| 100,000 | duplicates | 416,074 | 3,144,914 | 1,699 | 3,641 | 333,987 | 3,057,729 | 0 | 7,749,096 |
| 100,000 | half | 1,176,450 | 17,688,489 | 900 | 2,304 | 1,106,131 | 17,616,282 | 0 | 6,314,096 |
| 100,000 | all | 1,396,383 | 1,383,504 | 0 | 2 | 1,279,153 | 1,271,095 | 0 | 12,275,496 |
| 100,000 | updated | 18,832 | 28,958 | 1,917 | 4,536 | 1,631 | 3,716 | 0 | 45,196 |
| 100,000 | wide | 19,742 | 28,031 | 1,821 | 4,427 | 1,917 | 3,958 | 0 | 61,596 |
| 100,000 | rollback | 12,048 | 20,894 | — | — | — | — | — | 0 durable |

At 100,000 rows a miss is 15,751 µs selective and 21,733 µs with the full image. Both write 0 WAL bytes and both decode all 100,000 stored rows, about 8,028 µs. Building the image is 4,388 µs against an audit of 1,854 µs. The estimated image is 16,000,000 bytes. The selective path does not allocate it. The id list is empty. The decode remains.

A one-row delete is 19,952 µs versus 28,929 µs, and both logs are 36,996 bytes: one heap page, six index pages, and nine frames. The editor is 1,517 µs versus 3,650 µs. The durable write did not get smaller. The statement is still mostly the log sync. Rebuilding the row-id map is 952 µs on the untimed statement. The wide-text delete journals 144 bytes by the estimate while the 200-character value sits outside that estimate. Its log is 61,596 bytes on both paths.

Deleting 10,000 duplicate keys is 416,074 µs versus 3,144,914 µs. Both write 7,749,096 WAL bytes, 1,407 heap pages, 403 index pages, and 1,890 frames. The editor is 333,987 µs versus 3,057,729 µs. The image estimate is 14,400,000 bytes and preparing it is only 3,641 µs, so that gap is not the clone. The full-image editor searches the keep set from the end once per removed row. The selective path resolves each stored id once and unlinks from the highest index. The 334 ms that remains is unlink and index maintenance.

Deleting half the rows is 1,176,450 µs versus 17,688,489 µs. Both write 6,314,096 WAL bytes, one heap page, 42 index pages, and 1,540 frames. Prepare is 900 µs versus 2,304 µs. Publish is 1,106,131 µs versus 17,616,282 µs. The same keep-set search explains the full-image time. The selective publish is still 1.1 seconds, and that is the per-row unlink.

Deleting every row is 1,396,383 µs versus 1,383,504 µs. Both write 12,275,496 WAL bytes. There are no survivors to clone, and both editors unlink from the end, so the times match. Publish is about 1.28 seconds on both. That is the remaining bottleneck: one heap unlink and its index updates per row, then the same dirty pages as before.

At 100 rows the warm deletes sit inside the WAL, about 10–12 ms, and the image clone is a few microseconds. A miss is 30 µs versus 38 µs and compares all 100 rows. At 1,000 rows a miss is 387 µs versus 396 µs. A one-row delete is 12,066 µs versus 12,957 µs with the same 28,796 WAL bytes. At 10,000 rows a miss is 1,460 µs versus 2,102 µs. A one-row delete is 11,783 µs versus 12,086 µs with the same 28,796 WAL bytes. Deleting half of 10,000 rows is 47,323 µs versus 194,636 µs with the same 758,596 WAL bytes. Deleting every row at 10,000 is 61,803 µs versus 61,917 µs with the same 1,205,496 WAL bytes.

The selective path removes the survivor-image clone and the repeated keep-set scan. It does not reduce WAL bytes when the same pages are dirty. A miss still decodes every stored row. Deleting every row still unlinks every row. `INSERT` still snapshots the table. Peak RSS was not measured, so this run does not claim a smaller process.

## Milestone 30 batched DELETE unlinking

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone30`. It copies one database per size and runs the batched unlink against the Milestone 29 per-row loop. Both sides use the same deleted-id list, the same undo journal, and the same page-image WAL. Samples are 6 below 100,000 rows and 3 at 100,000 for a miss, a one-row delete, a ten-row delete, and a duplicate-key delete. Cold is the first sample. Warm is the median of the rest. Deleting half the rows, every row, one row after an indexed key change, one wide text value, and the precommit failure are one sample each. Injected errors are not a power-loss test.

`id_bytes` is the deleted row-id list, `count * 8`. `temp_bytes` for two or more deleted rows is `rows * 17 + count * 32`: a one-byte mark per stored row, 16 bytes of cloned id and link, and 32 bytes of key scratch per deleted row for two indexes. A miss and a one-row delete do not clone those chains, so their estimate is `count * 32` (0 for a miss, 32 for one row). It does not include the next-survivor index, a decoded value after its key is taken, allocator slack, or process RSS. `batched_postings` is the number of index pairs removed in one leaf rewrite. `singles` is the number removed by the single-entry delete. Their sum is `count` times the number of indexes that store a key for that row. The per-row path reports every posting as a single. One deleted row uses that path.

The sealed database matched on every workload: same WAL bytes, same heap, index, and frame counts, and same checksum. Those durable figures are the Milestone 29 numbers.

| Rows | Workload | Id bytes | Temp bytes | WAL bytes | Heap | Index | Frames | Checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 0 | 0 | 0 | 0 | 0 | 0 | 5,001,088,890 |
| 100,000 | one | 8 | 32 | 36,996 | 1 | 6 | 9 | 5,001,088,887 |
| 100,000 | ten | 80 | 1,700,320 | 94,396 | 1 | 20 | 23 | 5,001,088,770 |
| 100,000 | duplicates | 80,000 | 2,020,000 | 7,749,096 | 1,407 | 403 | 1,890 | 4,501,070,001 |
| 100,000 | half | 400,000 | 3,300,000 | 6,314,096 | 1 | 42 | 1,540 | 3,750,550,000 |
| 100,000 | all | 800,000 | 4,900,000 | 12,275,496 | 0 | 1 | 2,994 | 0 |
| 100,000 | updated | 8 | 32 | 45,196 | 1 | 7 | 11 | 5,001,088,887 |
| 100,000 | wide | 8 | 32 | 61,596 | 1 | 8 | 15 | 5,001,088,887 |
| 100,000 | rollback | 8 | — | 0 durable | 0 | 0 | 0 | 5,001,088,890 restored |

Rollback's encoded log was not a synced commit. The retry checksum is 5,001,088,885, the same one-row result as Milestone 29.

Warm latency, batched unlink then the per-row loop, in microseconds. `heap` is slot removal, link repair, the catalog head, and releasing empty pages. `index` is posting removal. `publish` is the whole editor, including those two and sealing dirty pages. `compare` is the stored-row decode. A real delete compares nothing. A miss compares every row.

| Rows | Workload | Batch | Per row | Heap | Per-row heap | Index | Per-row index | Publish | Per-row publish | Batched postings | Singles | WAL bytes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | miss | 16,112 | 16,130 | 0 | 0 | 0 | 0 | 12,917 | 13,191 | 0 | 0 | 0 |
| 100,000 | one | 18,862 | 19,968 | 48 | 52 | 105 | 110 | 1,496 | 1,616 | 0 | 2 | 36,996 |
| 100,000 | ten | 20,853 | 22,182 | 680 | 263 | 653 | 650 | 3,404 | 2,981 | 10 | 10 | 94,396 |
| 100,000 | duplicates | 206,318 | 393,425 | 2,905 | 161,384 | 21,450 | 50,908 | 138,069 | 323,068 | 18,666 | 1,334 | 7,749,096 |
| 100,000 | half | 395,651 | 1,152,486 | 17,759 | 782,024 | 248,161 | 232,694 | 337,589 | 1,092,107 | 34,786 | 65,214 | 6,314,096 |
| 100,000 | all | 539,959 | 1,371,010 | 16,512 | 757,180 | 287,613 | 365,429 | 443,491 | 1,273,094 | 100,648 | 99,352 | 12,275,496 |
| 100,000 | updated | 19,982 | 19,969 | 49 | 47 | 128 | 129 | 1,623 | 1,661 | 0 | 2 | 45,196 |
| 100,000 | wide | 22,706 | 20,997 | 93 | 91 | 212 | 208 | 2,066 | 1,945 | 0 | 2 | 61,596 |
| 100,000 | rollback | 13,427 | 13,612 | — | — | — | — | — | — | — | — | 0 durable |

At 100,000 rows the per-row half delete spends 782,024 µs in the heap editor and 232,694 µs in the index. The heap time is the repeated `Vec::remove` on the id and link vectors and the scan of every remaining link inside each page-free check. The batched path does that work once: 17,759 µs. The index time stays 248,161 µs because 65,214 of the 100,000 postings still go through the single-entry delete. Those are the postings that would drop a leaf below its minimum, so each one still borrows or merges. Grouping the other 34,786 does not move the total. Publish falls from 1,092,107 µs to 337,589 µs. The statement falls from 1,152,486 µs to 395,651 µs. The log is still 6,314,096 bytes.

Deleting 10,000 duplicate keys is the case the leaf group helps. 18,666 postings share a leaf and stay at the minimum, and 1,334 still delete one at a time. Index time falls from 50,908 µs to 21,450 µs. Heap time falls from 161,384 µs to 2,905 µs. Publish falls from 323,068 µs to 138,069 µs. Both logs are 7,749,096 bytes.

Deleting every row falls from 1,371,010 µs to 539,959 µs. Heap time falls from 757,180 µs to 16,512 µs. Index time falls from 365,429 µs to 287,613 µs, with 99,352 postings still on the single-entry path because emptying a leaf merges it. The log stays 12,275,496 bytes.

A miss is 16,112 µs on the batched path and 16,130 µs on the per-row path. It still decodes all 100,000 stored rows, about 8,504 µs, and writes no log. A one-row delete stays on the per-row unlink, so there is no chain clone and no grouped posting. It is 18,862 µs versus 19,968 µs, and the heap pass is 48 µs versus 52 µs. Both remove 2 postings one at a time. The log sync is about 12 ms on both. Ten rows are 20,853 µs versus 22,182 µs, both logs 94,396 bytes. The ten-row heap pass is 680 µs versus 263 µs: cloning both chains shows up before the repeated shifts dominate.

At 10,000 rows a half delete is 46,248 µs versus 45,985 µs, and publish is 30,630 µs versus 31,509 µs. The per-row heap pass is 8,834 µs there, so the vector shift has not yet dominated. Deleting every row is 50,439 µs versus 63,027 µs. The duplicate-key delete is 30,788 µs versus 33,019 µs. A miss decodes all 10,000 rows and writes nothing. At 1,000 rows and at 100 rows the warm deletes sit inside the log sync, about 11–17 ms, on both paths.

The batched path removes the repeated vector shift and the repeated free-page scan, and it rewrites a leaf once when several postings can leave without a borrow. One deleted row keeps the per-row unlink. The batched path does not reduce WAL bytes. It does not skip the no-match decode. A unique-key delete that empties leaves is still limited by one borrow or merge per posting. Peak RSS was not measured.

## Milestone 31 bulk B+ tree deletion

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone31`. It builds one database per size with seed `0x3100`: ids `1..=rows`, `active` true on even ids, `bucket` NULL when `(seed + id)` is divisible by 5 and otherwise that value modulo 4, and an 80-byte name when `id` is divisible by 11. Indexes are `idx_users_id` and `idx_users_bucket`. At 100,000 rows each workload is the median of three fresh database copies. Smaller sizes are one sample. `bulk` is the new delete. `grouped` forces the Milestone 30 path, which still rewrites a safe leaf once and deletes an underflowing posting one at a time. Both use the same heap unlink. This is not a power-loss test.

`scratch` is the key-vector bytes the span rebuild records. It is not the heap estimate from Milestone 30 and it is not RSS. `descents` counts root-to-leaf walks. A one-row delete never rebuilds a span. At 100 rows the tree is one leaf, so the span rebuild does not run and the two paths seal the same log.

Times are microseconds. Index time is posting removal. The log size is the bytes appended for that statement. Structural counters matched on every 100,000-row sample.

| rows | workload | bulk | grouped | bulk index | grouped index | bulk singles | grouped singles | spans | merges | bulk WAL | grouped WAL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | one | 20,174 | 19,892 | 14 | 12 | 1 | 1 | 0 | 0 | 36,996 | 36,996 |
| 100,000 | sparse | 202,760 | 221,556 | 16,296 | 36,191 | 0 | 10,265 | 1 | 471 | 9,581,796 | 9,565,396 |
| 100,000 | duplicates | 231,225 | 238,731 | 31,279 | 40,149 | 0 | 10,459 | 2 | 628 | 10,262,396 | 10,237,796 |
| 100,000 | half | 232,966 | 354,010 | 68,162 | 206,559 | 0 | 55,603 | 2 | 825 | 7,101,296 | 6,170,596 |
| 100,000 | all | 387,365 | 482,039 | 132,146 | 227,037 | 246 | 80,255 | 2 | 1,414 | 12,168,896 | 12,168,896 |
| 100,000 | alternating | 295,703 | 405,097 | 65,986 | 183,745 | 61 | 50,133 | 2 | 982 | 11,537,496 | 10,897,896 |
| 100,000 | consecutive | 147,911 | 174,085 | 35,396 | 87,274 | 0 | 24,096 | 2 | 529 | 4,555,196 | 3,136,596 |
| 100,000 | wide | 245,363 | 270,558 | 17,280 | 41,492 | 69 | 12,146 | 2 | 768 | 12,181,196 | 12,164,796 |

At 100,000 rows the half delete is the case Milestone 30 left on the single-entry path. The grouped run still removes 55,603 postings one at a time and spends 206,559 µs in the index. The span rebuild removes those with 0 single-entry deletes, 2 span repacks, 825 leaf merges, 11 internal merges, and 1 root contraction. Index time is 68,162 µs. The statement is 232,966 µs versus 354,010 µs. Heap time stays about 16.6 ms on both, because the heap unlink did not change. The log grows from 6,170,596 bytes to 7,101,296 bytes: the repacked leaves and the new internal pages are dirty. That extra 930,700 bytes does not cancel the index saving. The id index stays height 3, 396 pages versus 397. The half delete allocates 4 index pages and frees 840.

The half delete still walks from the root 89,911 times. That check is what keeps a missing posting ordered before a later corrupt page. The scratch for the span is 1,924,176 bytes. The grouped path does not build it.

Alternating keys drop the id index from height 3 and 397 pages to height 2 and 198 pages, because the survivors are packed to the leaf maximum instead of the 127-entry split shape. Index time is 65,986 µs versus 183,745 µs. The log grows from 10,897,896 bytes to 11,537,496 bytes. The statement is 295,703 µs versus 405,097 µs. Consecutive keys keep height 3 and 594 pages. Index time is 35,396 µs versus 87,274 µs, and the log grows from 3,136,596 bytes to 4,555,196 bytes. The statement is 147,911 µs versus 174,085 µs, so the larger log gives back about half of the index saving and the statement is still faster.

Deleting every row spends 132,146 µs in the index versus 227,037 µs. Both logs are 12,168,896 bytes and both trees are empty. 246 postings still use the single-entry delete, against 80,255 on the grouped path. Those are the postings left after the verified prefix stops at the last entries of a leaf. The rebuild frees 1,429 pages and allocates none. Sparse NULL-bucket rows and duplicate bucket 1 are faster in the index and write a slightly larger log. The wide-name delete is 17,280 µs versus 41,492 µs in the index.

A one-row delete does not rebuild a span. At 100,000 rows it is 20,174 µs versus 19,892 µs, with the same 36,996-byte log. The difference is the log sync, not the tree. At 100 rows every workload is inside that sync, about 11–13 ms, and the logs match. At 1,000 rows the half index is 1,005 µs versus 1,787 µs. At 10,000 rows it is 5,204 µs versus 18,251 µs, and the statement is 30,381 µs versus 41,899 µs.

The span rebuild is not faster on every workload. A one-row delete and a 100-row tree do not use it. On a consecutive delete the extra log is large enough to return part of the index saving, and the statement is still faster. No cost heuristic was added to skip the rebuild. Peak RSS was not measured. The no-match decode is unchanged.

## Milestone 32 ordered posting-run verification

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone32`. It uses the same seed `0x3100` database as Milestone 31. `ordered` checks a run that only moves forward or only backward along the leaf chain after one root descent. `descent` forces the Milestone 31 check, one root descent per posting, and then the same span rebuild. `grouped` is still the Milestone 30 path. At 100,000 rows each workload is the median of three fresh copies. Smaller sizes are one sample. This is not a power-loss test.

Publish hands the editor postings in descending stored order. An `id` delete is therefore strictly decreasing and takes the leaf-chain walk. A `bucket` delete in that same row order is not monotonic, so it still descends once per posting. The walk does not sort the request. A repeated pair or a change of direction stops the ordered prefix and uses the older check, so the first rejected posting stays the one the older check would reject.

The log, the span count, the page counts, and the checksum match the Milestone 31 bulk column. The chain walk does not allocate a second rebuild and does not change the scratch vector. `descents` is the root-to-leaf count. `verified` is the postings accepted by the chain walk. `fallback` is the postings accepted by a descent. Times are microseconds.

| rows | workload | ordered | descent | ordered index | descent index | descents | old descents | verified | fallback | WAL | checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | one | 20,100 | 19,708 | 14 | 15 | 1 | 1 | 0 | 0 | 36,996 | 5001482560 |
| 100,000 | sparse | 200,463 | 209,251 | 5,521 | 17,831 | 2 | 19,941 | 19,965 | 0 | 9,581,796 | 4001250051 |
| 100,000 | duplicates | 217,071 | 239,117 | 9,219 | 33,832 | 4 | 39,755 | 39,903 | 0 | 10,262,396 | 4001190051 |
| 100,000 | half | 210,462 | 242,871 | 41,517 | 72,273 | 40,000 | 89,911 | 49,999 | 39,999 | 7,101,296 | 3750796338 |
| 100,000 | all | 342,302 | 408,081 | 77,901 | 142,550 | 80,248 | 180,070 | 99,949 | 79,754 | 12,168,896 | 0 |
| 100,000 | alternating | 362,133 | 354,937 | 72,767 | 81,206 | 40,032 | 89,943 | 49,949 | 39,877 | 11,537,496 | 2500711280 |
| 100,000 | consecutive | 149,283 | 163,974 | 23,965 | 40,398 | 20,000 | 44,892 | 24,999 | 19,999 | 4,555,196 | 4688644540 |
| 100,000 | wide | 268,277 | 274,075 | 15,204 | 20,503 | 7,343 | 16,406 | 9,074 | 7,203 | 12,181,196 | 4546235365 |

The half delete is the number Milestone 31 left at 89,911 descents. The id index is one decreasing run, so those 49,999 postings are accepted after one descent and 1,335 sibling steps. The bucket index is not in posting order, and its 39,999 postings still descend. Index time falls from 72,273 µs to 41,517 µs. The statement falls from 242,871 µs to 210,462 µs. The log stays 7,101,296 bytes, the id index stays height 3 and 396 pages, and the checksum stays 3750796338. The span rebuild is unchanged: 2 spans, 825 leaf merges, 11 internal merges, 1 root contraction, 4 pages allocated, 840 freed, and 1,924,176 scratch bytes.

Sparse NULL-bucket rows and duplicate bucket 1 are monotonic on both indexes. Their descents fall to 2 and 4. Index time falls from 17,831 µs to 5,521 µs, and from 33,832 µs to 9,219 µs. The statements are faster by about 9 ms and 22 ms. The logs are unchanged.

Delete-all still has 246 single-entry removals, the same tail the span rebuild left. Its descents fall from 180,070 to 80,248, and index time falls from 142,550 µs to 77,901 µs. The statement falls from 408,081 µs to 342,302 µs. Both logs stay 12,168,896 bytes.

Consecutive keys cut index time from 40,398 µs to 23,965 µs and the statement from 163,974 µs to 149,283 µs. Alternating keys cut the index from 81,206 µs to 72,767 µs, and the statement in this run is 362,133 µs against 354,937 µs, so the statement does not get faster. The wide-name delete cuts the index from 20,503 µs to 15,204 µs and the statement from 274,075 µs to 268,277 µs. A one-row delete never walks a chain. At 100,000 rows it is 20,100 µs against 19,708 µs, with the same 36,996-byte log.

At 100 rows the tree is one leaf. The chain walk does not run, the two verifiers seal the same log, and the statement is the log sync, about 11–13 ms. At 1,000 rows the half index is 463 µs against 706 µs, and the statement is 15,010 µs against 13,609 µs, so the statement does not get faster at that size. At 10,000 rows the half index is 3,130 µs against 5,387 µs, and the statement is 30,068 µs against 31,719 µs.

The chain walk is not a cost heuristic. It does not choose between the span rebuild and the grouped path. Peak RSS was not measured. A request that is not monotonic still pays one descent per posting, and that is the remaining index cost on a half delete.

## Milestone 33 posting presence map

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone33`. It uses seed `0x3100`. `presence` is the default verifier. `chain` forces the Milestone 32 leaf-chain check and one descent per posting when the request changes direction. At 100,000 rows each workload is the median of three fresh copies. Smaller sizes are one sample. This is not a power-loss test.

The map is a hash table of every live `(key, row id)` pair and the leaf that holds it. It is built only when the request is not one monotonic run and the postings that would each descend would read more pages than the largest tree of this height. An internal page has at most 204 children, so a height-3 tree is bounded at 41,821 pages. A monotonic run still uses the Milestone 32 walk. One posting, ten postings, and the wide-name delete at height 3 stay on that walk. The map is dropped before the rebuild writes, and the rebuild validates the tree again. `map_bytes` is the payload of those pairs, `size_of<(i64, u64)>() + size_of<u32>()` times the posting count. It is not peak resident memory. Times are microseconds.

| rows | workload | presence | chain | presence index | chain index | descents | chain descents | map bytes | WAL | checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100,000 | one | 20,509 | 19,368 | 16 | 15 | 1 | 1 | 0 | 36,996 | 5001482560 |
| 100,000 | ten | 59,567 | 59,104 | 3,675 | 3,670 | 9 | 9 | 0 | 2,045,996 | 5001482470 |
| 100,000 | buckets | 236,435 | 260,198 | 16,145 | 39,951 | 155 | 40,032 | 1,597,580 | 10,906,096 | 3000897535 |
| 100,000 | half | 182,812 | 206,933 | 17,502 | 42,195 | 2 | 40,000 | 1,600,000 | 7,101,296 | 3750796338 |
| 100,000 | sparse | 193,519 | 194,329 | 5,640 | 5,719 | 2 | 2 | 0 | 9,581,796 | 4001250051 |
| 100,000 | alternating | 250,558 | 272,233 | 16,588 | 40,051 | 156 | 40,032 | 1,597,540 | 11,537,496 | 2500711280 |
| 100,000 | consecutive | 123,084 | 133,875 | 11,194 | 21,934 | 2 | 20,000 | 1,600,000 | 4,555,196 | 4688644540 |
| 100,000 | all | 287,960 | 338,207 | 29,585 | 79,774 | 495 | 80,248 | 1,595,080 | 12,168,896 | 0 |
| 100,000 | wide | 244,867 | 243,935 | 13,469 | 13,762 | 7,343 | 7,343 | 0 | 12,181,196 | 4546235365 |

The half delete is the case Milestone 32 left at 40,000 descents. The `id` index is one decreasing run. The `bucket` index is not. One map of the 80,000 live bucket postings accepts the 39,999 requested pairs after the initial descent. Descents fall from 40,000 to 2. Index time falls from 42,195 µs to 17,502 µs, of which 2,037 µs builds the map and 1,303 µs looks the requests up. The statement falls from 206,933 µs to 182,812 µs. The log stays 7,101,296 bytes, the checksum stays 3750796338, and the span rebuild is unchanged. Leaf decodes fall from 46,761 to 8,644 even though the rebuild validates the tree a second time.

`bucket = 0 OR bucket = 1` and the alternating delete also build one map. Their descents fall from about 40,000 to 155 and 156. The statements fall by about 24 ms and 22 ms. The remaining descents are same-leaf groups that finish before the request underflows a leaf and reaches the map. Delete-all falls from 80,248 descents to 495, and index time from 79,774 µs to 29,585 µs. The statement falls from 338,207 µs to 287,960 µs. The log stays 12,168,896 bytes. Consecutive keys fall from 20,000 descents to 2, and the statement from 133,875 µs to 123,084 µs.

Sparse NULL-bucket rows stay on the monotonic walk. Descents stay at 2, and the statement is 193,519 µs against 194,329 µs. A one-row delete does not build a map. At 100,000 rows it is 20,509 µs against 19,368 µs, with the same 36,996-byte log. Ten keys stay at 9 descents on both paths. The wide-name delete at height 3 is below the fanout bound, so it does not build a map. Its statement is 244,867 µs against 243,935 µs. At 10,000 rows that same delete is height 2, the map runs, and the statement is 32,226 µs against 34,224 µs.

At 100 rows the tree is one leaf. The map does not run, and the statement is the log sync, about 10–14 ms. At 1,000 rows the half index is 241 µs against 470 µs, and the statement is 15,313 µs against 13,741 µs, so the statement does not get faster at that size. The 1,000-row `bucket = 0 OR bucket = 1` index is 698 µs against 681 µs. The map's build cost is visible there, and the statement is not faster.

The map does not change the log or the scratch vector of the span rebuild. Peak RSS was not measured. A request below the fanout bound still descends once per posting.

## Milestone 34 early presence-map planning

16 Nov 2026, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone34`. It uses seed `0x3100`, the same rows as Milestone 33. `early` is the default planner. `late` keeps Milestone 33's map, which starts only after the first underflow and still uses the full-fanout ceiling. `chain` forces the Milestone 32 leaf-chain check. At 100,000 rows each workload is the median of three fresh copies. Smaller sizes are one sample. Page counters below are from the first copy. Statement time includes the log sync. This is not a power-loss test.

The planner runs before the per-leaf loop. It reads the root. A height-2 tree costs one plus the root's child count. A height-3 tree reads each of those children and adds their child counts. Both counts are exact, and neither reads a leaf to make the decision. On every 100,000-row plan the estimate was 634 pages and the later walk read 634 pages. A height of four or more fills the unseen levels to this tree's fanout; that ceiling was not the production shape in this profile. A height-1 tree is one page and stays on the grouping loop. A monotonic run stays on the Milestone 32 walk. An exact estimate also requires the request to be longer than the page count, so ten keys against 634 pages estimate the tree and then decline.

`build_us` is one structural walk plus filling the map. The walk records each leaf's parent path and entry count. `path_us` is the in-memory reserve and insert after that walk, and it is included in `build_us`. `member_us` is the request lookup. `validations` counts calls to the structural walk, including the check of the rebuilt tree. `map_bytes` is `size_of<(i64, u64)>() + size_of<u32>()` times the live posting count of the planned index. It is not peak resident memory. Peak RSS was not measured.

| workload | early | late | chain | early index | late index | descents | late descents | build | validations | late validations | map bytes | WAL | checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| one | 20,082 | 21,469 | 21,140 | 18 | 16 | 1 | 1 | 0 | 0 | 0 | 0 | 36,996 | 5001482560 |
| ten | 62,409 | 62,786 | 61,832 | 3,998 | 4,009 | 9 | 9 | 0 | 4 | 4 | 0 | 2,045,996 | 5001482470 |
| buckets | 244,513 | 247,121 | 269,690 | 16,264 | 16,702 | 63 | 155 | 1,645 | 5 | 5 | 1,600,000 | 10,906,096 | 3000897535 |
| half | 191,538 | 191,849 | 218,727 | 17,835 | 18,239 | 1 | 2 | 2,109 | 4 | 5 | 1,600,000 | 7,101,296 | 3750796338 |
| sparse | 206,033 | 203,781 | 204,019 | 6,056 | 5,884 | 2 | 2 | 0 | 2 | 2 | 0 | 9,581,796 | 4001250051 |
| alternating | 261,541 | 261,253 | 286,092 | 17,274 | 17,446 | 63 | 156 | 2,007 | 5 | 5 | 1,600,000 | 11,537,496 | 2500711280 |
| consecutive | 128,108 | 127,675 | 139,302 | 10,928 | 11,395 | 1 | 2 | 1,878 | 4 | 5 | 1,600,000 | 4,555,196 | 4688644540 |
| all | 305,296 | 306,560 | 350,484 | 32,399 | 31,247 | 248 | 495 | 2,151 | 5 | 5 | 1,600,000 | 12,168,896 | 0 |
| wide | 252,550 | 256,334 | 252,718 | 10,874 | 13,995 | 71 | 7,343 | 2,120 | 5 | 4 | 1,600,000 | 12,181,196 | 4546235365 |

Times are microseconds. Chain descents at 100,000 rows stay 1, 9, 40,032, 40,000, 2, 40,032, 20,000, 80,248, and 7,343.

The wide-name delete is the case the old ceiling missed. The bucket index is height 3 and holds 634 pages, under the 41,821-page fanout bound, so the late path builds no map and descends 7,343 times. The early plan accepts 7,272 pairs from the one structural walk. Descents fall to 71. Sixty-nine of those are single-entry deletes, and `delete` still walks from the root for each one. Index time falls from 13,995 µs to 10,874 µs. The build is 2,120 µs, of which 1,542 µs inserts the pairs, and the lookups are 160 µs. The statement is 252,550 µs against 256,334 µs. The log stays 12,181,196 bytes and the checksum stays 4546235365. The map payload is 1,600,000 bytes. The rebuild allocates 6 pages, frees 783, and uses 2,876,048 scratch bytes. Early validation count is 5 against the late path's 4, because the plan adds one walk and the late path never built a map.

Buckets, half, alternating, and consecutive already had a late map. With one walk, their index time is now level with that path or a few hundred microseconds under it: buckets 16,264 µs against 16,702 µs, half 17,835 µs against 18,239 µs, alternating 17,274 µs against 17,446 µs, consecutive 10,928 µs against 11,395 µs. Descents still fall: buckets 155 to 63, half 2 to 1, alternating 156 to 63, consecutive 2 to 1. Half and consecutive skip the pre-write validation (`validations` 4 against 5). The rebuilt tree is still validated after the write. Half still allocates 4 pages, frees 840, and uses 1,924,176 scratch bytes. Logs and checksums match the late path and the chain path.

Delete-all is the remaining index regression against the late map: 32,399 µs against 31,247 µs. Both paths validate 5 times, because 246 single-entry deletes write pages before the rebuild, so the pre-write check still runs. The early build is 2,151 µs against 1,761 µs, and 1,386 µs of that is inserting the full posting set and the parent paths. The statement is 305,296 µs against 306,560 µs. The chain index is 79,947 µs. The log stays 12,168,896 bytes.

Ten keys at 100,000 rows estimate 634 pages and decline. Descents stay at 9. Index time is 3,998 µs against 4,009 µs late and 3,904 µs on the chain path. One row never estimates. Sparse NULL-bucket rows stay on the monotonic walk: 2 descents, no map, log 9,581,796 bytes, checksum 4001250051. Its index time is 6,056 µs against 5,858 µs on the chain path.

At 100 rows the tree is one leaf and no plan runs. At 1,000 rows ten keys see 6 pages and do plan. Index time is 135 µs against 85 µs late and 92 µs on the chain path, on a statement of about 14 ms. That is a map the fanout ceiling would have refused, and it does not pay for itself at that size. The 1,000-row bucket index is 421 µs against 409 µs late. At 10,000 rows ten keys estimate 62 pages and decline. The wide delete plans, and its index time is 1,021 µs against 1,062 µs late.

A monotonic run, one posting, a request that is not longer than an exact page count, a one-leaf tree, and a validation or reservation failure stay on the older check. The late map remains available as a test force. The early plan is the default because the extra internal-page walk is gone, and the wide delete is faster for it.

## Milestone 35 validated-path reuse

16 Nov 2026, 4:01 PM CST, Apple M5, 17179869184 bytes of memory, rustc 1.100.0. The ignored test is `profile_milestone35`. It uses seed `0x3100`. `current` removes a non-underflow single through the parent path already in hand. `prior` restores Milestone 34: the older map bound, and `delete` for that single. `late` keeps Milestone 33's map. `chain` forces the Milestone 32 leaf-chain check. At 100,000 rows each workload is the median of three fresh copies. Smaller sizes are one sample. Counters below are from the first copy. Statement time includes the log sync. These are process-local timings.

The work unit is one page read. A map is planned when `postings * height` exceeds the largest tree of that height, which is the same point the late map would be built. An exact smaller tree is planned when `postings > pages * height`, one full walk per level. An inexact ceiling still plans when `postings * height` exceeds that ceiling. Ten keys on the 1,000-row tree see 6 pages. Eight indexed bucket postings are not longer than `6 * 2`, so the map stays unbuilt. Milestone 34 planned it. The statement is 12,547 µs against 12,557 µs, and the index is 132 µs against 142 µs. Descents are 9 against 1. The log is 61,596 bytes on both, and `map_bytes` is 0. At 10,000 rows the same ten keys see 62 pages and also decline. At 100,000 rows they see 634 pages and decline, as they did in Milestone 34.

A cached path is used only after the live leaf is decoded and the posting is found in it. A grouped rewrite and that single keep every other path: child indexes and the other leaf ids stay put, and a separator key may change. A span rebuild, a merge, a borrow, a root contraction, and a freed page drop the whole plan. The next posting descends from the new root. `reused` counts singles applied through a path already in hand. `invalidations` counts plans dropped for a structural change. `map_bytes` is `size_of<(i64, u64)>() + size_of<u32>()` times the live postings of the planned index, 20 bytes each. It leaves out the hash-table buckets and the parent-path vectors. Peak RSS was not measured.

| workload | current | prior | late | chain | index | prior index | descents | prior descents | reused | plans | map bytes | WAL | checksum |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| one | 20,985 | 20,560 | 22,064 | 20,927 | 19 | 17 | 1 | 1 | 0 | 0 | 0 | 36,996 | 5001482560 |
| ten | 67,963 | 65,393 | 65,564 | 63,728 | 4,455 | 4,560 | 9 | 9 | 0 | 0 | 0 | 2,045,996 | 5001482470 |
| buckets | 256,639 | 270,934 | 274,831 | 282,657 | 18,033 | 19,367 | 2 | 63 | 61 | 1 | 1,600,000 | 10,906,096 | 3000897535 |
| half | 201,259 | 198,214 | 199,086 | 224,699 | 18,947 | 19,062 | 1 | 1 | 0 | 1 | 1,600,000 | 7,101,296 | 3750796338 |
| sparse | 211,623 | 212,952 | 212,848 | 210,884 | 6,080 | 6,250 | 2 | 2 | 0 | 0 | 0 | 9,581,796 | 4001250051 |
| alternating | 271,544 | 273,780 | 273,138 | 296,004 | 18,276 | 18,763 | 2 | 63 | 61 | 1 | 1,600,000 | 11,537,496 | 2500711280 |
| consecutive | 133,956 | 131,435 | 132,385 | 145,790 | 11,635 | 11,472 | 1 | 1 | 0 | 1 | 1,600,000 | 4,555,196 | 4688644540 |
| all | 312,501 | 312,774 | 312,429 | 366,687 | 33,799 | 32,377 | 2 | 248 | 246 | 1 | 1,600,000 | 12,168,896 | 0 |
| wide | 262,996 | 261,099 | 267,243 | 266,024 | 11,513 | 11,197 | 2 | 71 | 69 | 1 | 1,600,000 | 12,181,196 | 4546235365 |

Times are microseconds. The log and the checksum match Milestone 34 on every 100,000-row workload. A non-underflow rewrite seals the same pages as one `delete` that does not merge.

Buckets and alternating are the mixed grouped and single deletes. Each reuses 61 paths, drops the plan once, and descends twice. Bucket index time is 18,033 µs against 19,367 µs, and the statement is 256,639 µs against 270,934 µs. Alternating index time is 18,276 µs against 18,763 µs. Half, consecutive, and sparse have no reused path. Their statements stay within a few milliseconds of the prior path, and the logs are unchanged: half 7,101,296 bytes, consecutive 4,555,196 bytes, sparse 9,581,796 bytes.

Delete-all reuses 246 paths. Descents fall from 248 to 2. The index timer rises from 32,377 µs to 33,799 µs. The statement stays 312,501 µs against 312,774 µs. The log stays 12,168,896 bytes. Each reused single still reads the ancestors before it writes the leaf, including ancestors whose separator did not change. That probe is the remaining index cost. Wide reuses 69 paths and descends twice, against 71 prior descents and 7,274 late descents. Its index time is 11,513 µs against 11,197 µs prior. The statement is 262,996 µs against 261,099 µs prior and 267,243 µs late. The log stays 12,181,196 bytes and the checksum stays 4546235365. The map payload stays 1,600,000 bytes wherever a plan is built.

One row never enters the multi-posting path, so `reused` stays 0 and the log stays 36,996 bytes. At 100 rows every tree is one leaf and no path is reused. At 1,000 rows the bucket, half, alternating, consecutive, delete-all, and wide statements do plan: half reuses 1 path, buckets 73, alternating 9, consecutive 6, delete-all 37, and wide 29. Their logs match the prior path.

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

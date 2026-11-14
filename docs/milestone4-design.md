# Milestone 4 design: incremental page commits

The implementation on `feat/milestone-4` follows this design, with later review corrections applied on top of the original text. Migration from version 1 is explicit (`rustdb --migrate PATH`) and never runs as part of open. The next table id is a high-water mark stored at offset 4092 of page 0. Files written with that mark do not reuse an id after `DROP TABLE` and restart. A file written before the mark has no deleted-id history, so the first create after opening it can reuse one. A failed `sync_all` of the commit record is an unknown outcome: the log is kept and the process does not report a rollback. The path lock is `PATH.lock`, kept after unlock, and the database inode is locked as well.

The measured Milestone 3 baseline is the historical table in [benchmarks.md](benchmarks.md). The incremental run is recorded there separately. Those scan times are in-memory. The file rewrite is what the incremental commit replaces.

## Recommendation

Use a **statement redo log of final page images**, stored in a sidecar file next to the database, then **checkpoint those pages in place** before `execute` returns.

A small mutation dirties a handful of pages: the heap page that receives the row, any neighbor page whose link changes, the catalog page that holds the table, and the meta page when the freelist or file length changes. The log and the checkpoint write that set. They do not walk or rewrite the other tables.

Page bodies stay the Milestone 3 slotted layout. Page id remains the page's index in the file, so heap links, catalog links, and freelist ids do not move when a page is updated. The format version becomes **2** because the meta page gains a nonce and a commit id, and a version 1 reader must refuse a file that may be mid-checkpoint.

This is a physical redo log for one statement, checkpointed immediately. It is not ARIES. There is no undo log, no buffer-pool eviction, no fuzzy checkpoint, and no group commit.

## Why this, and what the alternatives cost

Three designs can make one statement atomic. They differ in what gets copied and in how a crash is repaired.

### Full-file shadow, which is Milestone 3

`Store::publish` rebuilds every heap and the catalog from the in-memory row vectors (`write_heaps`, `write_catalog`, `write_freelist`), seals every page, and `Pager::commit` writes that image to a temporary file, syncs it, renames it over the database, and syncs the parent directory. A crash before the rename leaves the previous file. That atomicity is real and is already tested.

The cost is the whole file. At 10,000 rows the file is 2,748,416 bytes (671 pages). Every insert pays for every page again, including tables the statement did not touch. The in-memory `Vec` is then thrown away as a persistence source and rebuilt on the next open.

Keep this path only as the one-time upgrade from a version 1 file. It is the wrong steady state.

### Shadow paging of dirty pages

Copy each dirty page to a new physical slot, sync those slots, then swing one root (the meta page, or a page map) to publish them together. A crash before the root swing leaves the previous root. Unreachable new pages are ignored.

This stays attractive only if a page's identity is independent of its file offset. Today the page id **is** the file index (`storage-format.md`, header offset 4). Moving a page to a free slot changes its id. Every inbound pointer must then be updated: the previous row, the next row, the catalog head, and the catalog's `next page`. Updating those pages moves them too. One row edit can cascade across the chain.

A translation map (logical page id to physical offset) stops the cascade. The map is a new on-disk structure, a new crash rule, and another page type that itself must be updated atomically. That is a larger format and a larger recovery story than RustDB needs before it has indexes. Shadow paging is the right follow-on if a later milestone wants copy-on-write snapshots for readers. It is the wrong first replacement for a single writer that already serves SQL from memory.

### Rollback journal of the old pages

Before modifying the database, copy the original bytes of each dirty page to a journal, sync the journal, then overwrite the database, sync it, and delete the journal. Recovery copies the journal back when the journal is still valid.

The byte count matches a redo log: the old images, then the new images, then two syncs. The commit point is "the journal is gone", so the database sync cannot be skipped and a crash after a successful database sync still rolls the statement back until the journal is removed. A redo log can name the new images as the committed state as soon as they are synced, and replaying them is idempotent. That is a better fit for "the statement that reached disk is the statement recovery keeps."

### Logical statement log

Append `INSERT`/`UPDATE`/`DELETE` text or a row encoding, and replay it against the heap on recovery. The log is smaller than a page image. Replay must be exactly the same mutator, including slot placement and freelist order, or the replayed file diverges from the file the crashed process had in memory. Page images avoid that second implementation. A 4 KiB image of a page that changed by one 100-byte row is the right trade for a format whose unit is already the page.

## What stays

- SQL behavior, three-valued logic, and the in-memory `Table` as the execution engine. `SELECT` reads memory and does not touch the file. That is why the 0.372 ms scan can survive this change. Persistence must not start reading the heap from disk on every query.
- One statement is one atomic change. A multi-statement string is still applied one statement at a time.
- `Database::new()` stays memory-only and keeps the existing tests.
- Page size 4096. Slotted catalog and heap. Record tags for `NULL`, `INTEGER`, `TEXT`, and `BOOLEAN`. Maximum cell 4068 bytes. No overflow pages.
- Row ids are `u64`, assigned at insert, kept by `UPDATE`, not reused by `DELETE`, and not returned by SQL.
- Page id 0 is the meta page and the null link.
- Checksums stay CRC-32/ISO-HDLC over the page with the checksum field zeroed. A bad checksum is still a hard error, not a repair.
- Single writer. No B+ tree, no network protocol, no buffer-pool eviction.
- `#![forbid(unsafe_code)]` and the current public SQL API.

## Disk layout

### Version bump

Version 1 files remain readable. Opening one builds the in-memory image with the existing loader, then writes a version 2 file with the **existing** full-image rename path and replaces the version 1 file. That upgrade runs once. Its crash behavior is the Milestone 3 crash behavior: the old file remains until the rename.

After a successful upgrade, every commit uses the redo log. A version 1 binary already rejects `FORMAT_VERSION != 1` with `unsupported database format version N`. Leaving the version at 1 would let that binary open a file whose pages were half checkpointed and report a checksum or chain error, or worse, return a partial statement that happened to checksum. The version field is what makes that a clean refusal.

Page headers are unchanged. `flags` and `reserved` stay 0. Reusing them to hide a version change would make a version 1 open fail as `database page 0 is corrupt`, which is the wrong diagnosis.

### Meta page, version 2

The version 1 fields keep their offsets. Two fields are inserted before the inline freelist, so free ids move from offset 56 to offset 72.

| Offset | Type | Field |
| --- | --- | --- |
| 24 | 8 bytes | Magic `RUSTDB\0\0` |
| 32 | `u32` | Format version `2` |
| 36 | `u32` | Page size `4096` |
| 40 | `u32` | Page count, including page 0 |
| 44 | `u32` | Catalog root |
| 48 | `u32` | Freelist directory root, or 0 |
| 52 | `u32` | Count of free page ids stored in this page |
| 56 | `u64` | Database nonce, chosen once at creation or upgrade |
| 64 | `u64` | Commit id. `0` on a new file. Increases by 1 when a statement's checkpoint finishes |
| 72 | `u32` repeated | Inline free page ids |

Inline capacity falls by 16 bytes, from 1010 ids to 1006. Directory pages are unchanged: count at offset 24, ids at offset 28, 1017 ids, `next page` in the header. The freelist formula gains the new inline capacity and otherwise stays the same.

The nonce is random, or at least unique per file, and is not derived from the path. The commit id is the generation of the last finished checkpoint. Neither value is a SQL feature.

### Catalog and heap cells

The cell encodings in version 1 stay. Two persistence rules change, without new columns inside the cell:

- **Table ids become stable.** Version 1 writes `index + 1` after sorting names on every publish (`write_catalog`). Version 2 stores the id that was assigned at `CREATE TABLE` and never renumbers it. `DROP TABLE` does not reuse it. The next id is `max(existing) + 1`, remembered in memory after load. The id remains absent from SQL results.
- **The tail of each heap chain is cached in memory** while the file is open. Load already walks every chain. The walk records the last `(page, slot)`. Insert uses that cache. It is not a new on-disk field. A crash cannot observe a stale cache, because the cache is rebuilt from the recovered chain.

Upgrade assigns each table the id its last version 1 publish wrote, which is the name-sorted position, and then stops renumbering.

### Redo log

The log is a sidecar named by appending `.wal` to the database path, in the same directory. It is not a second database. It holds at most one statement. There is no log history to truncate later.

A frame is an `u32` page id plus the 4096-byte sealed page. The page's own checksum covers the image. The page id outside the image must equal the page id inside the header.

| Region | Bytes | Contents |
| --- | --- | --- |
| Header | 64 | Magic `RUSTWAL\0` (8), log version `u32` = 1, page size `u32` = 4096, nonce `u64`, base commit id `u64`, reserved to 64 |
| Frames | `(4 + 4096) * N` | Final image of each dirty page, each page id once |
| Trailer | 32 | Magic `RUSTCMT\0` (8), frame count `u32`, CRC-32 of the frame bytes `u32`, new commit id `u64`, reserved |

`base commit id` is the meta page's commit id when the statement started. `new commit id` is that value plus one. The meta frame, when present, carries the new commit id and the new page count. A statement that does not allocate or free a page still bumps the commit id in a meta frame, so recovery can tell a finished checkpoint from a log that was never applied. Every committed statement therefore dirties page 0. That is one extra 4 KiB image, not the rest of the file.

The trailer covers the frames. A torn last sector fails the trailer checksum or the magic check. Recovery treats that as "no commit."

## Data flow

```
SQL statement
  -> parse and apply to the in-memory table (unchanged validation)
  -> if the statement does not mutate, return
  -> build the final bytes of each dirty page in the pager cache
  -> write header, frames, and trailer to the .wal sidecar
  -> fsync the sidecar                         (commit point)
  -> write those pages into the database file
  -> fsync the database file
  -> truncate the sidecar to empty
  -> return Ok
```

`SELECT` does not enter this path. It reads the `Vec` that `apply` already updated on a previous statement, or the image loaded at open.

The in-memory undo snapshot taken today in `execute_statement` still covers a failure **before** the commit point. After the sidecar fsync has succeeded, memory is kept even if a later checkpoint step returns an error. Recovery will install the same pages. Rolling memory back at that point would recreate the Milestone 3 bug where memory and the durable file diverge.

Dirty-page construction reads and updates the pager cache. It does not call `write_heaps` on untouched tables. The cache is filled from the file on first use of a page and is the source of the next edit. After a successful checkpoint the cache matches the file. `publish` of a full `TableImage` list goes away for version 2 statements.

## Commit algorithm

Inputs: the in-memory statement has already succeeded, and the set of final sealed pages is known. Page 0 is always in the set. Its commit id is `base + 1`.

1. If `path.wal` exists and is non-empty, run recovery before accepting the statement. Open never leaves a committed log sitting behind a new statement.
2. Write the 64-byte header with the file nonce and the current commit id. Write each frame. Write the trailer. Use one `write` sequence on a file opened read/write in the same directory. Do not `rename` the log over the database.
3. `sync_all` the log. This is the commit point. A crash after this returns, and a later open must install these pages.
4. For each frame, seek to `page_id * 4096` and write the 4096 bytes. If `page_id` is past the current end, extend the file with sealed pages for the hole, then write the frame. Holes are only legal when the freelist and page count in the meta frame account for them. The implementation should allocate the next id rather than leave holes.
5. `sync_all` the database file.
6. Truncate the log to length 0 and `sync_all` the log again so the empty length is durable. Truncating in place avoids a directory sync on every statement. The directory entry for the log is created once, when the file is first created, and that creation is synced once.
7. Update the pager's page count and free-id vector from the new meta page. Return success.

A failure in steps 1–2, before step 3 returns, deletes or truncates the partial log, restores the in-memory undo snapshot, and leaves the database file as it was. A failure in steps 4–6 after step 3 returns an error to the caller and **does not** restore the snapshot. The next open replays the log.

The exclusive lock described below is held across the whole sequence.

## Recovery algorithm

Recovery runs at the start of `Pager::open`, before the catalog walk, and any time a commit finds a non-empty log.

1. Take the exclusive lock. A failed lock is `database is locked`. Do not read further.
2. If `path.wal` is missing or has length 0, load the database.
3. If the database is missing or shorter than one page, and the log is non-empty, refuse to open. Do not invent a database from a log whose nonce cannot be checked.
4. Read the database meta page with the version 2 rules. Check magic, version, page size, and file length against page count.
5. Read the log header. Refuse to open when the magic, log version, or page size is wrong, or when the nonce differs from the meta nonce. Do not delete that log. It may belong to a different generation that was copied beside this file.
6. If the header's base commit id is not the meta commit id and is not the meta commit id minus one, refuse to open. The only legal logs are "checkpoint not started" (`base == meta`) and "checkpoint wrote the new meta, truncate did not finish" (`base + 1 == meta`, and the trailer agrees).
7. A trailer is valid only when the file length is `64 + frame_count * (4 + 4096) + 32` for the trailer's own frame count, the trailer magic matches, that frame count matches the bytes between the header and the trailer, the trailer checksum matches those bytes, and `new commit id == base + 1`. Any other shape is not a commit.
8. No valid trailer: truncate the log to 0, sync it, and open the database unchanged. The statement is not durable.
9. Valid trailer: for each frame, reject a page checksum failure, a page id that does not match the frame, or a page id that does not fit in `u32` addressing. On success, write every frame at `page_id * 4096`, extending the file when the meta frame's page count requires it. Sync the database. Truncate the log to 0 and sync it.
10. Open the resulting file with the logical audit below. The audit failure is a hard error. Recovery does not delete pages to force the audit to pass.

Replaying a valid trailer is idempotent. Writing the same sealed images twice is the same file. A crash in step 9 leaves the trailer in place, and the next open repeats step 9.

Recovery does not consult SQL text. It does not rebuild heaps. It installs page images and then checks them.

## How each statement uses the log

The mutator edits cached pages and records the final image of each page it touched. Links are patched with the existing `patch_links` rules: `prev` of the head is the null link, `next` of the tail is the null link, and every other record points at its neighbor.

### INSERT

Memory assignment of row ids is unchanged, including the overflow check on `next_row_id`.

The new record is encoded with its row id and null links. The in-memory tail cache says where the chain ends.

- Empty table: allocate a heap page (freelist pop, or `page_count` as the new id), insert the cell, set the catalog head and the tail cache to that cell, set `next_row_id`.
- Tail page has room: insert the cell there, set the old tail's `next` and the new cell's `prev`, move the tail cache.
- Tail page is full: allocate another heap page, insert there, patch the old tail's `next` and the new cell's `prev`.

Dirty pages: the catalog page, every heap page that gained a cell or a new link, and page 0 when a page was allocated or the freelist changed. Page 0 is also dirtied to bump the commit id, so it is always included.

A multi-row `INSERT` is one statement and one log. The benchmark's one-row-per-statement workload is a separate commit per row, which is the comparison we owe the Milestone 3 numbers.

### UPDATE

Memory still evaluates every assignment against the original row, then installs the replacement with `rewrite_all`, which keeps row ids by position.

For each changed row the encoder produces a new cell.

- The new bytes fit in the existing slot: overwrite that cell. Links stay. One heap page is dirty, plus page 0 for the commit id.
- The new bytes do not fit: free the old slot, compact if needed, and insert on the same page or on another page with room. Patch the previous and next records, which may live on other pages. The row id inside the cell is the same id. The tail cache moves only when the tail itself moved.

An oversized row fails in `ensure_record_fits` before any log write. The file is unchanged and memory is rolled back.

### DELETE

Memory still keeps survivors with `retain_indexes`. For each removed index, the heap cell is unlinked and its slot is marked free. When a page loses its last live cell, it is sealed as a free data page and its id is pushed onto the freelist. The catalog head is updated when the removed row was the head. The tail cache is updated when the removed row was the tail.

Dirty pages are the catalog page, each heap page that lost a cell or a link, each newly free page, the freelist directory page when the inline list overflows, and page 0.

Deleting every row of one table does not rewrite other tables. It does rewrite every page of the table being deleted, because those pages have to become free. That cost is proportional to the deleted table.

### CREATE TABLE

Allocate a stable table id. Append one catalog cell to the last catalog page, or to a new catalog page when the cell does not fit. An empty table has a null head. No heap page is allocated until the first insert. A definition larger than 4068 bytes fails before the log, as it does today.

### DROP TABLE

Walk that table's chain from the in-memory row list, not by decoding the whole database. Free each heap page. Delete the catalog cell. If a catalog page loses its last cell and is not the root, unlink it from the catalog chain and free it. Other tables' pages are not rewritten.

`DROP` of several names stays one statement: one memory change, one log, one commit. A missing name still fails the statement before the log, with no table removed.

## Row ids and page ids

Row ids stay a property of the record and of the in-memory `row_ids` vector. The log never allocates them. Update-in-place and update-by-move both write the original id into the cell. Delete does not return the id to a free list. The next id is the catalog cell's `next_row_id`, already checked against overflow.

Page ids stay file indexes. In-place checkpoint does not assign a new id to an existing page. A new page takes a freelist id or `page_count`, and the meta frame's page count includes it before that frame is logged. Recovery writes that page at the same index. Links that were correct in the logged image stay correct because the id did not change underneath them.

Table ids are stable across statements, as described above. They are not row ids and they are not page ids.

## Free space

The freelist remains a list of page ids on the meta page and, when that list overflows, on directory pages. A page id is free, or it is reachable from the catalog, the freelist directories, or page 0. It is not both.

Reuse pops the highest free id, matching the current pager, so a table that shrinks and grows again consumes those ids before extending the file. The high-water mark is still the meta page count. The file shrinks only in the sense that it stops growing. This design does not punch holes or truncate the high-water mark. Truncation is a later optimization with its own crash window.

A slot freed inside a page that still has other cells is reused by the existing slot directory (`offset == 0 && length == 0`) and compaction. That reuse does not return the page to the freelist.

A page freed in a statement may be reused by a later statement, not by an earlier crash. The logged images are the final bytes of the statement. If one statement both frees a page and allocates it again, the log contains the final image only, once. Recovery does not replay the intermediate free.

## Readers and writers

### In one process

Statements run one at a time on one `Database`. `SELECT` runs before or after `apply`, never in the middle of a page edit. It observes the memory state of the last statement that returned, or the state loaded after recovery. A statement that fails before the commit point rolls memory back, so a following `SELECT` sees the previous rows. A statement that fails after the commit point leaves memory on the new rows, and recovery puts the same rows on disk.

There is no latch protocol and no snapshot for a second thread. The type is not `Sync`. That is intentional.

### A second process

Opening the database takes an exclusive advisory lock on the database file and holds it until the file is closed. A second open fails with a storage error that says the database is locked. It does not read, and it does not create a second log.

The lock has to disappear when the process dies, including after `kill` and after a crash. `create_new` on a lock file does not do that: the lock file survives and the next open cannot tell a live writer from a corpse without racing. `#![forbid(unsafe_code)]` rules out a direct `flock` call. The implementation should take the lock through a small safe crate (`fs4` or an equivalent already-reviewed wrapper) whose exclusive lock is released by the operating system when the process exits. That is one dependency, next to `sqlparser`. It is listed under unresolved decisions because this repository has so far refused extra dependencies. The fallback, if that dependency is rejected, is to refuse a second open while the process is alive and to document a stale lock file that a person must delete. That fallback is worse and should not be the default.

The lock is what stops a reader from observing a checkpoint. During steps 4–6 of commit the database file can contain a mixture of old and new pages. No other process is allowed to open it in that window. After recovery, an open sees either the previous commit or the committed statement, then the logical audit runs.

This design does not serve read-only snapshots to other processes. A second process always fails while the lock is held.

## Logical consistency

Checksums prove a page was not torn at random. They do not prove that the catalog, the chains, and the freelist describe one statement. Version 1 load already checks checksums, page ids, types, catalog cycles, heap `prev` links, value types, unique row ids, and freelist range. It also **accepts an orphan**: any id in `1..page_count` that is neither reachable nor listed free is added to the reusable set (`load` in `store.rs`). A checksum-valid catalog that drops a `next` pointer silently loses those rows, and the next commit may reuse the page.

Version 2 open, after recovery, rejects the file when any of the following is true.

- The file length is not `page_count * 4096`, or page 0 is not the meta page, or the nonce is zero.
- A page id inside a header does not equal its index. A checksum does not match. `flags` or `reserved` is not 0. The type is not meta, catalog, heap, or free.
- The catalog chain repeats a page, leaves the file, or visits a page that is not a catalog page.
- Two catalog cells share a name or a table id. A table id is 0. A name is empty. A definition fails the schema rules. `next_row_id` is 0.
- A heap chain repeats `(page, slot)`, leaves the file, visits a non-heap page, disagrees with the previous link, disagrees with the column count or types, or contains a row id that is 0, duplicated, or greater than or equal to `next_row_id`.
- The head cell's `prev` is not the null link, or the tail cell's `next` is not the null link.
- A live slot on a heap page that belongs to a table is not on that table's chain, or a chain entry is not a live slot. Every live cell is exactly one row.
- A heap page is reachable from two tables.
- A free id is 0, out of range, duplicated, equal to a directory page, or equal to a page that is also reachable.
- A page other than 0 is neither reachable from the catalog, a freelist directory, or a heap chain, nor present in the freelist. Orphans are corruption. They are not reused.
- A free data page's type is not free, or a directory page's type is not free, or a directory count runs past the page.
- A decoded cell has trailing bytes. `decode_record` and `decode_catalog` already require the cursor to finish the cell. The audit keeps that rule.

Version 1 upgrade does not run this audit against the old file's orphans. It classifies version 1 orphans as free ids and writes them into the version 2 freelist, once, so an older file that took the lenient path still opens. After that write, the version 2 audit applies. Reachable version 1 data is preserved. A version 1 file that already fails its own loader still fails.

The audit is also the check that a bug in the incremental mutator did not produce a self-consistent checksum on a wrong chain. Tests should run it after every committed statement by opening the file a second time, not only when a checksum is flipped.

## Guarantees

Provided, on a local filesystem that honors `File::sync_all` for a file that was written before the sync:

- After `execute` returns `Ok` for a mutating statement, a later open by this program sees that whole statement: every row it inserted, updated, or deleted, the same row ids, and the same table set.
- A crash before the log trailer is durable leaves the previous committed statement. The partial log is discarded.
- A crash after the trailer is durable and before the log is truncated installs that statement on the next open, including when some database pages were already overwritten and some were not.
- A single statement is atomic. Recovery never installs a subset of its frames.
- A second process cannot write the file while it is open.
- `SELECT` in the same process does not observe a half-applied statement.

Not provided:

- Power-loss atomicity beyond what `fsync` actually does on that machine. A drive or filesystem that reports a successful sync before the bytes are stable can still lose the last committed statement or tear a page. RustDB will not claim otherwise, and the test plan does not include pulling power.
- A torn sector that happens to match CRC-32. The checksum is the detection method. It is not a promise against a crafted collision.
- Repair. Audit failures and nonce mismatches stop the open.
- Atomicity of a multi-statement string, of a client that did not see `Ok`, or of a statement whose log committed and whose checkpoint then failed. In that last case the data is durable and the caller saw an error. A retry can duplicate a row. That window is the same shape as a crash after Milestone 3's rename.
- Concurrent readers, read-only clones, or a writer on another machine via a network filesystem. Locks and `fsync` on network filesystems are unsupported.
- Shrinking the file below its high-water mark.
- Index consistency. There are no indexes yet. A later index page can be one more frame in the same statement log without a new recovery protocol. That is an extension point, not part of this milestone.

## Failure injection

Tests should force each failure without killing the process. A test-only hook on the pager returns an error at a named stage, or it returns success and skips the rest of the process so the files stay behind for a second `open`. The second open is the recovery under test. The hook is not a SQL command and is not compiled into the library's public surface (`#[cfg(test)]`).

| Stage | Injection | Database file after the crash | Log after the crash | Required outcome |
| --- | --- | --- | --- | --- |
| Header only | Stop before any frame | Previous bytes | Header, no trailer | Open discards the log. Previous rows and row ids remain |
| Torn frame | Write a partial last frame, no trailer | Previous bytes | Short frame | Open discards the log. Checksum or length rejects the trailer |
| Frames without trailer | All frames, no trailer, no log sync | Previous bytes | Frames, no trailer | Open discards the log |
| Trailer not synced | Trailer written, skip `sync_all`, drop the handle as a crash | Previous bytes | Trailer present but the test treats the sync as failed by not acknowledging | The durable outcome is defined only for bytes that were synced. The test syncs the log when it wants this stage to be a commit, and it does not sync when it wants discard. Both are asserted |
| Commit point | Trailer synced, no database writes | Previous bytes | Valid trailer | Open installs every frame. The new statement is present, including its row ids |
| Checkpoint page *k* of *n* | First *k* pages overwritten, remaining pages old, log still valid | Mixed pages | Valid trailer | Open writes all frames again. The audit passes. The statement is complete |
| Torn database page | Overwrite 100 bytes of a checkpointed page, leave the log | One page fails its checksum | Valid trailer | Open replaces the page from the frame. The statement is complete |
| Database synced, log not truncated | All pages written and synced, log still valid, meta commit id already new | New bytes | Valid trailer whose new commit id equals meta | Open replays idempotently, truncates the log, and the statement is present once |
| Truncate then crash | Log length 0, database synced | New bytes | Empty | Open does not replay. The statement is present |
| Nonce mismatch | Valid log whose nonce is not the meta nonce | Previous or new | Foreign log | Open returns an error and does not truncate the log |
| Checksum-valid bad chain | After a normal commit, flip a `next` link and recompute that page's checksum, no log | Audited as corrupt | Empty | Open returns `database file is corrupt` or the page corrupt error. Rows from the broken chain are not returned |
| Orphan page | Append a sealed free page and increase nothing else, or bump page count without a freelist entry | Extra page | Empty | Open rejects the file. The page is not reused |
| Second writer | Second `Database::open` while the first handle is alive | Unchanged by the second process | Unchanged by the second process | Second open fails with the locked error. The first process can still commit |
| Lock released | Drop the first database, then open | Last commit | Empty | Second open loads the last committed statement |
| Upgrade crash | Version 1 file, crash the one-time full rewrite before rename | Version 1 bytes | None | Open still sees version 1 and can retry the upgrade. Version 2 is not partially visible |
| Pre-commit SQL error | Oversized row, bad type, missing table | Unchanged bytes | Empty | Memory and file match the previous statement |

Each row that says the statement is present checks row values, row ids, `next_row_id`, table ids, and the audit. Each row that says the previous statement remains checks that the file bytes of the database are unchanged or that a reload matches the previous image. Where the recovery rewrite can change page checksum placement but not logical content, compare the loaded tables, not the raw file, except for the stages that require the bytes to be untouched.

## Implementation phases

The phases are the order to build and test. They are one milestone, not three releases. Each phase leaves `cargo test` green and does not switch the default commit path until the phase that has recovery tests.

1. **Version 2 meta and upgrade.** Read and write the nonce and commit id. Upgrade a version 1 file through the existing rename path. Version 1 unit tests that construct pages directly keep using the version 1 helpers until they are pointed at version 2. Logical audit, including orphan rejection, runs on version 2 open. No change yet to how a statement is published: upgrade and any temporary v2 publish may still rewrite the file. This phase exists so the crash tests have a format to corrupt.
2. **Localized mutators against the page cache.** Implement insert, update, delete, create, and drop as dirty-page edits. After each edit, run the audit in memory and compare the decoded tables to the in-memory `Table` values, including row ids and table ids. The commit path can still be the full-file writer in this phase if that keeps the diff reviewable. The mutators must not call `write_heaps` for tables the statement did not change.
3. **Redo log, checkpoint, and the failure matrix.** Replace `Pager::commit` of the full image with the algorithm above. Wire `execute_statement` so memory rolls back only before the commit point. Add the exclusive lock. Run the injection matrix. Delete the full-image publish from the steady-state statement path. Keep it for version 1 upgrade.
4. **Benchmark.** Run the plan below on a release build. Record the results in `docs/benchmarks.md` only when they are real measurements. Do not add a predicted column.

Phase 2 and phase 3 can share a branch. Phase 2 is still worth a separate test suite, because a wrong link patch is easier to see before the log exists.

## Testing strategy

Existing tests stay. `Database::new()` coverage for Milestone 1 and 2 must not open a file. Milestone 3 restart, isolation, oversized-row, and checksum tests should run against version 2 after upgrade, with the same SQL assertions.

Add focused tests:

- One-row insert, update in place, update that moves a row, delete of the head, delete of the tail, delete of the last row on a page, create, and drop. Reopen and compare row ids and table ids.
- A second table that is not named by the statement keeps its page ids and its bytes. This is the regression that full-file rewrite cannot express. Record the page bytes of the untouched table before and after.
- Fill a heap page to `InsertError::Full`, insert one more row, and check that only the old tail, the new page, the catalog, and meta were logged. The test can count frames.
- Free-page reuse: delete enough rows to free a page, insert again, and assert the file length does not grow when a free id was available.
- Catalog spill: the existing 160-table test still reloads. Create one more table after the spill and assert the older catalog page's unchanged cells still have their table ids.
- The failure matrix in the table above.
- A version 1 fixture, written by the current helpers, opens under version 2 and then round-trips a statement without a second full rewrite. Assert the second statement's log frame count is far below the file's page count.
- Two-process behavior is two opens in one process with two `Database` values, plus one test that the lock does not stick after drop. A real second process is optional and not required for the matrix.

Property check, small and deterministic: for a script of a few hundred random inserts, updates, and deletes on two tables, with a fixed seed, every tenth statement reopens the file and compares it to the in-memory database. This is a unit test, not a benchmark.

## Performance evaluation

Do not treat the table in [benchmarks.md](benchmarks.md) as the control for a later day. Those numbers are one run. The evaluation reruns the Milestone 3 publisher and the new publisher on the same machine, in release mode, with the same schema and the same sample rules.

Harness: extend `examples/storage_bench.rs` so one invocation can run a named publisher. Same schema `bench (id INT, email TEXT, note TEXT, active BOOLEAN)`, same 64-byte note, same `user#####@example.com` emails. Sizes 100, 1,000, and 10,000. One discarded warmup of 20 rows. For each size, a fresh file:

1. Time `N` single-row inserts. Report the total and the mean. This remains one build, because repeating the Milestone 3 10,000-row build is a multi-minute run and the historical write-up already treats it that way.
2. Record the file length in bytes and in pages after those `N` rows. Also record the `.wal` length. After a successful return it should be 0. Record it anyway.
3. Five full scans of `SELECT id, email, note, active FROM bench`. Report min, median, and max. The median is the comparison figure.
4. Five updates of `id = 1`, flipping `active`. Report min, median, and max.
5. Five further single-row inserts. They see `N` through `N + 4` rows. Report min, median, and max.

Then, on the 10,000-row file only:

- Delete every row in one statement. Record the time and the file length.
- Insert the same `N` rows again one at a time. Record the final file length next to the length after the first build, as the reuse measurement.
- Recovery: after a successful insert, leave a valid trailer and do not write the database pages, then time a reopen and check the row count. Separately, leave a log with no trailer and check that reopen does not gain that row. Report those times as measurements, with the machine and the rustc version. They are not targets.

Write the new numbers beside a freshly rerun Milestone 3 column. Keep the historical 13 November 2026 table in place and label it historical. The interesting comparisons are insert and update latency against file size, scan latency against the same in-memory path, and file length after delete-and-rebuild. Frame counts from the log at 100, 1,000, and 10,000 rows belong in the notes so a later reader can see that a one-row insert wrote a handful of pages.

If the 10,000-row Milestone 3 rerun is too slow to finish alongside the new path, record how far it got and still publish the new path's full result. Do not fill the missing cells with estimates.

What this plan is able to show, without predicting the result:

- Whether one-row mutation latency still grows with the number of rows, or flattens once the dirty set stays small.
- Whether the scan path regressed. It should be the same memory scan. A regression means a statement started reading pages in order to answer `SELECT`.
- Whether the fixed sync cost still dominates at 100 and 1,000 rows. The historical run spent about 10 ms even on a 44 KiB file, so a design that syncs a small log and then syncs the database can remain in that band at small sizes while still removing the 2.6 MiB rewrite at 10,000 rows. That sentence is a hypothesis about where time can go. It is not an expected measurement.

## Risks and tradeoffs

- **Two syncs per statement.** The commit point is the log sync. The ack point is after the database sync and the log truncate. Returning earlier would remove one sync and make every crash depend on replay. Checkpoint-before-return keeps a normal open on the database file alone and makes the empty-log case the common one. The cost is latency. Measure it before adding lazy checkpoint.
- **Page 0 on every commit.** Bumping the commit id dirties the meta page even for an in-place update of one cell. That is one frame. It is the simplest generation check. A design that omits the meta frame when nothing else in page 0 changed needs a different generation rule and is easier to get wrong.
- **DROP and mass DELETE still write every page they free.** Incremental means "pages this statement changed," not "a constant amount of IO."
- **The pager cache holds pages of the open file.** A large database occupies memory in proportion to pages touched, and a long session can touch all of them. There is no eviction. That matches the current "the whole working set fits in memory" engine. Eviction is a later project and would require the log to carry images of evicted dirty pages. Do not add it here.
- **Exclusive lock rejects readers.** A backup process cannot copy a consistent file while the database is open, because a copy during checkpoint can tear. The supported backup is to copy after the process exits, or to add an explicit online-backup command later that copies under the same lock. Not part of this milestone.
- **Sidecar log.** Copying the database without `path.wal` during a crash window loses a committed statement or, if the copy is of a half-checkpointed file, produces a file the audit should reject. Operators copy both or copy only when the process is stopped. The nonce check refuses a log that does not belong to the file. It cannot repair a missing log.
- **Stable table ids** change an internal field that version 1 rewrote on every publish. Nothing in SQL reads it. Any external tool that assumed "table id equals name order" would be wrong after the first `CREATE` that follows a `DROP`. No such tool is in this repository.
- **Orphan rejection** can refuse a file that version 1 opened. The upgrade path converts existing orphans to free ids once so normal version 1 files still open. A file that is corrupt by the version 1 rules still fails.
- **CRC-32 and `fsync`.** Both are the real mechanisms. Neither is a proof against a dishonest disk.
- **New lock dependency.** One small crate is cheaper than an `unsafe` block and cheaper than a stale lock file. It is still a dependency decision.

## Unresolved decisions

These need a review before implementation. The recommended choice is stated so the default is clear.

1. **Checkpoint before returning.** Recommended. The alternative is to return after the log sync and checkpoint in the background or on the next open. That is faster and makes recovery part of the normal path. It should wait until the measured two-sync cost is actually too high.
2. **Lock implementation.** Recommended: a safe `flock` wrapper crate, held on the database file for the life of the open. Alternative: no new dependency, and a lock file that can outlive a crash. Do not implement `unsafe` to avoid the dependency.
3. **Automatic version 1 upgrade on open.** The original recommendation was one full rewrite on open. Review chose the alternative: version 1 open fails and names `rustdb --migrate PATH`. That command is the only rewrite of a whole file.
4. **Commit id stored by rewriting page 0 on every statement.** Recommended. Alternative: a commit id only in the log trailer, and page 0 rewritten only when the freelist or page count changes. That saves a frame on the hot update and complicates the "did checkpoint finish?" test. Keep the meta frame until a measurement shows it matters.

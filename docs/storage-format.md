# RustDB storage format

Versions 2 and 3. Every integer field is little-endian. Text is UTF-8. The file is a sequence of fixed 4096-byte pages. Rust struct layout is not written to disk.

A new database is 8192 bytes: a meta page and an empty catalog page. Version 2 is what `Database::open` creates. `rustdb --migrate PATH` (or `Database::migrate`) rewrites version 1 as version 2, or version 2 as version 3. It does not run on open. Version 3 adds the index catalog. The meta-page version is checked before a redo log is opened. This process accepts versions 2 and 3 and then recovers. Any other version fails in that check, and the log is left untouched. A reader that only accepts version 2 rejects a version 3 file in the same check, before recovery or any write.

## Page header

Every page starts with the same 24-byte header.

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u16` | Page type |
| 2 | `u16` | Slot count |
| 4 | `u32` | Page id. Must equal the page's index in the file |
| 8 | `u32` | CRC-32/ISO-HDLC of the page with this field set to zero |
| 12 | `u32` | Next page, or 0 when there is no next page |
| 16 | `u16` | `lower`: end of the slot directory |
| 18 | `u16` | `upper`: start of the contiguous free region |
| 20 | `u16` | Flags. Must be 0 |
| 22 | `u16` | Reserved. Must be 0 |

Page types:

| Value | Name | Role |
| --- | --- | --- |
| 1 | Meta | Page 0. File identity, catalog root, and the inline freelist |
| 2 | Catalog | Table definitions, chained through `next page` |
| 3 | Heap | Table rows |
| 4 | Free | Either a free data page or a freelist directory |
| 5 | Index catalog | Index definitions, chained through `next page`. Version 3 only |
| 6 | Index internal | B+ tree separators and child page ids |
| 7 | Index leaf | Sorted `(i64 key, u64 row id)` pairs |

Opening a page with a bad checksum, a mismatched page id, or an unknown type fails. The database is not repaired.

## Meta page

Page 0 is the only meta page. After the header:

| Offset | Type | Field |
| --- | --- | --- |
| 24 | 8 bytes | Magic `RUSTDB\0\0` |
| 32 | `u32` | Format version. `2` is what a new file writes. `3` is a migrated file that can store indexes. `1` requires an explicit migration. Any other value is unsupported |
| 36 | `u32` | Page size. Must be 4096 |
| 40 | `u32` | Page count, including page 0. At least 2 |
| 44 | `u32` | Catalog root page id |
| 48 | `u32` | Freelist overflow page, or 0 |
| 52 | `u32` | Count of free page ids stored in this page |
| 56 | `u64` | Database nonce. Chosen when the file is created or migrated. Must be non-zero in version 2 |
| 64 | `u64` | Commit id. `0` on a new or freshly migrated file. Increases by 1 when a statement's checkpoint finishes |
| 72 | `u32` repeated | Inline free page ids. A version 2 writer stores at most 1005. A version 3 writer stores at most 1004 |
| 4088 | `u32` | Index-catalog root, or 0. Version 3 only. A version 2 file uses this word for its last inline free id when the free list is full |
| 4092 | `u32` | Next table id. The high-water mark for `CREATE TABLE` |

Version 1 stored the inline free ids at offset 56 and had no nonce or commit id. Opening a version 1 file returns an error that names `rustdb --migrate` and does not convert the file. Version 2 moves that list to offset 72. A current writer leaves the last four bytes of page 0 for the table-id high-water mark, so it stores at most 1005 inline ids. A reader still accepts 1006 inline ids, which is what a version 2 file used before this field existed. When the count is 1006, offset 4092 is a page id and the high-water mark is treated as unset. This is a compatible extension of version 2: an older reader ignores bytes past the free-id list, and a new reader still opens an older file.

A stored high-water mark of 0 means the file has no record of deleted table ids. That is a version 2 file written before this field, or a file whose 1006 inline free ids occupy offset 4092. The next id is then one past the highest id still in the catalog, or 1 when the catalog is empty. A deleted id from that history cannot be recovered, so the first `CREATE TABLE` after opening such a file can reuse one. The commit that follows writes the derived mark. From that commit onward the stored mark does not go backward. A non-zero mark must be greater than every stored table id. A smaller mark is corrupt. The counter uses checked addition. A next id of `u32::MAX`, or a stored table id of `u32::MAX`, is rejected as exhausted rather than wrapped. For a file this version wrote, `DROP TABLE` does not decrease the mark, so creating a table, dropping it, and reopening the file does not assign that id again.

The file length must be `page count * 4096`. A short file, or a length that is not a multiple of the page size, is truncated. An empty file is initialized instead of rejected.

Free page ids name pages that the next publish may reuse. They are not read as rows. When the inline list is not large enough, `freelist overflow` points at a free directory page. A directory page stores `u32` count at offset 24 and that many page ids starting at offset 28, and its header `next page` continues the directory. One directory page holds 1017 ids. Directory pages themselves are not free data pages.

## Slotted pages

Catalog and heap pages store variable-length cells. The slot directory starts at offset 24 and grows upward. Each slot is two `u16` values: cell offset and cell length. Offset 0 and length 0 mark a free slot. Cells grow downward from the end of the page.

`lower` is `24 + slot count * 4`. Live cells lie in `[upper, 4096)` and do not overlap. A cell that does not fit is rejected after the page is compacted. Compaction packs live cells at the end of the page and keeps their slot indexes, so a deleted slot can be reused.

The largest cell is 4068 bytes (`4096 - 24 - 4`). A larger record is rejected. Rows do not span pages. A table spans pages by storing each row on one page and linking rows together.

A published heap page from the migration rewrite is packed. An ordinary statement may leave a free slot until the page is compacted on a later insert. Slot indexes stay stable across a delete, so links keep pointing at the same slot.

## Catalog cell

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u32` | Table id. A file with a stored high-water mark assigns the next id from offset 4092 and then increments that mark. `DROP TABLE` does not return the id. A legacy file with no mark can reuse a deleted id once, as described above |
| 4 | `u64` | Next row id to assign. Starts at 1 and is never reused |
| 12 | `u32` | Head page, or 0 when the table is empty |
| 16 | `u16` | Head slot |
| 18 | `u16` | Table-name byte length, then the UTF-8 name |
|  | `u16` | Column count |
|  | repeated | `u16` name length, UTF-8 name, `u8` type code |

Type codes are `1` INTEGER, `2` TEXT, and `3` BOOLEAN. Column order in this cell is the table's order. A table definition that does not fit in 4068 bytes is rejected. Catalog pages chain when one page fills.

## Heap record

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u64` | Row id. Unique within the table, assigned at insert, kept by update |
| 8 | `u32` | Previous row's page, or 0 for the first row |
| 12 | `u16` | Previous row's slot |
| 14 | `u32` | Next row's page, or 0 for the last row |
| 18 | `u16` | Next row's slot |
| 20 | `u16` | Value count. Must match the catalog column count |
| 22 | values | One tagged value per column |

A value is a tag byte followed by a payload:

| Tag | Payload |
| --- | --- |
| 0 | `NULL`. No payload |
| 1 | INTEGER. 8-byte little-endian two's complement `i64` |
| 2 | TEXT. `u32` byte length, then that many UTF-8 bytes. Invalid UTF-8 is corruption |
| 3 | BOOLEAN. `0` or `1`. Any other byte is corruption |

`NULL` is a real tag. It is not an empty string, a zero, or a missing column. The row links are the insertion order of the rows that are still present. A page id of 0 is the null link because page 0 is the meta page.

The physical `(page, slot)` of a row stays put when an update fits in the existing cell. An update that needs a larger cell, or an insert or delete that changes the chain, can move that one row. Other rows keep their page and slot. The `u64` row id does not change. Deleted ids are not reused. Index leaves store that row id, not a position in a `Vec`.

## Index pages

Version 3 keeps an index catalog on type 5 slotted pages. The meta page's index-catalog root is 0 when there are no indexes. A catalog cell is:

| Field | Type |
| --- | --- |
| Name length and UTF-8 name | `u16` + bytes |
| Table id | `u32` |
| Column ordinal | `u16` |
| Column-name length and UTF-8 name | `u16` + bytes |
| Root page id, or 0 for an empty tree | `u32` |
| Key type | `u8`. `1` means INTEGER |
| Index format | `u16`. `1` is the only format |

A leaf (type 7) uses the shared header. `next page` is the right sibling. Offset 24 is the entry count (`1..=254`). Offset 26 is zero. Entries start at offset 28 and are 16 bytes: a little-endian `i64` key, then a `u64` row id. The left sibling is a `u32` at offset 4092. Slot count, `lower`, and `upper` stay at the empty-directory values (`0`, `24`, `24`). A non-root leaf holds at least 127 entries. Keys are ordered by the signed integer, then by row id. The same pair cannot appear twice. `NULL` is not stored.

An internal node (type 6) stores the key count at offset 24 (`1..=203`) and zero at offset 26. The leftmost child is a `u32` at offset 28. Each of the next records is 20 bytes: an `i64` key, a `u64` row id, and the `u32` child to the right of that separator. Bytes 4092..4096 stay zero, and `next page` is 0. Separator `i` is the minimum pair in child `i + 1`. A non-root internal node holds at least 101 separators. A root with one child is replaced by that child and freed. An empty root leaf is freed and the catalog root becomes 0.

Opening a version 3 file checks occupancy, order, separators, child pointers, sibling links, page ownership, the freelist, and that each non-NULL heap value has exactly the leaf entry its row id implies. A valid checksum is not enough. A version 2 file that contains a type 5, 6, or 7 page is corrupt. A child chain taller than 64 pages, or any page id that appears twice in one tree or sibling walk, is corrupt. The open returns that error instead of following the chain.

## Commit

A successful `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`, `INSERT`, `UPDATE`, or `DELETE` edits only the pages that statement changes. Page 0 is one of them, because it stores the new commit id. Unchanged heap, catalog, and index pages are not rewritten. One commit holds at most 8192 frames (`64 + 8192 * 4100 + 32` bytes of log). A statement that would dirty more than that, including a large `CREATE INDEX`, fails before the log is written and does not publish a partial tree. Heap and index pages from one statement share that commit.

The commit sequence is:

1. Build sealed images of the dirty pages, including page 0 with `commit id + 1`.
2. Write those images to `PATH.wal` in the same directory. The log is not renamed over the database.
3. Sync the same file that wrote the log. This is the commit point when the sync returns success.
4. Write the same pages into the database at `page id * 4096`, extending the file when a new page id is past the old end.
5. Sync the database.
6. Truncate the log to zero length and sync that empty file.
7. Return success only after step 6.

A failure before step 3, while the bytes on disk are still an incomplete log, restores the in-memory tables. An incomplete log may be deleted. The database file is unchanged. A failed `sync_all` in step 3 does not prove that the commit record is absent. The log is left in place, memory is not restored, and the process refuses every later statement. The error says the outcome is unknown and that this is not a confirmed rollback. Reopening is what decides: a durable commit record is replayed, and an incomplete log that belongs to this file is discarded. A write that already left a valid or damaged commit record is handled the same way as a failed sync. A failure after a successful step 3 also leaves memory in place and refuses later statements. Retrying the statement after a reopen can insert a second row, because the first attempt may already be durable.

A single-row insert rewrites page 0, the catalog page, and one new heap page, and the file grows by one page. An in-place update of that row rewrites page 0 and the heap page. The file length does not change. Deleting that only row turns the heap page into a free page without growing the file, and a later insert can reuse it.

`SELECT` reads the in-memory tables and does not write the log. A no-op publish that dirties nothing also skips the log.

The log layout is a 64-byte header, then frames, then a 32-byte trailer. The header is magic `RUSTWAL\0`, log version `1`, page size `4096`, the database nonce, and the base commit id. A frame is a `u32` page id plus the 4096-byte sealed page. The trailer is magic `RUSTCMT\0`, the frame count, a CRC-32 of the frame bytes, and the new commit id, which must be the base commit id plus one. The trailer is valid only when the file length is `64 + frame count * 4100 + 32`, the magic and checksum match, and the new commit id is exactly one past the base.

Recovery runs before the catalog is loaded:

- A missing or empty log does nothing.
- A log may be truncated only when it has no commit trailer, its nonce matches page 0, and its base commit id is the commit id in page 0 or exactly one less. The comparison uses checked addition. Any other incomplete log is refused and left in place.
- A trailer whose magic is present but whose length or checksum does not match is damaged. Open fails and the log is left in place. It is not treated as an incomplete log.
- A valid trailer is applied when the nonce matches and either the base commit id or the new commit id equals the commit id in page 0. The page 0 frame must be present, and its nonce and commit id must match the trailer. Every frame is written, the database is synced, and the log is truncated. Replaying an already-applied trailer writes the same pages again.
- A valid trailer with a foreign nonce, a missing page 0 frame, or a commit id that matches neither value is refused, and the log is left in place.
- A version 2 file refuses a log whose page 0 frame is not version 2, or whose frames include an index page, and does not apply or truncate that log. A version 3 file requires the page 0 frame to be version 3.

An open also checks that every live heap slot is on its table's chain, that a heap page belongs to one table, that free ids do not overlap live pages, and that every page id is either reachable or on the freelist. A version 2 file with an orphan page is corrupt. Migration of a version 1 file treats an orphan as free once, while building the replacement, and the replacement is then held to the version 2 rules.

## Migration

`rustdb --migrate PATH` acquires the path lock and refuses a non-empty redo log. The source version selects the target. Version 1 is rewritten as version 2: rows, row ids, and table ids are kept, the new nonce is chosen for that file, and the commit id starts at 0. Version 2 is rewritten as version 3: the same rows, ids, nonce, and commit id are kept, the index-catalog root is 0, and inline free ids past the 1004-id capacity move onto a freelist directory. A version 3 file is left unchanged and the command fails. Both replacements are checked and then renamed over the database. A failure before the rename leaves the original file. Ordinary statements do not rename the database.

## Locking

The lock file is `PATH.lock` in the same directory as the database. It is created on the first open and is not deleted when the database is closed. The open acquires it before reading, recovering, creating, or migrating.

Two advisory locks are held for the life of the open. `flock` on `PATH.lock` coordinates processes that open the same path. A process-local set also rejects a second open in the same process, because `flock` is owned by the process and would not block a second descriptor there. `flock` on the database file itself stays with that inode, so renaming the database file does not let another process open the renamed inode while the first process still holds it.

The sidecar name follows the path. Migration renames a temporary file onto the locked path, so the sidecar still covers the name other opens use. Renaming a live database to a different path does not move `PATH.lock`. The inode lock is what still blocks another process. A process that ignores advisory locks is not stopped. The locks are not a security boundary. Network filesystems are not supported.

## What this does not promise

`File::sync_all` is the durability mechanism. A failed sync of the commit record is an unknown outcome, not evidence that the record is missing, so that path does not delete the log and does not report a rollback. The tests inject failures between these steps in one process. That is not a power-loss test, and it is not evidence that a filesystem makes `sync_all` atomic with respect to torn pages or a lost directory entry. RustDB assumes a local filesystem that honors `sync_all` and `flock`. Network filesystems are not supported. Copying the database without `PATH.wal` while a checkpoint is in progress can drop a committed statement or produce a file the next open rejects. CRC-32 detects torn and flipped pages. It is not a proof against collision. The file is not shrunk when pages become free. Dropping a table or deleting every row of one table rewrites the pages of that table, because those pages become free. Page 0 is rewritten on every commit that changes anything. There is no repair tool. A one-row `UPDATE` still compares every stored row of that table before it writes the dirty pages.

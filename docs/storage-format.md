# RustDB storage format

Version 1. Every integer field is little-endian. Text is UTF-8. The file is a sequence of fixed 4096-byte pages. Rust struct layout is not written to disk.

A new database is 8192 bytes: a meta page and an empty catalog page.

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
| 20 | `u16` | Flags. Must be 0 in version 1 |
| 22 | `u16` | Reserved. Must be 0 in version 1 |

Page types:

| Value | Name | Role |
| --- | --- | --- |
| 1 | Meta | Page 0. File identity, catalog root, and the inline freelist |
| 2 | Catalog | Table definitions, chained through `next page` |
| 3 | Heap | Table rows |
| 4 | Free | Either a free data page or a freelist directory |

Opening a page with a bad checksum, a mismatched page id, or an unknown type fails. The database is not repaired.

## Meta page

Page 0 is the only meta page. After the header:

| Offset | Type | Field |
| --- | --- | --- |
| 24 | 8 bytes | Magic `RUSTDB\0\0` |
| 32 | `u32` | Format version. Version 1 is the only accepted value |
| 36 | `u32` | Page size. Must be 4096 |
| 40 | `u32` | Page count, including page 0. At least 2 |
| 44 | `u32` | Catalog root page id |
| 48 | `u32` | Freelist overflow page, or 0 |
| 52 | `u32` | Count of free page ids stored in this page |
| 56 | `u32` repeated | Those free page ids |

The file length must be `page count * 4096`. A short file, or a length that is not a multiple of the page size, is truncated. An empty file is initialized instead of rejected.

Free page ids name pages that the next publish may reuse. They are not read as rows. When the inline list is not large enough, `freelist overflow` points at a free directory page. A directory page stores `u32` count at offset 24 and that many page ids starting at offset 28, and its header `next page` continues the directory. One directory page holds 1017 ids. Directory pages themselves are not free data pages.

## Slotted pages

Catalog and heap pages store variable-length cells. The slot directory starts at offset 24 and grows upward. Each slot is two `u16` values: cell offset and cell length. Offset 0 and length 0 mark a free slot. Cells grow downward from the end of the page.

`lower` is `24 + slot count * 4`. Live cells lie in `[upper, 4096)` and do not overlap. A cell that does not fit is rejected after the page is compacted. Compaction packs live cells at the end of the page and keeps their slot indexes, so a deleted slot can be reused.

The largest cell is 4068 bytes (`4096 - 24 - 4`). A larger record is rejected. Rows do not span pages. A table spans pages by storing each row on one page and linking rows together.

A published heap page is packed: the publish path writes live rows into fresh cells and does not leave holes. The slot reuse and compaction behavior is part of the page implementation and is covered by unit tests.

## Catalog cell

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u32` | Table id. Regenerated from 1 in catalog order on each publish |
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

The physical `(page, slot)` of a row changes when the table is published again. The `u64` row id does not. Deleted ids are not reused.

## Publish

A successful `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, or `DELETE` builds a new page image and writes it to a uniquely named temporary file in the same directory as the database. The name looks like `.filename.<pid>.<sequence>.tmp`. It is created with `create_new`, so an existing file is not truncated. The temporary file's contents are synced. The parent directory is then synced, where the operating system allows a directory to be opened and `sync_all`'d. The temporary file is renamed over the database, and the parent directory is synced again.

`SELECT` does not publish. A failure before the rename deletes the temporary file, leaves the previous database file unchanged, and restores the in-memory catalog to the previous statement. A failure while reopening the file after a successful rename does not roll that catalog back, because the new file is already the one a later process would open.

Old heap and catalog pages become free ids in the new image. The next publish reuses those ids, so a table that shrinks and grows again does not make the file grow without bound. The high-water mark is the page count stored in the meta page.

This is not a write-ahead log and it is not a power-loss claim. A crash before the rename leaves the previous file in place; a leftover temporary file is ignored on the next open. A crash during the rename is up to the operating system and the filesystem. RustDB does not claim that a power loss in the middle of a publish is atomic, and it will not try to reconstruct a torn temporary file. The second directory sync makes the new directory entry durable for a later process crash on filesystems that honor directory `sync_all`. It is not evidence that a power loss during that sync is atomic. After a successful return, the new file has a checksum on every page. A later open rejects a checksum failure instead of returning a partial row.

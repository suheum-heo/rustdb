# Milestone 5 design: persistent B+ tree indexes

The implementation on `feat/milestone-5` follows this design. `SELECT` still scans the in-memory table. Choosing an index during planning is Milestone 6.

One correction while implementing: an in-memory database rejects `CREATE INDEX` and `DROP INDEX` instead of keeping a second maintenance path. The tree lives in the page file. New files stay version 2 until `rustdb --migrate PATH`. A version 2 `CREATE INDEX` fails before any log write and names that command. Version 2 to version 3 migration copies the nonce and commit id, spills inline free ids that occupied offsets 4088 and 4092, and writes no redo log.

The measured index numbers are in [benchmarks.md](benchmarks.md). They are maintenance and standalone-tree times. They are not `SELECT` speedups.

A tree walk treats a repeated page id, or a child chain taller than 64 pages, as a corrupt page and returns that error.

## 1. Architecture overview

An index is a B+ tree stored in the same 4096-byte pages as the heap. A leaf entry is a signed integer key and the row's stable `u64` id. Internal nodes hold copies of those pairs as separators and point at child pages. Equal integer keys are kept, ordered by row id. NULL keys are omitted. There is no `UNIQUE` constraint.

A version 2 file stays version 2 until `rustdb --migrate PATH` publishes a complete version 3 image. `CREATE INDEX` on a version 2 file fails before any redo-log write and names that command. After migration, `INSERT`, `UPDATE`, `DELETE`, `DROP INDEX`, and `DROP TABLE` keep every index consistent inside the same statement commit that already writes the heap. The editor dirties the index pages it changes. `Pager::checkpoint` logs those pages, syncs the log, writes them into the database, syncs the database, and truncates the log. There is no second commit protocol and no logical replay of index operations.

The in-memory `Table` stays the execution engine. It does not keep a second copy of the tree. The on-disk tree is the index. The next statement's editor reads it through the pager.

```
--migrate on a version 2 file
  -> validate the version 2 image
  -> write a complete version 3 image to a temporary file
  -> validate that image
  -> rename it over the database
  -> no redo log is written

CREATE INDEX / INSERT / UPDATE / DELETE / DROP TABLE / DROP INDEX
  -> rejected on a version 2 file when the statement touches an index
  -> on version 3: editor dirties heap, catalog, index pages, and page 0
  -> one redo log of those sealed pages, within the frame cap
  -> checkpoint, then the statement returns
```

Milestone 5 delivers:

- Page types and a B+ tree that can be tested without SQL.
- An explicit version 2 to version 3 migration.
- Catalog records and `CREATE INDEX` / `DROP INDEX` on a version 3 file.
- Maintenance on the existing row-edit paths, including a tree built over rows that already exist, bounded by a redo-log frame cap.
- Open-time checks that the tree matches the heap and that every index page is owned once.

Out of scope: composite keys, unique constraints, partial indexes, expression indexes, descending keys, `INCLUDE` columns, index-only scans, index-aware `SELECT`, a NULL side structure, multi-writer concurrency, MVCC, and network access. `CREATE INDEX IF NOT EXISTS`, `UNIQUE`, and an index on more than one column are parse errors.

## 2. Page layouts

Every page is 4096 bytes and keeps the 24-byte header from [storage-format.md](storage-format.md). The page id is the page's index in the file. The checksum is CRC-32/ISO-HDLC over all 4096 bytes with the checksum field set to zero while it is computed. `flags` and `reserved` stay 0.

Shared header, which already accounts for type, checksum, and the right-sibling pointer:

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 2 | Page type |
| 2 | 2 | Slot count. Index nodes store 0. They are not slotted pages, so this is not the entry count |
| 4 | 4 | Page id |
| 8 | 4 | Checksum |
| 12 | 4 | `next page`. Leaf: right sibling, or 0 at the right edge. Internal, catalog, and other pages: the existing meaning, which is 0 for an internal node |
| 16 | 2 | `lower`. Index nodes store 24 |
| 18 | 2 | `upper`. Index nodes store 24 |
| 20 | 2 | Flags. 0 |
| 22 | 2 | Reserved. 0 |

`lower == upper == 24` means the slotted directory is empty. `verify` for types 6 and 7 checks that, and does not run the slotted-cell overlap check. The entry count lives in the node preface so it cannot be mistaken for a slot directory length.

New page types:

| Value | Name | Role |
| --- | --- | --- |
| 5 | Index catalog | One slotted cell per index, chained through `next page` |
| 6 | Index internal | Separators and child page ids |
| 7 | Index leaf | Sorted `(key, row id)` entries and sibling links |

A type outside 1..=7 is a corrupt page. Catalog and heap pages keep today's slotted checks.

Index nodes are dense arrays. A slotted cell would add 4 bytes of directory per key and would allow a hole in the key order. Index catalog cells stay slotted because an index definition is variable length.

### Leaf

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 24 | Header. Type 7. `next page` is the right sibling |
| 24 | 2 | Entry count |
| 26 | 2 | Zero |
| 28 | 16 each | `i64` key, then `u64` row id |
| 4092 | 4 | Left sibling, or 0 at the left edge |

The entry region is `4092 - 28 = 4064` bytes. `4064 / 16 = 254` exactly, so a leaf holds **254** entries and the region has no spare byte. Page 0 is never a leaf. Offset 4092 of a leaf is the left sibling, not the table-id mark.

### Internal node

| Offset | Size | Field |
| --- | ---: | --- |
| 0 | 24 | Header. Type 6. `next page` is 0 |
| 24 | 2 | Key count `n` |
| 26 | 2 | Zero |
| 28 | 4 | Leftmost child page id |
| 32 | 20 each | Separator `i64`, separator `u64` row id, right child `u32` |
| 4092 | 4 | Zero |

`n` keys have `n + 1` children. The record region is `4092 - 32 = 4060` bytes. `4060 / 20 = 203` exactly, so an internal node holds **203** separators and **204** children. The last 4 bytes stay 0. An internal node has no sibling pointers.

### Occupancy

`L = 254` leaf entries. `K = 203` internal keys.

A non-root leaf has at least `floor(L / 2) = 127` entries after the statement's rebalance. A non-root internal node has at least `floor(K / 2) = 101` keys. Those floors are the largest ones for which a merge still fits:

- Leaf merge moves entries only. The fullest legal merge is 127 + 127 = 254. An underflowing leaf has at most 126 entries, and a sibling that cannot donate has at most 127, so the merge that actually runs is at most 126 + 127 = 253.
- Internal merge pulls the parent separator down between the two nodes. 101 + 1 + 101 = 203. A floor of 102 would be 102 + 1 + 102 = 205, which does not fit.

The root is the exception. A root leaf holds 1..=254 entries. A root internal node holds 1..=203 keys and therefore 2..=204 children. Zero entries are not stored: the catalog root page id is 0 and the page is free. A root that has one child is replaced by that child.

### Split, borrow, merge, contraction

Leaf split of a full leaf plus one new pair (255 entries): the left page keeps **127** and the right page receives **128**. Both are at least 127. The separator copied to the parent is the first entry of the right page. The original page stays the left leaf. A new page is the right leaf. Pages do not change type. When the splitting leaf was the root, a new internal page becomes the root and the catalog root id changes.

Internal split of a full node plus one new separator (204 keys, 205 children): promote the separator at index **101**. The left page keeps keys `0..101` (101 keys, 102 children). The right page receives keys `102..204` (102 keys, 103 children). The promoted separator is stored only in the parent. It is the minimum entry of the right page's leftmost subtree. A split of the root allocates a new internal root the same way.

Borrow runs before merge. A leaf sibling can donate when it has at least 128 entries, so it still has 127 afterward. An internal sibling can donate when it has at least 102 keys. The right sibling is asked first, then the left. A leaf borrow moves one entry and sets the parent separator to the right node's new first entry. An internal borrow rotates one separator through the parent.

Otherwise the nodes merge, the emptied page returns to the freelist, and the parent separator is removed. The parent then uses the same underflow rules. When the root has one child, that child becomes the root and the old root page is freed. This repeats until the root is a leaf or the tree is empty.

## 3. Key encoding and NULL semantics

The indexed value is the existing `INTEGER` column: a little-endian `i64`. Order is signed numeric order. The bytes of a negative key are not compared with `memcmp`. Equal keys are ordered by ascending `u64` row id.

Row ids in one table are unique and never 0, and `DELETE` does not reuse them, so `(key, row id)` is unique inside one index. A second copy of the same pair is corrupt. Duplicate integer keys are a normal, supported case.

NULL is not an integer and is not stored.

- `INSERT` of NULL writes the heap row and writes no leaf entry.
- `UPDATE` from NULL to an integer inserts one entry.
- `UPDATE` from an integer to NULL deletes that entry and inserts nothing.
- `UPDATE` from NULL to NULL does not touch the tree.
- `UPDATE` from an integer to the same integer does not touch the tree.
- `UPDATE` from one integer to another deletes the old pair and inserts the new pair.
- `DELETE` of a NULL key removes no leaf entry.
- `CREATE INDEX` skips existing NULL rows. A column that is entirely NULL has catalog root page id 0.

`IS NULL` stays a scan of the table in Milestone 6. This index has no NULL side structure for that predicate to use. `WHERE age = NULL` remains unknown and matches no row. No `UNIQUE` constraint is created by `CREATE INDEX` or by anything else in this milestone. Two rows may share an integer key.

Text and boolean columns are rejected at `CREATE INDEX`. The error names the column and the type. An expression, a descending key, a partial `WHERE`, and a second column are rejected at parse time.

## 4. B+ tree algorithms

The tree module takes a pager and a root page id. Insert and delete return the new root page id when the root grows, shrinks, or disappears. They do not sync the file. The editor seals the final page images. The log never contains the intermediate split, only the pages that are dirty when the statement finishes.

### Invariants

- Every stored key lives in a leaf. An internal separator is a copy of the smallest entry in the right child's subtree.
- Entries in one node are strictly increasing by `(key, row id)`.
- Child page ids are non-zero, have the right type, and appear once in the file.
- Leaf siblings point at each other. The right neighbor's entries are all greater. The left neighbor's entries are all smaller.
- After the statement's rebalance, every non-root node meets the floor in section 2.
- The catalog root id is 0, a leaf, or an internal node.

### Search

Binary search inside a node. Descent for `(key, row id)` chooses the child before the first separator that is greater than the probe. The last child receives every greater probe.

Key-only lookups use a row-id sentinel:

- The first entry for `key` is the successor of `(key, 0)`. Real row ids start at 1.
- The end of `key` is the successor of `(key + 1, 0)`, or the end of the chain when `key` is `i64::MAX`.

Equality returns those row ids in ascending order. A range descends to the lower bound and then walks sibling links. Ascending uses the right link. Descending uses the left link. Inclusive and exclusive bounds are both representable.

### Direct API used by tests

Search, range scans, both iteration orders, insert, and delete are callable without SQL. The executor does not call them from `SELECT`.

## 5. Index catalog and version 3 migration

Index definitions live on their own catalog chain. A table cell stays the version 2 encoding, and `decode_catalog` still requires that cell to consume every byte.

### Why migration is a separate publication

`wal::recover` reads the database's page 0 before it parses the log. The shipped version 2 reader continues into recovery when that page is version 2. It then verifies every frame with the version 2 page rules. A version 3 frame is either an unknown page type, which that reader reports as a corrupt page, or a page 0 whose version field it does not accept. The check happens too late to be a clean refusal, and it happens only when page 0 on disk is still version 2. Once a checkpoint has overwritten page 0 and not the rest of the file, the same reader stops at `unsupported database format version 3` and never reads the log that would repair the mixed pages.

A page 0 version field inside one redo record does not close that window. The version 2 reader decides whether to parse the log from the page 0 already in the database file. Publishing version 3 by renaming a finished image does close it: the path names either the complete version 2 file or the complete version 3 file, and the migration writes no log.

### Command

`rustdb --migrate PATH` keeps its current version 1 behavior: a version 1 file becomes a version 2 file, with a new nonce and commit id 0. A second run is required to go on to version 3. The source version selects the target.

| Source | Result |
| --- | --- |
| Version 1 | Today's migration to version 2. Unchanged |
| Version 2 | Migration to version 3, specified below |
| Version 3 | Error: `already format version 3` |
| Any other version | `unsupported database format version N` |
| Missing file | Error naming the path. No file is created |
| Non-empty `PATH.wal` | Error: `refusing to migrate a database that has a redo log`. The file is not rewritten |

`CREATE INDEX` and `DROP INDEX` on a version 2 database return an error that names `rustdb --migrate PATH` and do not write a log. Ordinary `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE`, and `DROP TABLE` on a version 2 file stay available, so a Milestone 5 binary can still maintain a version 2 database. A new database created by this binary is still version 2. It becomes version 3 only through migration. A Milestone 4 binary can open it until that migration.

### Version 3 image

Migration loads the version 2 file with the existing strict checks, then copies every page into a new image. Row ids, table ids, the next row id of each table, column order, heap bytes, and the table-id high-water mark are copied. The nonce and commit id are copied as well. Migration is not a redo-log commit, so it does not add one to the commit id.

Page 0 in the new image:

| Offset | Size | Version 3 meaning |
| --- | ---: | --- |
| 32 | 4 | Format version `3` |
| 52 | 4 | Inline free-id count, at most 1004 |
| 72 | 4 each | Inline free-page ids |
| 4088 | 4 | Index-catalog root, or 0 |
| 4092 | 4 | Next table id, the same high-water rule as version 2 |

A version 3 writer stores at most **1004** inline free ids, because offset 4088 is the catalog root and offset 4092 is the table-id mark. `1004 = (4088 - 72) / 4`.

When the version 2 inline list uses offset 4088 or 4092, those ids are free-page ids and must be kept:

- Count 1005: the id at offset 4088 moves onto a freelist directory page.
- Count 1006: the ids at offsets 4088 and 4092 both move onto a directory page. Offset 4092 in the version 2 page was not a table-id mark. The new mark is one past the highest table id still in the catalog, which is the same derivation version 2 already uses when the stored mark is 0. The deleted-id history of that legacy file is still unrecoverable. Migration does not invent it.

The index-catalog root is 0. Migration creates no index and no index page. The first `CREATE INDEX` allocates the catalog page.

Before the rename, the builder reads the new image back and runs the version 3 open checks: page 0 version is 3, the page count matches the byte length, the nonce is the version 2 nonce, the commit id is the version 2 commit id, the catalog root is 0, every copied row id and value is still present, and the strict page walk succeeds. A failed check deletes the temporary file and leaves the version 2 database in place. The command returns success only after that validation and the rename.

### Crash matrix

The temporary file is created in the database directory, synced, and renamed over the path, which is the existing `persist` publication. A leftover `*.tmp` is not a database. The redo log is empty throughout, because migration refuses a non-empty log and writes none.

| Crash point | Path names | Temporary file | WAL | Version 2 binary | Version 3 binary |
| --- | --- | --- | --- | --- | --- |
| Before migration | Complete version 2 | Absent | Empty | Opens | Opens as version 2. `CREATE INDEX` asks for migration |
| During the temporary write, or after a failed sync of that file | Complete version 2 | Absent or partial. A failed write removes it | Empty | Opens the version 2 file | Same. Migration can be retried |
| Temporary file synced, rename not done | Complete version 2 | Complete unpublished version 3 image | Empty | Opens the version 2 file and ignores the temporary name | Same. Retry validates a new image and renames it |
| Rename has replaced the path. Parent-directory sync has not finished | Complete version 3 image | The temporary name is gone | Empty | `unsupported database format version 3`. The log is not read | Opens version 3. Page 0 and the empty index catalog are valid |
| Rename and parent sync finished | Complete version 3 image | Gone | Empty | `unsupported database format version 3` | Opens version 3 |
| Caller starts migration while a log exists | Unchanged version 2 | Not created | Unchanged, non-empty | Migration returns the redo-log error. Open uses today's version 2 recovery | Same |

There is no crash state in which the path names a mixture of version 2 pages and version 3 pages. There is no crash state in which a version 2 page 0 and a version 3 redo log exist together. A version 2 binary that sees the published file rejects the version in page 0 and returns before `wal::recover`.

### Index catalog cell

On type 5 pages, after migration has created a place for them:

| Offset | Type | Field |
| --- | --- | --- |
| 0 | `u16` | Name length, then UTF-8 name |
|  | `u32` | Owning table id |
|  | `u16` | Column ordinal, 0-based |
|  | `u16` | Column-name length, then UTF-8 name |
|  | `u32` | Root page id, or 0 when the tree has no pages |
|  | `u8` | Key type. `1` is integer |
|  | `u16` | Index format version. `1` is this leaf and internal layout |

Names use the same folding as tables. The name is unique among indexes and may match a table name. On open, the ordinal must exist and the stored column name must match the table. A mismatch is a corrupt catalog. Unknown index format versions and unknown key types are unsupported-index errors.

`DROP INDEX name` frees that tree and removes its cell. `DROP TABLE` does that for every index of the table, in the same commit as the heap drop. A missing index name, `IF EXISTS`, `CASCADE`, and `RESTRICT` are rejected.

### `CREATE INDEX`

```sql
CREATE INDEX idx_users_age ON users(age);
```

Accepted on a version 3 file: one name, one table, one unqualified integer column. No `UNIQUE`, no `IF NOT EXISTS`, no method, no included columns, no predicate, no expression, no sort direction.

| Situation | Result |
| --- | --- |
| Database is version 2 | Error naming `rustdb --migrate`. No log |
| Name already used by an index | Error, no log |
| Table or column missing | Error, no log |
| Column is text or boolean | Error naming the type. No log |
| Any clause outside the form above | Parse error |
| Version 3, new name, integer column | One commit, if the frame cap allows it. Existing non-NULL values are inserted. NULL values are skipped |

## 6. Row-ID resolution

Each leaf entry is `(i64 key, u64 row id)`. It is not a heap `(page, slot)` and it is not a position in a `Vec`.

`Table` stores `row_ids` parallel to `rows`. `DELETE` goes through `retain_indexes`, which compacts both vectors and drops the ids of removed rows. A stored vector index would name a different row after that compaction. `UPDATE` keeps the id at the same vector position. The id is assigned at insert, is never 0, and is not reused. SQL does not return it.

The editor's `LiveTable::links` remembers the current `(page, slot)` for the life of the process. That list is rebuilt from the heap chain on open. A row's slot changes when an update grows the cell or when a neighbor is unlinked, so it is the wrong durable reference.

Milestone 5 resolves a row id by scanning that table's in-memory `row_ids` vector. Direct tests use this to show that a lookup found the intended rows. `SELECT` does not use it.

A later physical locator can be added without rewriting these leaves. It would be a separate map from row id to `(page, slot)`, updated only when a row moves, in the same redo log as that move. Lookups would still return row ids. The locator would be consulted by a future fetch, not by the comparison order. Leaf layout version 1 stays valid. The locator gets its own page type and its own index-format or page-format version so an older Milestone 5 reader refuses it cleanly instead of reading it as a tree node.

## 7. WAL integration and the frame cap

On a version 3 file, index maintenance hooks `append_row`, `rewrite_row`, `unlink_row`, and `drop_table`, plus `create_index` and `drop_index`.

| Statement | Index pages in the same log as the heap change |
| --- | --- |
| `INSERT` of an integer | Insert `(key, new row id)` into each index on that column |
| `INSERT` of NULL | No index page |
| `UPDATE` of an indexed column, NULL to integer | Insert the new pair |
| `UPDATE` of an indexed column, integer to NULL | Delete the old pair |
| `UPDATE` of an indexed column, integer to a different integer | Delete the old pair and insert the new pair |
| `UPDATE` that leaves every indexed value unchanged, including NULL to NULL and a non-indexed column | No index page. The dirty set stays page 0 and the heap page when the row does not move |
| `DELETE` of an integer key | Delete that pair |
| `DELETE` of a NULL key | No index page |
| `DROP INDEX` | Free that tree and remove its catalog cell. The heap is unchanged and is not rewritten |
| `DROP TABLE` | Free every index of that table and free the heap, one commit |
| Restart | Open replays a durable log, then checks the tree against the heap |
| Partial checkpoint | The log still has every final page. Replay writes them all, then validation runs |

The log stores final sealed images. A split and the following parent update are one dirty set. Recovery does not resume a half-finished split.

A failure before the log sync is `BeforeCommit`. The executor rolls the in-memory table back, the editor is dropped, and the database bytes stay as they were. A failed sync of a commit record, or any failure after a durable trailer, is `AfterCommit`: the log stays, later statements are refused, and reopen installs the whole committed image. That image includes the heap change and the matching index change together. There is no committed state in which one of them was installed and the other was not.

### Frame cap

Every commit, including `CREATE INDEX`, is limited to **8192** frames. A frame is the existing 4-byte page id plus one 4096-byte page, 4100 bytes. With the 64-byte header and 32-byte trailer, the largest log this writer will encode is:

```text
64 + 8192 * 4100 + 32 = 33,587,296 bytes
```

The editor also holds those pages before the log is built, which is 8192 × 4096 = 33,554,432 bytes of page memory, plus the encoded buffer during the write. The next page allocation that would make the dirty set larger than 8192 fails with `redo log frame limit of 8192 exceeded`. That failure happens before `write_log`. It is `BeforeCommit`. The temporary tree is discarded with the editor. The database file and any previous log are unchanged.

8192 frames cover a tree of about two million integer keys: 8000 leaf pages hold 8000 × 254 = 2,032,000 entries, and the internal nodes above them are about 8000 / 204 ≈ 40 pages, plus page 0 and the catalog. A larger `CREATE INDEX` is rejected. The build is not split across several commits, and the writer does not start a log it does not intend to finish.

A `CREATE INDEX` that stays under the cap and reaches a durable trailer is the new committed database. Reopen installs that complete index. A build that stops before the trailer leaves the previous image.

## 8. Recovery and corruption handling

Version 3 recovery still replays sealed pages and still truncates a log only after the database sync. It gains a version gate so a log and a file of different versions are never applied.

`recover` reads page 0 of the database first.

| Database page 0 | Log | Action |
| --- | --- | --- |
| Version 2 | Empty or missing | Open as version 2 |
| Version 2 | A version 2 log: page 0 frame version 2, page types 1..=4, nonce and commit id match today's rules | Today's replay or discard |
| Version 2 | Any frame with page type 5, 6, or 7, or a page 0 frame whose version is not 2 | Leave the log. Open fails with an unsupported-log error. Do not apply and do not truncate |
| Version 3 | Empty or missing | Continue to validation |
| Version 3 | Page 0 frame version 3, nonce and commit id match, each page id once | Replay every frame, sync, truncate, then validate |
| Version 3 | Page 0 frame version is not 3, nonce or commit id disagrees, a page id repeats, or the trailer is damaged | Leave the log. Open fails. Do not truncate a damaged trailer |
| Any other file version | Anything | `unsupported database format version N`, before recovery |

A version 2 binary never reaches this table for a published version 3 file, because its `read_meta` rejects version 3 before `recover`. The rows in the table are the version 3 binary's rules, including when it is asked to open a version 2 file that somehow has a version 3 log beside it.

After a successful replay, `load` validates. Validation failure means the committed image is corrupt. Recovery does not delete pages or rebuild a tree from the heap. The open fails and the file is not rewritten.

The structural walk marks page 0, the table catalog, freelist directories, heap pages, the index catalog, and every page reached by child pointers and leaf sibling links. Every other page id must be on the freelist and have type free. The following are corrupt even when each page checksum is valid:

- An index node whose count, width, `lower`, `upper`, or unused tail disagrees with section 2.
- Entries that are not strictly increasing, or a repeated `(key, row id)`.
- A separator that is not the minimum entry of the right child's subtree.
- A child or sibling that is missing, is page 0, or has the wrong type.
- A page reached twice, or a cycle in the child pointers or the leaf list.
- A leaf chain whose order disagrees with an in-order walk of the internal nodes.
- A non-root node below its occupancy floor.
- An index page that is not reachable from exactly one root and is not on the freelist.
- A page that is both in a tree and on the freelist, or is claimed by two indexes.
- A catalog cell whose table id, ordinal, or column name does not match the table catalog, or whose root page has the wrong type.
- An index entry whose row id is absent from that table's heap.
- An index key that differs from the integer stored in that heap row.
- A non-NULL integer in an indexed column with no matching leaf entry.
- A leaf entry for a row whose indexed value is NULL.

Version 2 files skip the index walk. A type 5, 6, or 7 page in a version 2 file is corrupt, not an index the reader quietly uses. Checksums remain a torn-page check. They are not the structural check.

## 9. Test plan

Tree unit tests use an in-memory pager, real 4096-byte pages, and no `fsync`. SQL tests cover migration, maintenance, restart, and the redo log. Default `cargo test` does not insert thousands of rows as separate durable statements.

### Tree shape

| Case | What it checks |
| --- | --- |
| Empty tree | Root id 0. Equality, range, and both iteration orders return nothing |
| One leaf | Insert, search, delete back to empty, page freed, root id 0 |
| Root split | 255 distinct keys. Root becomes internal. Children hold 127 and 128 |
| Thousands of keys, both insertion orders, and one deterministic random order | The scan equals the sorted pairs. Occupancy holds |
| Duplicate keys | Equality and a range that starts or ends inside the run return the row ids in order |
| Delete and rebalance | Underflow, borrow from the right, borrow from the left, leaf merge, internal merge, root contraction to a leaf, then to empty |
| Occupancy equations | Production constants are 254 and 203. 127 + 127 = 254. 101 + 1 + 101 = 203. A test-only smaller fanout builds a height-3 tree |

### Migration

- Version 1 migration still produces version 2.
- Version 2 migration copies row ids, table ids, values, nonce, and commit id, and publishes version 3 with index-catalog root 0.
- A version 2 file with 1005 inline free ids, and one with 1006, keeps every free id and still opens after migration.
- A non-empty WAL refuses migration and leaves the file.
- A crash injected before the rename leaves the version 2 file openable and the temporary file unpublished.
- After a successful rename, a reader that only accepts version 2 reports `unsupported database format version 3` and does not read the log.
- The version 3 open validates page 0 and the empty index catalog.
- `CREATE INDEX` on the unmigrated file writes no log.

### SQL, maintenance, and durability

- Missing table, missing column, text, boolean, duplicate index name, and a second column.
- NULL insert, NULL-to-integer, integer-to-NULL, NULL-to-NULL, and an update of a non-indexed column. The last two do not dirty an index page.
- Duplicate keys, restart, `DROP INDEX`, and `DROP TABLE`.
- Incomplete WAL during an indexed insert: reopen shows the previous tree.
- Stop after the log sync, and stop after one database page write. Reopen installs the whole committed tree.
- A damaged trailer is kept and open fails.
- A build that would need frame 8193 fails before `write_log`. The file bytes and the previous log length are unchanged.

### Corruption

Reseal a page so the checksum is valid, or leave it invalid when the checksum itself is the case:

- Unsorted keys, a wrong separator, a sibling cycle, a repeated child, a root aimed at a heap page, an orphan index page, a page on both a tree and the freelist.
- A leaf row id that is not in the heap, a leaf key that disagrees with the heap value, and a non-NULL heap value missing from the leaf.
- Catalog ordinal or column name mismatch, and index format version 99.

Each open fails. The file is not rewritten.

## 10. Benchmark plan

Numbers are recorded only after a release run, in [benchmarks.md](benchmarks.md), with the machine, compiler, and command. This design does not estimate those times.

The harness uses `bench (id INT, age INT, email TEXT, note TEXT, active BOOLEAN)`, the same email and 64-byte note as the Milestone 4 load, and sizes 100, 1,000, and 10,000. Each size is built twice: once after `CREATE INDEX` on `age`, and once without an index. The indexed file is migrated to version 3 first.

Record wall time, including `fsync`, for equality lookup and range lookup through the direct API, for the durable insert load, for updates of `age` and of `active`, and for deletes. Also record frame counts, file length, page count, and the height after `N` keys. The `active` update is the check that a non-indexed change adds no index frames. `SELECT` times stay in the old tables. This milestone does not claim that a query got faster.

The run is one machine and one load per cell. It is not a power-loss measurement.

## 11. Implementation phases

Each phase keeps `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` green. Version 2 SQL behavior stays available. `SELECT` does not consult an index.

1. **Pages.** Types 5, 6, and 7. Encode, decode, and `verify`. Assert the 254 and 203 capacities from the offsets in section 2.
2. **Tree.** Search, insert, split, delete, borrow, merge, and root contraction on an in-memory pager, including the reduced-fanout height-3 case.
3. **Migration.** Version 2 to version 3 through the temporary file and rename. The crash points in the matrix. Version 1 migration stays a version 2 result. `CREATE INDEX` on version 2 writes nothing.
4. **Catalog and SQL.** Index cells, parser, `CREATE INDEX`, and `DROP INDEX` on version 3. Build a tree for existing rows. Enforce the 8192 frame cap before any log write.
5. **Maintenance.** Hook append, rewrite, unlink, and drop table. Cover every NULL and integer transition, and a non-indexed update that leaves index pages clean.
6. **Open checks.** Reachability, freelist conflicts, and agreement between every leaf and the heap.
7. **Redo log.** Version gate in `recover`. Incomplete log, partial checkpoint, and damaged trailer on an indexed statement.
8. **Benchmark.** One measured run. Record the actual numbers. Do not invent them.

## 12. Remaining tradeoffs

These are accepted limits of this design, not open product choices.

**NULL keys are absent.** `IS NULL` in Milestone 6 scans the table. Adding a NULL bucket later is a new structure and a new index format version. It does not change the integer leaf.

**There is no unique constraint.** Duplicate integer keys are stored on purpose. Enforcing uniqueness is a different statement result and a different failure in the middle of a multi-row insert.

**Version 3 is a published image, not a redo record.** Migration copies the file and renames it. A large version 2 database pays for one full rewrite at migration time. Later commits are incremental. A new database is version 2 until that command runs, including a database created by this binary.

**Legacy version 2 files still have no deleted-table-id history.** Migration of a file whose table-id mark is 0 writes one past the highest remaining id. It does not recover ids that were dropped before the mark existed.

**The frame cap refuses a tree past about two million integer keys in one commit.** Raising 8192 raises the memory and log bound in the same proportion. The build is not streamed across commits.

**The row-id locator is deferred.** Leaves stay `(key, row id)`. A physical map can be added beside them later.

**`SELECT` ignores indexes.** A correct index does not change query results or query cost until Milestone 6.

**Durability is still `File::sync_all` plus rename for migration.** The crash matrix describes process failure between these steps. It is not a power-loss proof, and it assumes a local filesystem that honors `sync_all`, rename replacement, and `flock`.

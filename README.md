# RustDB

RustDB is a small relational SQL engine written in Rust. It parses a limited SQL subset, checks types against a table schema, and runs queries from a REPL. It does not use SQLite, PostgreSQL, or any other database engine.

`cargo run` starts an empty in-memory database. `cargo run -- --database PATH` opens a page file, creating it when the path is missing or empty. The SQL behavior is the same in both modes.

## Architecture

SQL text enters through the REPL or `Database::execute`. `sqlparser` (the `sqlparser` crate, version 0.64) turns that text into its own syntax tree. The parser module lowers the supported subset into a small internal statement tree and rejects everything else. The executor only sees that internal tree, so an unsupported clause cannot be skipped by accident.

```text
SQL text
  -> sqlparser
  -> internal statement tree (parser)
  -> expression binder (expr)
  -> access path (plan)
  -> executor
  -> in-memory catalog
  -> dirty pages, redo log, then checkpoint (file mode)
  -> rendered result
```

| Module | Responsibility |
| --- | --- |
| `value` | `INTEGER`, `TEXT`, `BOOLEAN`, and `NULL` |
| `schema` | Column names, types, and insert column mapping |
| `table` | One table: schema, insertion-ordered rows, and stable row ids |
| `database` | Catalog of named tables, in memory or backed by a file |
| `storage` | Pager, slotted pages, B+ tree indexes, redo log, and incremental page edits |
| `ast` | Statements and expressions the executor understands |
| `parser` | `sqlparser` boundary and identifier normalization |
| `expr` | Bind columns, check types, and evaluate three-valued logic |
| `plan` | Choose a sequential scan or one integer index scan for a single table |
| `executor` | `CREATE`, `DROP`, `INSERT`, `SELECT`, `UPDATE`, `DELETE`, `EXPLAIN`, integer indexes, and a two-table `INNER JOIN` |
| `format` | ASCII result tables |
| `repl` | Interactive session |
| `error` | Parse, schema, type, and storage errors |

While a statement runs, rows live in a `Vec`. That keeps `INSERT` validation and tuple projection the same as the in-memory engine. After a successful mutating statement, file mode publishes those rows into 4096-byte slotted pages. The byte layout is documented in [docs/storage-format.md](docs/storage-format.md). It uses explicit little-endian integers and UTF-8, not Rust's in-memory representation.

`NULL` is its own value, not a sentinel integer or an empty string. A column reference keeps the column's declared type even when the stored cell is null. A `NULL` literal has no type and is compatible with every column.

Unquoted identifiers are folded to lowercase. Quoted identifiers keep their case and match exactly, so `Users` and `"Users"` are different tables.

`INSERT` checks every row before it appends any of them. `UPDATE` and `DELETE` build the replacement rows and install them only after every row succeeds. A type error or a missing column leaves the table unchanged. In file mode a failure before the commit record is synced restores memory when the log is still incomplete. A failed sync of that record does not prove the record is absent: memory stays, the log stays, and the process refuses further statements until the file is reopened. A failure after a successful log sync does the same. Separate statements in one string are applied one at a time: if the second statement fails, the first one stays, including on disk.

`SELECT`, `UPDATE`, and `DELETE` share one filter. The `WHERE` expression is bound to column indexes and type-checked before any row is read. An empty table still reports a missing column or a comparison between different types. `AND` and `OR` are type-checked on both sides; a false or unknown left side does not hide a type error on the right.

A query keeps a row only when `WHERE` evaluates to TRUE. FALSE and UNKNOWN both drop the row.

`SELECT` from one table, or from one `INNER JOIN`, can group rows before that sort. `COUNT(*)` counts every row that passes `WHERE`. `COUNT(column)` skips NULL. `SUM` adds non-NULL integers and returns NULL when none remain. Without `GROUP BY`, an empty input still returns one aggregate row. With `GROUP BY`, groups come out in the order their key was first seen, and NULL keys share one group. `HAVING` then keeps a group only when it evaluates to TRUE. The same aggregate call shares one accumulator in the select list, `HAVING`, and `ORDER BY`. `SELECT` then sorts, then applies `LIMIT`, then projects. `ORDER BY` can name a grouped column or an aggregate that is absent from the select list. The sort is stable, so equal keys stay in the order the groups were produced. When the filter is an equality or a bounded range on one indexed `INTEGER` column of a single table, file mode reads candidate row ids from that B+ tree and then evaluates the original `WHERE`. Candidates are put back into insertion order before that filter, so a query without `ORDER BY` stays in insertion order. `SELECT` does not rewrite the file.

An `INNER JOIN` of two tables keeps a pair only when `ON` is TRUE. The left table is the outer input. Its rows are visited in insertion order, and the matching inner rows are visited in insertion order. `WHERE` runs after `ON`. A query that does not group then sorts, applies `LIMIT`, and projects. NULL never equals NULL, so a NULL join key matches no row. `IS NULL` can match. A stored `ON` cell that is not NULL and not the column's declared type is joined with the nested loop for that execution, so a comparison error is not skipped by a hash or index probe. `EXPLAIN` still reports the plan chosen from the schema and does not show that fallback. Duplicate keys produce one output row per matching pair. Pairs that fail `ON` or `WHERE` are not stored. Without `ORDER BY`, `LIMIT` keeps the first matches in that order and still evaluates the remaining pairs. An aggregate join feeds each kept pair into the grouping hash and does not store the pair. When the hash join builds the outer table, it still buffers match positions so groups stay in outer-then-inner order. `HAVING`, `ORDER BY`, and `LIMIT` run after every pair has been accumulated. The join does not write the file.

An equality between two `INTEGER`, `TEXT`, or `BOOLEAN` columns builds an in-memory hash table from the smaller input. Equal sizes build the inner table. NULL keys are omitted. Each hash bucket keeps one group per distinct key, so a shared fingerprint does not mix two values. Duplicate keys keep every pair, and the original `ON` runs after the key matches. Matches come out in outer insertion order, then inner insertion order. When that equality is an integer column with an inner index, the join probes the index instead if the outer table is empty or the inner table has more than 64 times as many rows. The earliest matching index in catalog order supplies the key. Reversed operands are the same equality. Candidates are put back into the inner table's insertion order, and the original `ON` is evaluated again. A NULL outer key skips the probe. A corrupt index fails the statement. `OR`, a non-equality, and `IS NULL` alone read every inner row. The nested loop and the hash join copy one input because the catalog stores both tables in one map. The index path copies the outer rows, because each probe borrows the database. `RIGHT JOIN`, `FULL JOIN`, a third table, `USING`, and `NATURAL JOIN` are rejected.

A `LEFT JOIN` or `LEFT OUTER JOIN` of two tables keeps every left row. A pair is kept only when `ON` is TRUE. A left row with no such pair is returned once, with NULL for every right column, and `WHERE` runs after that decision. Filtering a matched pair does not add a null-extended row. A same-type column equality hashes the right table, including a reversed equality and an equality inside `AND`. NULL keys are omitted. Exact key equality selects the bucket group, then the original `ON` runs, so a residual predicate can reject a candidate. The build side is always the right table. Left rows stream in insertion order, and matching right rows stay in insertion order, so there is no pair-position buffer. A stored cell that is not NULL and not the column's declared type uses the nested loop, so a comparison error is not skipped. `OR`, a range, `IS NULL`, and any other predicate stay a nested loop. An integer equality on a right-table B+ tree is `IndexLeftNestedLoopJoin` when the left table is empty or the right table has more than 64 times as many rows. That factor is separate from the inner join's 64× rule. A smaller right table stays a hash, because building it is cheaper than one tree probe per left row. This is a size rule, not a cost model. The earliest matching index in catalog order supplies the key. Reversed operands are the same equality. A NULL left key does not probe. Candidates come back in right insertion order, and the original `ON` runs again, so a residual can reject a key hit. A corrupt page, a broken leaf link, a missing row id, a repeated row id, or a key that disagrees with the heap fails the statement and is not replaced by the nested loop. The probe checks the entries it returns. It does not walk the heap looking for a posting that is missing. Open compares the whole tree with the heap, so a file that is missing a posting fails to open. A posting removed after open can omit a match without an error. `EXPLAIN` still names the schema plan and appends the index name only for that strategy. `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX`, with `GROUP BY` and `HAVING`, read those logical rows through the same accumulator as an inner join. `COUNT(*)` counts a null-extended row. `COUNT` of a column, and `SUM`, `AVG`, `MIN`, and `MAX`, skip NULL. The joined rows are not stored for the aggregate. `COUNT(DISTINCT column)`, `SUM(DISTINCT integer)`, and `AVG(DISTINCT integer)` use that same accumulator. A NULL argument, including every column of a null-extended right side, is not stored in the per-group set. Equal values count once inside one group, and another group keeps its own set. `SELECT DISTINCT` projects each surviving logical row and keeps the first copy of that tuple, so NULL matches NULL and a null-extended row can collapse with a match. `GROUP BY`, `HAVING`, and aggregates stay off `SELECT DISTINCT`. `MIN(DISTINCT ...)` and `MAX(DISTINCT ...)` stay rejected.

A qualifier is the alias when the query wrote one, and the table name otherwise. The catalog name is not visible beside its alias. An unqualified column must match exactly one input. `SELECT *` lists the outer columns and then the inner columns, using each column's declared name, so the same name can appear twice. `SELECT alias.*` lists one input. `ON` accepts the same comparisons, `AND`, `OR`, `NOT`, `IS NULL`, and `IS NOT NULL` as `WHERE`.

## Durability

Each successful `CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`, `DROP INDEX`, `INSERT`, `UPDATE`, or `DELETE` writes the pages that statement changed into `PATH.wal`, syncs that same file, writes those pages into the database, syncs the database, and only then truncates the log. A successful sync of the log is the commit point. A failed sync is not treated as a rollback. `SELECT` does not write the log. Unchanged pages stay as they are, including other tables' heap pages.

The process holds an advisory `flock` on `PATH.lock` and on the database file for as long as it has the database open. The lock file stays after unlock. A second open of the same path fails, including a second open in this process. Renaming the database file does not release the inode lock held by the process that already has it open. `flock` is advisory: a process that does not take it is not stopped. A forked child inherits the lock until it closes that descriptor or starts a new program. The descriptor is close-on-exec.

A crash before the commit record is durable leaves the previous database, and an incomplete log that belongs to the file is discarded on open. A crash after the commit record is durable and before the log is cleared is repaired on the next open by writing the logged pages again. That replay is safe to repeat. If `sync_all` of the commit record returns an error, the statement is not rolled back: the trailer may already be on disk, and the next open can make that statement visible. The caller has to treat that error as an uncertain outcome. The tests inject that error. They do not cut power. A version 1 file is not migrated on open. `rustdb --migrate PATH` rewrites version 1 as version 2, or version 2 as version 3, and keeps row ids and table ids. A new database stays version 2 until that command is run. `CREATE INDEX` on version 2 fails and names the command.

This is not a power-loss proof. The tests stop the commit at chosen steps, including a sync that returns an error after the commit record has been written. They do not cut power. RustDB assumes a local filesystem that honors `File::sync_all` and `flock`. The byte layout is in [docs/storage-format.md](docs/storage-format.md). The commit boundaries and what recovery keeps are in [docs/reliability.md](docs/reliability.md).

A row must fit in one heap cell, at most 4068 bytes. Larger rows are rejected in both modes. There are no overflow pages. A table can still occupy many pages: each row is one cell, and cells are linked in insertion order.

Deleted rows leave free pages that a later statement can reuse, so deleting a large table and inserting a similar one does not grow the file without bound. The file is not shrunk. Inside one page, a deleted slot can be reused. An update that fits in the existing cell keeps that `(page, slot)`.

Every row has a `u64` id assigned at insert. `UPDATE` keeps it. `DELETE` does not reuse it. Row ids are not returned by SQL. Table ids for a file written by this version come from a counter stored at the end of page 0. `DROP TABLE` does not return an id, including after that file is reopened. A file written before the counter existed has no record of deleted ids, so the first new table after opening it can reuse one. Table ids are not returned by SQL either.

## Build and run

Requires a stable Rust toolchain (this project was built with Rust 1.100).

```sh
cargo run
cargo run -- --database ./rustdb.db
cargo run -- --migrate ./rustdb-v1.db
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --all-targets --locked
cargo test --locked
```

GitHub Actions runs those four checks on pull requests and pushes to `main`, and when the workflow is started by hand. It uses the stable toolchain and `Cargo.lock`. Ignored release profiles are not part of that job, and pull requests do not run a fuzzing campaign. The workflow file is in the tree; a green run on GitHub is separate from a local `cargo test`. How to run property tests, the mutation campaign, and recovery checks is in [docs/reliability.md](docs/reliability.md).

With no arguments the REPL uses an empty in-memory database, and exiting drops every table. `--database PATH` creates a version 2 file when it does not exist and reloads it on the next run. `--migrate PATH` rewrites a version 1 file as version 2, or a version 2 file as version 3, and then exits. It does not start the REPL. A version 3 file is left unchanged and the command fails. The REPL prompts with `rustdb> `. End each statement with `;`. A statement can span lines. `exit` and `quit` close the session. SQL errors are printed and the session continues.

Library use:

```rust
use rustdb::{render_result, Database};

fn main() -> Result<(), rustdb::DbError> {
    let mut db = Database::new();
    db.execute("CREATE TABLE users (id INT, name TEXT);")?;
    db.execute("INSERT INTO users VALUES (1, 'Ada');")?;
    let results = db.execute("SELECT name FROM users WHERE id = 1;")?;
    println!("{}", render_result(&results[0]));
    Ok(())
}
```

## Supported SQL

```sql
CREATE TABLE table_name (column_name type, ...);
DROP TABLE table_name [, table_name, ...];
INSERT INTO table_name [(column_name, ...)] VALUES (expr, ...), ...;
UPDATE table_name SET column_name = expr [, column_name = expr, ...] [WHERE condition];
DELETE FROM table_name [WHERE condition];
SELECT * FROM table_name [[AS] alias] [WHERE condition]
    [ORDER BY column_name [ASC | DESC] [NULLS FIRST | NULLS LAST]
        [, column_name [ASC | DESC] [NULLS FIRST | NULLS LAST], ...]]
    [LIMIT count];
SELECT column_name [, column_name, ...] FROM table_name [[AS] alias] [WHERE condition]
    [ORDER BY ...] [LIMIT count];
SELECT select_list
    FROM table_name [[AS] alias]
    [INNER] JOIN table_name [[AS] alias] ON condition
    [WHERE condition]
    [ORDER BY ...] [LIMIT count];
EXPLAIN SELECT ...;
```

Types:

| SQL names | Stored as |
| --- | --- |
| `INT`, `INTEGER`, `BIGINT`, `SMALLINT`, `TINYINT`, `MEDIUMINT` | 64-bit signed integer |
| `TEXT`, `VARCHAR`, `CHAR`, `STRING` | Unicode text |
| `BOOLEAN`, `BOOL` | Boolean |

`INT(11)` and `VARCHAR(n)` are accepted. Display widths and character limits are not enforced.

Every stored column accepts `NULL`. `INSERT` and `UPDATE` both accept a `NULL` literal. A named `INSERT` column list must still mention every column once; there are no defaults. Column constraints, including `NOT NULL` and `PRIMARY KEY`, are rejected rather than ignored.

`WHERE` supports `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `AND`, `OR`, `NOT`, `IS NULL`, `IS NOT NULL`, and parentheses. Comparisons require the same type on both sides. An untyped `NULL` literal takes the other operand's type. Ordering comparisons (`<`, `>`, `<=`, `>=`) are defined for integers and text. Booleans support `=` and `!=` only. Text order is UTF-8 byte order.

### NULL and three-valued logic

Comparisons involving `NULL` evaluate to UNKNOWN, which is itself represented as `NULL`. `age = NULL` matches no row, including rows whose `age` is null. `age IS NULL` matches those rows. `IS NULL` and `IS NOT NULL` are never unknown.

`AND`, `OR`, and `NOT` use the SQL truth tables. `UNKNOWN AND FALSE` is FALSE. `UNKNOWN OR TRUE` is TRUE. Every other combination with UNKNOWN stays UNKNOWN. `NOT UNKNOWN` is UNKNOWN.

`WHERE` retains a row only for TRUE:

```sql
SELECT id FROM users WHERE age = NULL;          -- no rows
SELECT id FROM users WHERE age IS NULL;         -- rows whose age is null
SELECT id FROM users WHERE true OR age = NULL;  -- every row
SELECT id FROM users WHERE NOT age IS NULL;     -- rows whose age is not null
```

`NULL > 1` is UNKNOWN. `NULL > true` is still a type error, because the resolved type is BOOLEAN and ordering is not defined for booleans. `age > 'x'` is a type error even when some `age` cells are null, because the column's declared type is INTEGER.

Result tables print a null cell as `NULL`.

### UPDATE and DELETE

```sql
UPDATE users SET age = 31 WHERE id = 2;
UPDATE users SET name = 'Unknown' WHERE name IS NULL;
UPDATE users SET age = 18;
UPDATE swap SET a = b, b = a;
DELETE FROM users WHERE age < 18;
DELETE FROM users WHERE name IS NULL;
DELETE FROM users;
```

Each assignment is type-checked against the target column. The right-hand side of every assignment is evaluated against the original row, so `SET a = b, b = a` swaps the two values. Assigning the same column twice is rejected. The reported count is the number of rows matched by `WHERE`, including rows whose new values equal the old ones. `UPDATE` and `DELETE` without `WHERE` affect every row. An empty table reports `0 rows updated` or `0 rows deleted`.

When `WHERE` contains an equality on an indexed `INTEGER` column, the statement looks up that key and then walks the heap for every row storing the same integer. Those two sets must be the same row ids, in heap order, before any row changes. A missing posting, an extra posting, a stale row id, or a key that disagrees with the heap fails the statement and does not publish. Reversed operands and an equality inside `AND` use the same lookup. The earliest index in catalog order wins when more than one equality matches. The original `WHERE` then runs on that complete set, so a residual predicate can reject one. Duplicate keys are all candidates, in heap order. A `NULL` indexed value is not in the tree and does not match `column = integer`. `OR`, a range, `IS NULL`, a non-indexed column, and a `TEXT` or `BOOLEAN` equality keep the heap scan. A wrong-typed `WHERE` cell uses that scan before the probe, so the comparison error is unchanged. `EXPLAIN` is still only defined for `SELECT`. This is a shape rule, not a cost model, and it does not change join planning. The heap walk makes an indexed mutation read every row of the chosen column.

### ORDER BY and LIMIT

```sql
SELECT * FROM users ORDER BY age ASC;
SELECT name FROM users ORDER BY age DESC LIMIT 5;
SELECT * FROM users ORDER BY age DESC, name ASC;
SELECT * FROM users ORDER BY age ASC NULLS FIRST;
```

`ASC` is the default. NULL is greater than every non-NULL value, which is PostgreSQL's default: `ASC` sorts nulls last and `DESC` sorts nulls first. `NULLS FIRST` and `NULLS LAST` are absolute; `DESC` does not flip an explicit null order. Boolean order is `false` then `true`. Equal keys keep insertion order.

`LIMIT` takes one nonnegative integer literal. `LIMIT 0` returns no rows. A limit larger than the row count returns every remaining row. `OFFSET`, `FETCH`, `LIMIT` expressions, and `ORDER BY` expressions (`ORDER BY 1`, `ORDER BY age + 1`) are rejected.

### Indexes

```sql
CREATE INDEX idx_users_age ON users(age);
DROP INDEX idx_users_age;
```

An index names one `INTEGER` column. Duplicate keys are stored. `NULL` is left out of the tree. The index name is separate from table names, so an index may share a name with a table. `CREATE INDEX` reads the rows already in the table and builds the tree in that statement's commit. Later `INSERT`, `UPDATE`, and `DELETE` statements update every index on the table in the same commit as the heap. `DROP TABLE` removes the table's indexes. `UNIQUE`, more than one column, `DESC`, and `IF NOT EXISTS` are rejected.

`SELECT` uses an index for `column = integer`, `column < integer`, `column <= integer`, `column > integer`, `column >= integer`, and the same shapes with the integer on the left, when the query reads one table. An `AND` of those comparisons on one indexed column is one range. The scan bounds are inclusive: `age < 25` is `age <= 24`, and `25 < age` is `age >= 26`. The original `WHERE` still runs on each candidate, so an extra predicate such as `name = 'Ada'` is applied after the index lookup. `EXPLAIN SELECT` prints the path the query will use. For one table the columns are `access`, `index`, and `bounds`. It reads an index's minimum and maximum key when a range needs that fact, and it does not read the matching rows or estimate a cost. A nested-loop join's `EXPLAIN` names `NestedLoopJoin`, the outer table, the inner table, the `ON` condition, and the `WHERE` filter when there is one. An index nested-loop join's `EXPLAIN` names `IndexNestedLoopJoin`, both tables, the index, the equality used for the probe, the original `ON` when it is wider than that equality, and the `WHERE` filter. A hash join's `EXPLAIN` names `HashJoin`, both tables, the build side, the equality, the original `ON` when it is wider than that equality, and the `WHERE` filter. The build side is the smaller table, or the inner table when the sizes are equal. `EXPLAIN` names the algorithm execution will use after the size rule above. It does not read join rows or write the file. An aggregate query's `EXPLAIN` names `Aggregate` when there is no `GROUP BY` and `HashAggregate` when there is, then the grouping columns, the aggregate functions, the underlying scan, the `WHERE` filter, and `HAVING`. An aggregate that appears only in `HAVING` or `ORDER BY` is listed too. An aggregate over a join names that same aggregate access, then the join algorithm, both tables, the build side or the index, the equality, and the `WHERE` filter. It does not read the joined rows or aggregate them. An alias is shown as `alias (table)` when it differs from the catalog name.

A range that includes every `i64` value, such as `age >= -9223372036854775808`, is a sequential scan. A range that includes every key currently stored in the index is also a sequential scan. `age >= 0` is one of those ranges only when every stored key is at least 0. The `INTEGER` type does not imply that. Equality stays an index lookup. A one-sided range that still excludes some stored key, such as `age >= 50` when smaller keys exist, stays an index scan. There is no histogram and no assumption that keys are spread uniformly.

`OR`, `IS NULL`, `IS NOT NULL`, `!=`, and a predicate that contains `OR` anywhere stay a sequential scan. So does a column with no index, a non-integer comparison, and an in-memory database. `UPDATE` and `DELETE` use an index only for one integer equality, as described above. A range in `UPDATE` or `DELETE` still scans. If several indexes could apply to a `SELECT`, an equality is chosen over a range, and otherwise the earliest index in catalog order is used. There is no cost model.

A delete that leaves a leaf legally occupied rewrites that leaf and any parent separator whose stored minimum actually changes. That rewrite leaves the ancestors above that separator on disk. A later structural edit in the same statement can still write those pages. The statement decodes each ancestor and the leftmost child of that ancestor before the leaf is written, so a damaged page on that walk fails the statement with nothing published. A leaf that would fall below its minimum, a borrow, a merge, and a root contraction still use the single-entry delete. Those structural edits drop any cached parent path.

Opening a file checks the whole index: order, siblings, separators, and that every non-NULL heap integer has its leaf entry. A query checks the entries it reads and any right sibling it follows. That sibling must point back, the keys must stay ordered across the boundary, and a chain that ends before the rightmost leaf fails the statement. A missing row id, a duplicate row id, a key that disagrees with the heap, an unsorted visited leaf, or a page that cannot be decoded fails the statement. A `SELECT` or join lookup does not walk the heap looking for a posting that is missing. Open would have rejected a file that was already missing one. A posting removed after open can still hide a row from that lookup. An indexed `UPDATE` or `DELETE` does walk the heap for the chosen key and fails when the index set differs, so the incomplete change is not committed. A wrong-typed `WHERE` cell makes that execution scan, so the comparison error matches the heap path. `EXPLAIN` does not show that fallback. An in-memory database has no file to store the tree, so `CREATE INDEX` and `DROP INDEX` fail there. A version 2 file must be migrated before `CREATE INDEX`.

## Example

This is a real session from `cargo run`:

```text
rustdb> CREATE TABLE users (id INT, name TEXT, age INT);
OK
rustdb> INSERT INTO users VALUES (1, 'Alice', 24);
1 row inserted
rustdb> INSERT INTO users VALUES (2, 'Bob', NULL);
1 row inserted
rustdb> INSERT INTO users VALUES (3, NULL, 30);
1 row inserted
rustdb> SELECT name, age FROM users WHERE age IS NULL OR name IS NULL;
+------+------+
| name | age  |
+------+------+
| Bob  | NULL |
| NULL | 30   |
+------+------+
2 rows returned
rustdb> UPDATE users SET age = 31 WHERE id = 1;
1 row updated
rustdb> UPDATE users SET name = 'Unknown' WHERE name IS NULL;
1 row updated
rustdb> DELETE FROM users WHERE age < 18;
0 rows deleted
rustdb> SELECT name, age FROM users ORDER BY age DESC, name ASC;
+---------+------+
| name    | age  |
+---------+------+
| Bob     | NULL |
| Alice   | 31   |
| Unknown | 30   |
+---------+------+
3 rows returned
rustdb> SELECT name FROM users ORDER BY age ASC LIMIT 1;
+---------+
| name    |
+---------+
| Unknown |
+---------+
1 row returned
rustdb> exit
```

`ORDER BY age DESC` places Bob first because a null age sorts before every number in descending order. `ORDER BY age ASC LIMIT 1` returns Unknown, whose age is 30, because ascending order places nulls last.

The same statements persist across processes. This session used `cargo run -- --database /tmp/rustdb-m3-demo/rustdb.db`, exited, and started again with the same path:

```text
rustdb> CREATE TABLE users (id INT, name TEXT, age INT);
OK
rustdb> INSERT INTO users VALUES (1, 'Alice', 24), (2, 'Bob', NULL);
2 rows inserted
rustdb> UPDATE users SET age = 31 WHERE id = 1;
1 row updated
rustdb> exit
```

```text
rustdb> SELECT name, age FROM users ORDER BY age ASC;
+-------+------+
| name  | age  |
+-------+------+
| Alice | 31   |
| Bob   | NULL |
+-------+------+
2 rows returned
rustdb> DELETE FROM users WHERE name = 'Alice';
1 row deleted
rustdb> SELECT name, age FROM users;
+------+------+
| name | age  |
+------+------+
| Bob  | NULL |
+------+------+
1 row returned
rustdb> exit
```

## Current limitations

- File mode checkpoints the pages a statement changes. It is not a power-loss proof. Fault-injection tests stop the process between the log sync, the page writes, the database sync, and clearing the log. They do not show what a filesystem does when power is lost during `sync_all`. Network filesystems are not supported. A checksum or audit failure is reported and not repaired.
- A row cannot exceed 4068 bytes. Rows do not overflow onto a second page.
- One writer. A second process cannot open a database this process already has open. There are no concurrent readers and no multi-writer mode.
- No `RIGHT JOIN`, `FULL JOIN`, join of more than two tables, subqueries, `LIKE`, `IN`, `BETWEEN`, or arithmetic. `SELECT DISTINCT` on a `LEFT JOIN` is rejected together with `GROUP BY`, `HAVING`, or an aggregate.
- No primary keys, unique constraints, defaults, or column constraints. `NOT NULL` is rejected with the other constraints, so the engine does not yet enforce it.
- No `UPDATE` or `DELETE` with `ORDER BY`, `LIMIT`, `RETURNING`, or a second table.
- No `OFFSET`, `FETCH`, `LIMIT ALL`, or `ORDER BY` expressions.
- No transactions beyond a single statement. A multi-statement string is not atomic.
- `SELECT` from one table can use one single-column integer index for an equality or a range that does not cover every stored key. A range over every `i64`, or over every key in the index, scans the table. `OR`, `IS NULL`, `IS NOT NULL`, `!=`, and any predicate that contains `OR` scan the table. `UPDATE` and `DELETE` use one integer equality index when it exists, list those row ids, and then evaluate the original `WHERE`. A range, `OR`, `IS NULL`, and a non-integer equality still scan. An `UPDATE` prepares and publishes only the matched rows. A stored value that already matches is not rewritten, and a larger record uses the existing relocate path inside that commit. A `DELETE` that removes rows unlinks the listed row ids and does not compare the survivors. A delete that matches nothing still decodes the stored rows. The WAL still records only the dirty pages, so an `UPDATE` that changes the same rows writes the same log as the old full compare. A selective `UPDATE` keeps a journal of the matched rows' previous images and restores those images if publish fails before the log sync. A `DELETE` keeps each removed row's image and original position and splices that row back, with the same row id, if publish fails before the log sync. The row-id counter does not change. `INSERT` still snapshots the table. An `UPDATE` or `DELETE` that matches every row journals every row. Publish of a `DELETE` passes the removed row ids to the editor and does not clone the surviving values. The editor checks that list. One deleted row keeps the per-row unlink. Two or more repair heap links in descending stored order and release each emptied heap page once. Index postings that share a leaf are removed in one rewrite when the leaf stays at its minimum. A run that would drop a leaf below its minimum is repacked across the sibling span that holds the verified prefix, and the internal pages over the surviving leaves are rebuilt in that same commit. A run that is strictly increasing or strictly decreasing in `(key, row id)` order is checked along the leaf chain after one root descent. Publish passes postings in descending stored order. A run that changes direction is planned before the per-leaf loop. The plan reads the root and, at height three, each of the root's children, and estimates how many pages that tree holds. It checks the run against a temporary map of every validated `(key, row id)` pair when one descent per remaining posting would read more pages than the largest tree of this height. An exact smaller tree is planned when the request is longer than one full walk per level, `postings > pages * height`. A shorter request stays on the descent path. A height of four or more fills the unseen lower levels to this tree's fanout, which is a ceiling. The old full-fanout ceiling remains the fallback when that probe cannot read a child. That map is filled from the same structural walk that accepts the tree: the walk records each leaf's parent path and entry count, then the plan reserves the hash table and inserts those pairs. The map is built before any grouped rewrite, and it is dropped when a rebuild, a merge, or a root change moves pages. A grouped rewrite that leaves every other leaf in place keeps the map. A single posting that does not underflow is removed through the path already in hand and also keeps the map. The live leaf is decoded again, and the posting has to still sit there. The map alone does not authorize a delete. The rebuild checks this tree before it writes. When the first action is the rebuild and nothing has been written since the plan's check, that pre-write check is skipped. The rebuilt tree is validated after the write. A repeated posting, a request below that bound, a map that cannot be reserved, and a test that forces the older check still descend once per posting. Reserving that map does not change a page. An allocator abort while the validated posting list is built is process-level and is not returned as a query error. A borrow, a merge, and a root contraction still use the single-entry delete, which descends from the root. A test can force the per-row loop, the grouped leaf rewrite, the per-posting descent, or that earlier single-entry descent. The repacked tree can use different pages and a different log size than deleting one posting at a time. The page format and the WAL record are unchanged. Those ids are a copy, so the undo journal still holds the images and positions. A delete that matches nothing still audits every in-memory row and decodes the stored rows, and it writes no log when they already match. The equality check reads every heap row before the mutation, so a posting removed after open fails the statement instead of committing a partial change. A malformed `WHERE` cell uses the heap scan for that execution. An `INNER JOIN` uses an in-memory hash table for a same-type equality. It probes an inner integer index instead when the outer table is empty or the inner table has more than 64 times as many rows. That factor sits between a measured 20× case, where the hash scan was faster, and measured 100× and 400× cases, where the index probe was faster. `OR`, a non-equality, and `IS NULL` alone read both tables. There is no cost-based optimizer, histogram, composite index, or merge join. The hash table is not stored in the file. A range such as `age >= 50` can still be slower than a table scan when it matches a large share of the rows. A `LEFT JOIN` or `LEFT OUTER JOIN` of two tables hashes the right table when `ON` contains a same-type `INTEGER`, `TEXT`, or `BOOLEAN` column equality, including a reversed equality and an equality inside `AND`. The hash stores positions of non-NULL right keys. Exact key equality picks the bucket group, then the original `ON` runs, so a residual predicate can reject a candidate. NULL keys are not matches. A left row with no TRUE `ON` result is returned once, with NULL for every right column, and `WHERE` runs after that. `WHERE` is not moved into `ON`. The build side is always the right table, so left order does not need a pair buffer. `OR`, a range, `IS NULL`, and any other predicate stay `LeftNestedLoopJoin`. An integer equality on a right-table index is `IndexLeftNestedLoopJoin` when the left table is empty or the right table has more than 64 times as many rows. That 64 is not the inner join's factor. Smaller right tables stay `HashLeftJoin`. This is a size rule, not a cost model. A corrupt page, a broken leaf link, a missing row id, a repeated row id, or a key that disagrees with the heap fails the statement. The probe does not notice a posting removed after open; open itself rejects a file that is missing one. A malformed `ON` cell uses the nested loop for that execution only, and `EXPLAIN` does not show that fallback. A `TEXT` or `BOOLEAN` equality stays `HashLeftJoin` even when an integer index exists on another column. `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX` accumulate each surviving logical row, including one null-extended row for an unmatched left row that `WHERE` keeps. `COUNT(*)` counts that row. A right-column aggregate ignores its NULLs. `COUNT(DISTINCT column)`, `SUM(DISTINCT integer)`, and `AVG(DISTINCT integer)` use the same per-group sets as a single table: NULL arguments, including every column of a null-extended right side, are not stored. Equal values in one group count once, and another group keeps its own set. `GROUP BY` and `HAVING` use the same binding as an inner join. `SELECT DISTINCT` keeps the first projected logical row, including one null-extended row, and compares the whole tuple. A plain `LEFT JOIN` can still sort by a column that is not selected. `MIN(DISTINCT ...)` and `MAX(DISTINCT ...)` stay rejected.
- Integer type names are aliases of `i64`. `SMALLINT` does not reject values outside the 16-bit range.
- `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX` read one table, one `INNER JOIN`, or one `LEFT JOIN`. `AVG` of an integer column ignores `NULL` and returns an exact reduced ratio. A whole average prints with `.0` (`4.0`). A terminating fraction prints as a decimal (`0.5`). A repeating fraction prints reduced (`1/3`). The ratio is a query result: it is not a column type, and the file format is unchanged. A terminating decimal has at most 63 digits. Comparison uses the `i128` cross product when it fits, and an exact 256-bit product otherwise. It does not use floating point. `MIN` and `MAX` ignore `NULL` and use the same order as `ORDER BY`, including `false` then `true`. Boolean `<` and `>` stay rejected. `HAVING` runs after grouping and keeps a group only when it is TRUE. The grouping hash table is not stored. `COUNT(DISTINCT column)`, `SUM(DISTINCT integer)`, and `AVG(DISTINCT integer)` each keep one in-memory set per group. Empty and all-NULL inputs make `COUNT` 0 and `SUM` and `AVG` NULL. `MIN(DISTINCT ...)` and `MAX(DISTINCT ...)` are rejected. A new distinct value is recorded only after every aggregate of that input row succeeds. The sets are not spilled. A select-list alias (`AS` or a bare alias) is the output name. An unqualified `ORDER BY` or `HAVING` name uses a unique explicit alias before an implicit output name, and a qualified name ignores aliases. `WHERE`, `ON`, and `GROUP BY` do not see select aliases. `EXPLAIN` appends `slots` and `aliases` on an aggregate plan. A hash join that builds the outer table buffers one pair of positions per kept match so group order stays outer-then-inner. That buffer grows with the match count and is not spilled. The joined rows themselves are not stored. An aggregate `LEFT JOIN` reports `HashLeftJoin`, `LeftNestedLoopJoin`, or `IndexLeftNestedLoopJoin`, both tables, the right-table build side, the equality and residual `ON`, `WHERE`, the grouping columns, the aggregate expressions, `HAVING`, the projection, `ORDER BY`, and `LIMIT`. An index plan appends the right index name. That plan does not read rows.
- `SELECT DISTINCT` keeps the first copy of each projected row from one non-aggregate table, one non-aggregate `INNER JOIN`, or one non-aggregate `LEFT JOIN`. It runs after `WHERE` and before `ORDER BY` and `LIMIT`. Comparison is the whole output tuple: NULL matches NULL, text keeps its case, and a hash collision still compares the cells. On a join, each projected pair is admitted as the join visits it. The joined rows are not stored beside that set. A left join admits a null-extended row the same way. `ORDER BY` on `DISTINCT` must name a selected output column or alias. A unique explicit alias wins, then a unique implicit output name, then one projected source column. A qualified name ignores aliases. A column left out of the projection is rejected. A plain join may still sort by a column that is not selected. `LIMIT` does not stop the scan. One stored tuple is kept per unique output row, and that set is not spilled. A `HashMap` that cannot grow aborts the process. `DISTINCT` with `GROUP BY`, with `HAVING`, or with an aggregate is rejected. `SELECT ALL` and `DISTINCT ON` are rejected. `EXPLAIN` for one table appends `distinct`, `projection`, `filter`, `order_by`, and `limit` after the scan columns. `EXPLAIN` for a distinct inner join reports the join access, both inputs, the hash build side or index name, the equality and residual predicates, the filter, `HashDistinct`, the projection, `ORDER BY`, and `LIMIT`. `EXPLAIN` for a distinct left join reports `HashLeftJoin`, `LeftNestedLoopJoin`, or `IndexLeftNestedLoopJoin`, both tables, the right-table build side, the equality and residual `ON`, `WHERE`, `HashDistinct`, the projection, `ORDER BY`, and `LIMIT`. An index plan appends the right index name. That plan does not read rows or probe the index. A malformed stored `ON` cell can still make execution use the nested loop.
- Floating-point literals and function calls other than `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX` are rejected.
- The REPL is single-threaded. Prompts are always printed, including when input is piped.
- Result cells are printed raw, so a newline inside a text value breaks the ASCII table.

## Next milestone

Milestone 30 should reduce the per-row cost of unlinking a deleted record and dropping its index postings. At 100,000 rows, deleting every row still spends about 1.3 seconds in that work on both the selective path and the full-image path, and the log is the same 12,275,496 bytes. A no-match `DELETE` still decodes every stored row. `INSERT` still snapshots the table. A range, `OR`, or `IS NULL` still scans. A third table, `RIGHT JOIN`, `FULL JOIN`, window functions, arithmetic, and subqueries stay out of scope.

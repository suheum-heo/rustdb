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
| `storage` | Pager, slotted pages, redo log, and incremental page edits |
| `ast` | Statements and expressions the executor understands |
| `parser` | `sqlparser` boundary and identifier normalization |
| `expr` | Bind columns, check types, and evaluate three-valued logic |
| `executor` | `CREATE`, `DROP`, `INSERT`, `SELECT`, `UPDATE`, and `DELETE` |
| `format` | ASCII result tables |
| `repl` | Interactive session |
| `error` | Parse, schema, type, and storage errors |

While a statement runs, rows live in a `Vec`. That keeps `INSERT` validation and tuple projection the same as the in-memory engine. After a successful mutating statement, file mode publishes those rows into 4096-byte slotted pages. The byte layout is documented in [docs/storage-format.md](docs/storage-format.md). It uses explicit little-endian integers and UTF-8, not Rust's in-memory representation.

`NULL` is its own value, not a sentinel integer or an empty string. A column reference keeps the column's declared type even when the stored cell is null. A `NULL` literal has no type and is compatible with every column.

Unquoted identifiers are folded to lowercase. Quoted identifiers keep their case and match exactly, so `Users` and `"Users"` are different tables.

`INSERT` checks every row before it appends any of them. `UPDATE` and `DELETE` build the replacement rows and install them only after every row succeeds. A type error or a missing column leaves the table unchanged. In file mode a failure before the commit record is synced restores memory when the log is still incomplete. A failed sync of that record does not prove the record is absent: memory stays, the log stays, and the process refuses further statements until the file is reopened. A failure after a successful log sync does the same. Separate statements in one string are applied one at a time: if the second statement fails, the first one stays, including on disk.

`SELECT`, `UPDATE`, and `DELETE` share one filter. The `WHERE` expression is bound to column indexes and type-checked before any row is read. An empty table still reports a missing column or a comparison between different types. `AND` and `OR` are type-checked on both sides; a false or unknown left side does not hide a type error on the right.

A query keeps a row only when `WHERE` evaluates to TRUE. FALSE and UNKNOWN both drop the row.

`SELECT` then sorts, then applies `LIMIT`, then projects. `ORDER BY` can name a column that is absent from the select list. The sort is stable, so equal keys stay in insertion order. `SELECT` does not rewrite the file.

## Durability

Each successful `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, or `DELETE` writes the pages that statement changed into `PATH.wal`, syncs that same file, writes those pages into the database, syncs the database, and only then truncates the log. A successful sync of the log is the commit point. A failed sync is not treated as a rollback. `SELECT` does not write the log. Unchanged pages stay as they are, including other tables' heap pages.

The process holds an advisory `flock` on `PATH.lock` and on the database file for as long as it has the database open. The lock file stays after unlock. A second open of the same path fails, including a second open in this process. Renaming the database file does not release the inode lock held by the process that already has it open. `flock` is advisory: a process that does not take it is not stopped. A forked child inherits the lock until it closes that descriptor or starts a new program. The descriptor is close-on-exec.

A crash before the commit record is durable leaves the previous database, and an incomplete log that belongs to the file is discarded on open. A crash after the commit record is durable and before the log is cleared is repaired on the next open by writing the logged pages again. That replay is safe to repeat. A version 1 file is not migrated on open. `rustdb --migrate PATH` rewrites it as version 2 and keeps row ids and table ids.

This is not a power-loss proof. The tests stop the commit at chosen steps, including a sync that returns an error after the commit record has been written. They do not cut power. RustDB assumes a local filesystem that honors `File::sync_all` and `flock`. The byte layout, the log, and the remaining filesystem assumptions are in [docs/storage-format.md](docs/storage-format.md).

A row must fit in one heap cell, at most 4068 bytes. Larger rows are rejected in both modes. There are no overflow pages. A table can still occupy many pages: each row is one cell, and cells are linked in insertion order.

Deleted rows leave free pages that a later statement can reuse, so deleting a large table and inserting a similar one does not grow the file without bound. The file is not shrunk. Inside one page, a deleted slot can be reused. An update that fits in the existing cell keeps that `(page, slot)`.

Every row has a `u64` id assigned at insert. `UPDATE` keeps it. `DELETE` does not reuse it. Row ids are not returned by SQL. Table ids for a file written by this version come from a counter stored at the end of page 0. `DROP TABLE` does not return an id, including after that file is reopened. A file written before the counter existed has no record of deleted ids, so the first new table after opening it can reuse one. Table ids are not returned by SQL either.

## Build and run

Requires a stable Rust toolchain (this project was built with Rust 1.100).

```sh
cargo run
cargo run -- --database ./rustdb.db
cargo run -- --migrate ./rustdb-v1.db
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

With no arguments the REPL uses an empty in-memory database, and exiting drops every table. `--database PATH` creates a version 2 file when it does not exist and reloads it on the next run. `--migrate PATH` rewrites a version 1 file as version 2 and then exits. It does not start the REPL. A version 2 file is left unchanged and the command fails. The REPL prompts with `rustdb> `. End each statement with `;`. A statement can span lines. `exit` and `quit` close the session. SQL errors are printed and the session continues.

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
SELECT * FROM table_name [WHERE condition]
    [ORDER BY column_name [ASC | DESC] [NULLS FIRST | NULLS LAST]
        [, column_name [ASC | DESC] [NULLS FIRST | NULLS LAST], ...]]
    [LIMIT count];
SELECT column_name [, column_name, ...] FROM table_name [WHERE condition]
    [ORDER BY ...] [LIMIT count];
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

### ORDER BY and LIMIT

```sql
SELECT * FROM users ORDER BY age ASC;
SELECT name FROM users ORDER BY age DESC LIMIT 5;
SELECT * FROM users ORDER BY age DESC, name ASC;
SELECT * FROM users ORDER BY age ASC NULLS FIRST;
```

`ASC` is the default. NULL is greater than every non-NULL value, which is PostgreSQL's default: `ASC` sorts nulls last and `DESC` sorts nulls first. `NULLS FIRST` and `NULLS LAST` are absolute; `DESC` does not flip an explicit null order. Boolean order is `false` then `true`. Equal keys keep insertion order.

`LIMIT` takes one nonnegative integer literal. `LIMIT 0` returns no rows. A limit larger than the row count returns every remaining row. `OFFSET`, `FETCH`, `LIMIT` expressions, and `ORDER BY` expressions (`ORDER BY 1`, `ORDER BY age + 1`) are rejected.

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
- No `JOIN`, `GROUP BY`, `DISTINCT`, aggregates, subqueries, `LIKE`, `IN`, `BETWEEN`, or arithmetic.
- No primary keys, unique constraints, defaults, or column constraints. `NOT NULL` is rejected with the other constraints, so the engine does not yet enforce it.
- No `UPDATE` or `DELETE` with `ORDER BY`, `LIMIT`, `RETURNING`, or a second table.
- No `OFFSET`, `FETCH`, `LIMIT ALL`, or `ORDER BY` expressions.
- No transactions beyond a single statement. A multi-statement string is not atomic.
- No indexes. Filtering scans the table from first inserted row to last.
- Integer type names are aliases of `i64`. `SMALLINT` does not reject values outside the 16-bit range.
- Floating-point literals and function calls are rejected.
- The REPL is single-threaded. Prompts are always printed, including when input is piped.
- Result cells are printed raw, so a newline inside a text value breaks the ASCII table.

## Next milestone

Milestone 4 should add a B+ tree for equality and range lookup on one column. The page format is now concrete enough to hold internal index pages. A write-ahead log can wait until rewriting the whole file is the measured cost; it should not arrive disguised as a transaction system.

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
  -> page image, written after a successful mutation (file mode)
  -> rendered result
```

| Module | Responsibility |
| --- | --- |
| `value` | `INTEGER`, `TEXT`, `BOOLEAN`, and `NULL` |
| `schema` | Column names, types, and insert column mapping |
| `table` | One table: schema, insertion-ordered rows, and stable row ids |
| `database` | Catalog of named tables, in memory or backed by a file |
| `storage` | Pager, slotted pages, record codec, and catalog image |
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

`INSERT` checks every row before it appends any of them. `UPDATE` and `DELETE` build the replacement rows and install them only after every row succeeds. A type error or a missing column leaves the table unchanged. In file mode the previous file is also left unchanged, because the new image is renamed into place only after the statement succeeds. Separate statements in one string are applied one at a time: if the second statement fails, the first one stays, including on disk.

`SELECT`, `UPDATE`, and `DELETE` share one filter. The `WHERE` expression is bound to column indexes and type-checked before any row is read. An empty table still reports a missing column or a comparison between different types. `AND` and `OR` are type-checked on both sides; a false or unknown left side does not hide a type error on the right.

A query keeps a row only when `WHERE` evaluates to TRUE. FALSE and UNKNOWN both drop the row.

`SELECT` then sorts, then applies `LIMIT`, then projects. `ORDER BY` can name a column that is absent from the select list. The sort is stable, so equal keys stay in insertion order. `SELECT` does not rewrite the file.

## Durability

Each successful `CREATE TABLE`, `DROP TABLE`, `INSERT`, `UPDATE`, or `DELETE` writes a complete page image to a uniquely named temporary file in the same directory, syncs that file, syncs the directory, and renames the temporary file over the database. The directory is synced again after the rename on systems that can sync a directory. A crash before the rename leaves the previous file, and a leftover temporary file is ignored. A failure before the rename restores the in-memory tables. RustDB does not keep a write-ahead log and does not claim that a power loss during the rename is atomic. A checksum failure on a later open is reported and not repaired.

A row must fit in one heap cell, at most 4068 bytes. Larger rows are rejected in both modes. There are no overflow pages. A table can still occupy many pages: each row is one cell, and cells are linked in insertion order.

Deleted rows are omitted from the next image. Their pages become free ids and can be reused, so deleting a large table and inserting a similar one does not grow the file without bound. Inside one page, a deleted slot is reusable after compaction. A published page is packed, with no holes.

Every row has a `u64` id assigned at insert. `UPDATE` keeps it. `DELETE` does not reuse it. The on-disk `(page, slot)` can change on the next publish; the row id does not. Row ids are not returned by SQL.

## Build and run

Requires a stable Rust toolchain (this project was built with Rust 1.100).

```sh
cargo run
cargo run -- --database ./rustdb.db
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

With no arguments the REPL uses an empty in-memory database, and exiting drops every table. `--database PATH` creates the file when it does not exist and reloads it on the next run. The REPL prompts with `rustdb> `. End each statement with `;`. A statement can span lines. `exit` and `quit` close the session. SQL errors are printed and the session continues.

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

- File mode replaces the whole page image after each mutating statement. There is no write-ahead log, buffer-pool eviction, or crash recovery. A power loss during the rename is not claimed to be atomic. A leftover temporary file from a crash before the rename is not recovered.
- A row cannot exceed 4068 bytes. Rows do not overflow onto a second page.
- One writer. The file is not locked, and two processes must not publish it at the same time.
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

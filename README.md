# RustDB

RustDB is a small relational SQL engine written in Rust. Milestone 1 is an in-memory database: it parses a limited SQL subset, checks types against a table schema, and runs queries from a REPL. It does not use SQLite, PostgreSQL, or any other database engine.

Nothing is stored on disk. Restarting the process drops every table.

## Architecture

SQL text enters through the REPL or `Database::execute`. `sqlparser` (the `sqlparser` crate, version 0.64) turns that text into its own syntax tree. The parser module immediately lowers the supported subset into a small internal statement tree and rejects everything else. The executor only sees that internal tree, so an unsupported clause cannot be skipped by accident.

```text
SQL text
  -> sqlparser
  -> internal statement tree (parser)
  -> executor
  -> catalog of in-memory tables
  -> rendered result
```

| Module | Responsibility |
| --- | --- |
| `value` | `INTEGER`, `TEXT`, and `BOOLEAN` scalars |
| `schema` | Column names, types, and insert column mapping |
| `table` | One table: schema plus rows in insertion order |
| `database` | Catalog of named tables |
| `ast` | Statements and expressions the executor understands |
| `parser` | `sqlparser` boundary and identifier normalization |
| `executor` | `CREATE`, `DROP`, `INSERT`, and `SELECT` |
| `format` | ASCII result tables |
| `repl` | Interactive session |
| `error` | Parse, schema, and type errors |

Rows are stored row by row (`Vec<Vec<Value>>`). That makes insert validation and tuple projection direct. A columnar layout can wait until a measured scan is worth optimizing.

Unquoted identifiers are folded to lowercase. Quoted identifiers keep their case and match exactly, so `Users` and `"Users"` are different tables.

`INSERT` checks every row before it appends any of them. A bad value in a multi-row insert leaves the table unchanged. Separate statements in one string are applied one at a time: if the second statement fails, the first one stays.

`WHERE` expressions are bound to column indexes and type-checked before any row is read. An empty table still reports a missing column or a comparison between different types. `AND` and `OR` are type-checked on both sides; a false left side does not hide a type error on the right.

## Build and run

Requires a stable Rust toolchain (this project was built with Rust 1.100).

```sh
cargo run
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

The REPL prompts with `rustdb> `. End each statement with `;`. A statement can span lines. `exit` and `quit` close the session. SQL errors are printed and the session continues.

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
SELECT * FROM table_name [WHERE condition];
SELECT column_name [, column_name, ...] FROM table_name [WHERE condition];
```

Types:

| SQL names | Stored as |
| --- | --- |
| `INT`, `INTEGER`, `BIGINT`, `SMALLINT`, `TINYINT`, `MEDIUMINT` | 64-bit signed integer |
| `TEXT`, `VARCHAR`, `CHAR`, `STRING` | Unicode text |
| `BOOLEAN`, `BOOL` | Boolean |

`INT(11)` and `VARCHAR(n)` are accepted. Display widths and character limits are not enforced.

`WHERE` supports `=`, `!=`, `<>`, `<`, `>`, `<=`, `>=`, `AND`, `OR`, and parentheses. Comparisons require the same type on both sides. Ordering is defined for integers and text (UTF-8 byte order). Booleans support `=` and `!=` only. A named `INSERT` column list must mention every column once; there are no defaults and no `NULL`.

## Example

This is a real session from `cargo run`:

```text
rustdb> CREATE TABLE users (id INT, name TEXT, age INT);
OK
rustdb> INSERT INTO users VALUES (1, 'Alice', 24);
1 row inserted
rustdb> INSERT INTO users VALUES (2, 'Bob', 30);
1 row inserted
rustdb> SELECT name FROM users WHERE age > 25;
+------+
| name |
+------+
| Bob  |
+------+
1 row returned
rustdb> SELECT * FROM missing;
Error: table 'missing' does not exist
rustdb> exit
```

More queries the engine runs:

```sql
INSERT INTO users (name, age, id) VALUES ('Cara', 30, 3);
SELECT id, name FROM users WHERE age >= 30 OR name = 'Alice';
SELECT * FROM users WHERE age > 24 AND name != 'Alice';
DROP TABLE users;
```

## Current limitations

- Memory only. There is no file format, buffer pool, or crash recovery.
- No `NULL`, `UPDATE`, `DELETE`, `JOIN`, `ORDER BY`, `LIMIT`, `GROUP BY`, `DISTINCT`, aggregates, or subqueries.
- No primary keys, unique constraints, defaults, or column constraints. Those clauses are rejected rather than ignored.
- No transactions beyond a single statement. A multi-statement string is not atomic.
- No indexes. `SELECT` scans the table from first inserted row to last.
- Integer type names are aliases of `i64`. `SMALLINT` does not reject values outside the 16-bit range.
- `NOT`, `LIKE`, `IN`, `BETWEEN`, and function calls are rejected.
- The REPL is single-threaded. Prompts are always printed, including when input is piped.
- Result cells are printed raw, so a newline inside a text value breaks the ASCII table.

## Next milestone

Milestone 2 should stay in memory and finish the row model before any page format exists:

- `NULL`
- `UPDATE` and `DELETE`
- `ORDER BY` and `LIMIT`

`NULL` changes how every value is represented. Persistence and a B+ tree should follow that, once a tuple layout is worth writing to disk.

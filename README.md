# RustDB

RustDB is a small relational SQL engine written in Rust. It parses a limited SQL subset, checks types against a table schema, and runs queries from a REPL. It does not use SQLite, PostgreSQL, or any other database engine.

Nothing is stored on disk. Restarting the process drops every table.

## Architecture

SQL text enters through the REPL or `Database::execute`. `sqlparser` (the `sqlparser` crate, version 0.64) turns that text into its own syntax tree. The parser module lowers the supported subset into a small internal statement tree and rejects everything else. The executor only sees that internal tree, so an unsupported clause cannot be skipped by accident.

```text
SQL text
  -> sqlparser
  -> internal statement tree (parser)
  -> expression binder (expr)
  -> executor
  -> catalog of in-memory tables
  -> rendered result
```

| Module | Responsibility |
| --- | --- |
| `value` | `INTEGER`, `TEXT`, `BOOLEAN`, and `NULL` |
| `schema` | Column names, types, and insert column mapping |
| `table` | One table: schema plus rows in insertion order |
| `database` | Catalog of named tables |
| `ast` | Statements and expressions the executor understands |
| `parser` | `sqlparser` boundary and identifier normalization |
| `expr` | Bind columns, check types, and evaluate three-valued logic |
| `executor` | `CREATE`, `DROP`, `INSERT`, `SELECT`, `UPDATE`, and `DELETE` |
| `format` | ASCII result tables |
| `repl` | Interactive session |
| `error` | Parse, schema, and type errors |

Rows are stored row by row (`Vec<Vec<Value>>`). `NULL` is its own value, not a sentinel integer or an empty string. A column reference keeps the column's declared type even when the stored cell is null. A `NULL` literal has no type and is compatible with every column.

Unquoted identifiers are folded to lowercase. Quoted identifiers keep their case and match exactly, so `Users` and `"Users"` are different tables.

`INSERT` checks every row before it appends any of them. `UPDATE` and `DELETE` build a complete replacement row list and install it only after every row succeeds. A type error or a missing column leaves the table unchanged. Separate statements in one string are applied one at a time: if the second statement fails, the first one stays.

`SELECT`, `UPDATE`, and `DELETE` share one filter. The `WHERE` expression is bound to column indexes and type-checked before any row is read. An empty table still reports a missing column or a comparison between different types. `AND` and `OR` are type-checked on both sides; a false or unknown left side does not hide a type error on the right.

A query keeps a row only when `WHERE` evaluates to TRUE. FALSE and UNKNOWN both drop the row.

`SELECT` then sorts, then applies `LIMIT`, then projects. `ORDER BY` can name a column that is absent from the select list. The sort is stable, so equal keys stay in insertion order.

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

## Current limitations

- Memory only. There is no file format, buffer pool, or crash recovery.
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

Milestone 3 should store the current row format on disk. The in-memory value model, including `NULL`, is now stable enough to give a page layout something concrete to persist. Indexes, a write-ahead log, and concurrency should wait until a page can be written and read back.

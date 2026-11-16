//! RustDB is a small relational SQL engine.
//!
//! It parses a limited SQL subset, stores typed tables, and runs them from a
//! REPL. [`Database::new`] is in memory. [`Database::open`] reads and writes a
//! page file and holds an exclusive lock on it. It does not lock rows.
//! `CREATE INDEX` stores an integer B+ tree. `SELECT` uses that tree for an
//! equality or a bounded range on one indexed integer column, then checks the
//! original `WHERE`. An `INNER JOIN` probes an integer index on the inner
//! table when `ON` has a matching equality and the inner table is empty-probed
//! or more than 64 times the outer table. Every other equality of two scalar
//! columns builds an in-memory hash table on the smaller input. A stored `ON`
//! cell that is not NULL and not its declared type uses the nested loop for
//! that execution. `EXPLAIN` still names the plan chosen from the schema.
//! Other joins walk both tables in insertion order. A `SELECT` can `GROUP BY`
//! columns of one table, one `INNER JOIN`, or one `LEFT JOIN` and compute
//! `COUNT`, `SUM`, `AVG`, `MIN`, and `MAX`. `AVG` is an exact ratio and is not stored.
//! `HAVING` keeps a group only when it is TRUE. A select-list alias is an
//! output name, an `ORDER BY` name, and an unqualified `HAVING` name.
//! `COUNT`, `SUM`, and `AVG` accept `DISTINCT` on one column. `SELECT DISTINCT`
//! keeps the first copy of each projected row from one non-aggregate table or
//! from one non-aggregate `INNER JOIN`. A `LEFT JOIN` of two tables keeps
//! every left row. A same-type column equality hashes the right table. Any
//! other `ON` uses a nested loop. A left row with no TRUE `ON` match is
//! returned once, with NULL for each right column. `WHERE` runs after that
//! extension. `COUNT(*)` counts a null-extended row. `COUNT(DISTINCT ...)`,
//! `SUM(DISTINCT ...)`, and `AVG(DISTINCT ...)` use the same per-group sets
//! as a single table and ignore NULL, including a null-extended right column.
//! `SELECT DISTINCT` stays off that join.
//! Other filters scan the in-memory table.
//!
//! # Example
//!
//! ```
//! use rustdb::Database;
//!
//! let mut db = Database::new();
//! db.execute("CREATE TABLE users (id INT, name TEXT);").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'Ada');").unwrap();
//! let results = db.execute("SELECT name FROM users WHERE id = 1;").unwrap();
//! assert_eq!(results.len(), 1);
//! ```

#![forbid(unsafe_code)]

mod ast;
mod database;
mod error;
mod executor;
mod expr;
mod format;
mod parser;
mod plan;
mod repl;
mod schema;
mod storage;
mod table;
#[cfg(test)]
mod test_dir;
mod value;

#[cfg(feature = "fuzz")]
pub mod fuzzing;
#[cfg(not(feature = "fuzz"))]
pub(crate) mod fuzzing;

pub use database::Database;
pub use error::DbError;
pub use executor::{QueryResult, StatementResult};
pub use format::render_result;
pub use repl::run;
pub use storage::{MAX_RECORD_LEN, PAGE_SIZE};
pub use value::{DataType, Value};

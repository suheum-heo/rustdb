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
//! keeps the first copy of each projected row from one non-aggregate table,
//! one non-aggregate `INNER JOIN`, or one non-aggregate `LEFT JOIN`. A `LEFT JOIN` of two tables keeps
//! every left row. An indexed integer equality probes the right index when the
//! left table is empty or the right table has more than 64 times as many rows.
//! That factor is separate from the inner join's rule. Any other same-type
//! column equality hashes the right table. Any other `ON` uses a nested loop.
//! A left row with no TRUE `ON` match is
//! returned once, with NULL for each right column. `WHERE` runs after that
//! extension. `UPDATE` and `DELETE` use one integer equality index to list
//! candidate row ids when `WHERE` has that equality. The heap must contain
//! exactly those equality matches before any row changes. A missing or extra
//! posting fails the statement. The original predicate then runs on that
//! complete set. A range, `OR`, `IS NULL`, and a non-integer equality still
//! scan the heap. The index and the WAL change only inside the existing
//! publish of that statement. An `UPDATE` publishes the matched rows through
//! that commit and does not compare the other heap values. A larger record
//! relocates with the existing page editor. Before that publish, the statement
//! saves the previous images of the matched rows and restores them if the
//! commit fails before the log sync. A `DELETE` saves each removed row's
//! image and original position and puts that row back, with the same id, if
//! the commit fails before the log sync. Publish unlinks those ids and does
//! not clone the surviving values. One deleted row keeps the per-row unlink.
//! Two or more repair heap links in one descending pass. Index postings that
//! share a leaf are removed together when the leaf
//! stays at its minimum. A run that would drop a leaf below its minimum is
//! repacked across that leaf span, and the internal pages above the surviving
//! leaves are rebuilt in the same commit. A run that is strictly increasing or
//! strictly decreasing is checked along the leaf chain after one root descent.
//! Publish hands postings over in descending stored order, which is decreasing
//! for an indexed `id`. A run that changes direction is planned before the
//! per-leaf loop. The plan reads the root shape and builds a temporary map of
//! the validated postings when the remaining descents would read more pages
//! than that estimate. The structural walk records each leaf's parent path,
//! and the map is filled from the pairs that walk already accepted.
//! An exact page count also requires the request to be
//! longer than that count. A repeated posting, a map that is not worth building,
//! a map that cannot be reserved, and a test that forces the older check
//! still descend once per posting. The map is dropped when a rebuild frees
//! pages. An allocator abort is process-level and is not a query error. One posting, and any run that stays
//! at the minimum, still uses the earlier delete. A test can force that
//! grouped path. The rebuilt pages can differ from deleting one posting at a
//! time. A delete that matches nothing still audits
//! memory and decodes the stored rows, and it writes no log when they already
//! match. `INSERT` still snapshots the table.
//! `COUNT(*)` counts a null-extended row. `COUNT(DISTINCT ...)`,
//! `SUM(DISTINCT ...)`, and `AVG(DISTINCT ...)` use the same per-group sets
//! as a single table and ignore NULL, including a null-extended right column.
//! `SELECT DISTINCT` keeps the first copy of each projected logical row,
//! including a null-extended row. NULL matches NULL in that comparison.
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

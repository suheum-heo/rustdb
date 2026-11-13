//! RustDB is a small in-memory relational SQL engine.
//!
//! It parses a limited SQL subset, stores typed tables in memory, and runs
//! them from a REPL. It does not persist data, lock rows, or build indexes.
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
mod repl;
mod schema;
mod table;
mod value;

pub use database::Database;
pub use error::DbError;
pub use executor::{QueryResult, StatementResult};
pub use format::render_result;
pub use repl::run;
pub use value::{DataType, Value};

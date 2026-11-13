//! Errors returned by parsing and execution.

use std::fmt;

use crate::value::DataType;

/// A failure while parsing or executing SQL.
///
/// Variants stay structured so callers can match specific catalog and schema
/// failures. Messages are written for a person sitting at the REPL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbError {
    /// The SQL text could not be parsed.
    Parse(String),
    /// The statement is syntactically valid and intentionally rejected.
    Unsupported(String),
    /// The statement is outside the subset this engine can execute.
    InvalidStatement(String),
    /// A `CREATE TABLE` used a name that is already in the catalog.
    TableAlreadyExists(String),
    /// A statement named a table that is not in the catalog.
    TableNotFound(String),
    /// A statement named a column that is not in the table.
    ColumnNotFound {
        /// Table that was searched.
        table: String,
        /// Column name that was not found.
        column: String,
    },
    /// A table or column list defined the same column twice.
    DuplicateColumn {
        /// Table that owns the column list.
        table: String,
        /// Column that appeared more than once.
        column: String,
    },
    /// An `INSERT` row did not have one value per target column.
    ValueCount {
        /// Table receiving the row.
        table: String,
        /// Number of values required.
        expected: usize,
        /// Number of values provided.
        actual: usize,
    },
    /// A value's type did not match the column type.
    TypeMismatch {
        /// Column that rejected the value.
        column: String,
        /// Type declared for the column.
        expected: DataType,
        /// Type of the value that was supplied.
        actual: DataType,
    },
    /// A named `INSERT` omitted a column and this engine has no defaults.
    MissingValue {
        /// Table receiving the row.
        table: String,
        /// Column that was not given a value.
        column: String,
    },
    /// A comparison or boolean operator was used with the wrong types.
    TypeError(String),
    /// An integer token could not be stored as `i64`.
    InvalidLiteral(String),
    /// A catalog or execution invariant was violated.
    Internal(String),
}

impl DbError {
    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidStatement(message.into())
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(f, "failed to parse SQL: {message}"),
            Self::Unsupported(message)
            | Self::InvalidStatement(message)
            | Self::TypeError(message) => {
                write!(f, "{message}")
            }
            Self::TableAlreadyExists(name) => write!(f, "table '{name}' already exists"),
            Self::TableNotFound(name) => write!(f, "table '{name}' does not exist"),
            Self::ColumnNotFound { table, column } => {
                write!(f, "column '{column}' does not exist in table '{table}'")
            }
            Self::DuplicateColumn { table, column } => {
                write!(f, "duplicate column '{column}' in table '{table}'")
            }
            Self::ValueCount {
                table,
                expected,
                actual,
            } => write!(f, "table '{table}' expects {expected} values, got {actual}"),
            Self::TypeMismatch {
                column,
                expected,
                actual,
            } => write!(f, "column '{column}' expects {expected}, got {actual}"),
            Self::MissingValue { table, column } => {
                write!(f, "missing value for column '{column}' in table '{table}'")
            }
            Self::InvalidLiteral(message) => write!(f, "{message}"),
            Self::Internal(message) => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for DbError {}

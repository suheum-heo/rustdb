//! Scalar types stored in a row.

use std::fmt;

/// A column type.
///
/// Integer SQL names (`INT`, `INTEGER`, `BIGINT`, and the smaller integer
/// aliases) all use this one 64-bit type. The original name is not retained,
/// so it is not a storage constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Signed 64-bit integer.
    Integer,
    /// Unicode text, stored as a Rust `String`.
    Text,
    /// Boolean.
    Boolean,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Boolean => "BOOLEAN",
        })
    }
}

/// One scalar cell.
///
/// `Null` is a value in every column. It has no runtime type of its own: a
/// null literal may be stored in an integer, text, or boolean column, and a
/// stored null still occupies that column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// Signed 64-bit integer.
    Integer(i64),
    /// Text.
    Text(String),
    /// Boolean.
    Boolean(bool),
}

impl Value {
    /// The runtime type of a non-null value.
    ///
    /// `NULL` returns [`None`] because a null literal is compatible with every
    /// column type. A column reference still has the column's declared type
    /// even when the stored cell is null.
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Self::Null => None,
            Self::Integer(_) => Some(DataType::Integer),
            Self::Text(_) => Some(DataType::Text),
            Self::Boolean(_) => Some(DataType::Boolean),
        }
    }

    /// Whether this cell is SQL `NULL`.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Text(value) => write!(f, "{value}"),
            Self::Boolean(value) => write!(f, "{value}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_scalars_without_quotes() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Integer(-12).to_string(), "-12");
        assert_eq!(Value::Text("Bob".to_string()).to_string(), "Bob");
        assert_eq!(Value::Boolean(true).to_string(), "true");
        assert_eq!(Value::Boolean(false).to_string(), "false");
    }

    #[test]
    fn reports_runtime_types() {
        assert_eq!(Value::Null.data_type(), None);
        assert!(Value::Null.is_null());
        assert_eq!(Value::Integer(1).data_type(), Some(DataType::Integer));
        assert_eq!(Value::Text(String::new()).data_type(), Some(DataType::Text));
        assert_eq!(Value::Boolean(false).data_type(), Some(DataType::Boolean));
        assert_eq!(DataType::Boolean.to_string(), "BOOLEAN");
    }
}

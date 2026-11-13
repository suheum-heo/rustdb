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
/// `NULL` is intentionally absent. Milestone 1 rejects null literals instead of
/// inventing a third state that later statements could not update or filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Signed 64-bit integer.
    Integer(i64),
    /// Text.
    Text(String),
    /// Boolean.
    Boolean(bool),
}

impl Value {
    /// The runtime type of this value.
    #[must_use]
    pub fn data_type(&self) -> DataType {
        match self {
            Self::Integer(_) => DataType::Integer,
            Self::Text(_) => DataType::Text,
            Self::Boolean(_) => DataType::Boolean,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
        assert_eq!(Value::Integer(-12).to_string(), "-12");
        assert_eq!(Value::Text("Bob".to_string()).to_string(), "Bob");
        assert_eq!(Value::Boolean(true).to_string(), "true");
        assert_eq!(Value::Boolean(false).to_string(), "false");
    }

    #[test]
    fn reports_runtime_types() {
        assert_eq!(Value::Integer(1).data_type(), DataType::Integer);
        assert_eq!(Value::Text(String::new()).data_type(), DataType::Text);
        assert_eq!(Value::Boolean(false).data_type(), DataType::Boolean);
        assert_eq!(DataType::Boolean.to_string(), "BOOLEAN");
    }
}

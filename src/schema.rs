//! Table schemas.

use std::collections::HashSet;

use crate::error::DbError;
use crate::value::DataType;

/// A named column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    name: String,
    data_type: DataType,
}

impl Column {
    pub(crate) fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn data_type(&self) -> DataType {
        self.data_type
    }
}

/// Ordered columns for one table.
///
/// Names stored here are already normalized: unquoted identifiers are
/// lowercased, and quoted identifiers keep their case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Schema {
    columns: Vec<Column>,
}

impl Schema {
    pub(crate) fn try_new(table: &str, columns: Vec<Column>) -> Result<Self, DbError> {
        if columns.is_empty() {
            return Err(DbError::invalid(
                "CREATE TABLE requires at least one column",
            ));
        }

        let mut seen = HashSet::with_capacity(columns.len());
        for column in &columns {
            if column.name.is_empty() {
                return Err(DbError::invalid("column name must not be empty"));
            }
            if !seen.insert(column.name.clone()) {
                return Err(DbError::DuplicateColumn {
                    table: table.to_string(),
                    column: column.name.clone(),
                });
            }
        }

        Ok(Self { columns })
    }

    pub(crate) fn len(&self) -> usize {
        self.columns.len()
    }

    pub(crate) fn column(&self, index: usize) -> Option<&Column> {
        self.columns.get(index)
    }

    pub(crate) fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// Map an `INSERT` column list to schema indexes.
    ///
    /// An empty list means "every column, in schema order". A non-empty list
    /// must name every column exactly once because there are no defaults.
    pub(crate) fn resolve_insert_columns(
        &self,
        table: &str,
        columns: &[String],
    ) -> Result<Vec<usize>, DbError> {
        if columns.is_empty() {
            return Ok((0..self.columns.len()).collect());
        }

        let mut positions = Vec::with_capacity(columns.len());
        let mut seen = vec![false; self.columns.len()];
        for name in columns {
            let Some(index) = self.index_of(name) else {
                return Err(DbError::ColumnNotFound {
                    table: table.to_string(),
                    column: name.clone(),
                });
            };
            if seen[index] {
                return Err(DbError::DuplicateColumn {
                    table: table.to_string(),
                    column: name.clone(),
                });
            }
            seen[index] = true;
            positions.push(index);
        }

        if let Some(index) = seen.iter().position(|present| !present) {
            return Err(DbError::MissingValue {
                table: table.to_string(),
                column: self.columns[index].name.clone(),
            });
        }

        Ok(positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, data_type: DataType) -> Column {
        Column::new(name, data_type)
    }

    #[test]
    fn rejects_an_empty_schema() {
        let error = Schema::try_new("users", Vec::new()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "CREATE TABLE requires at least one column"
        );
    }

    #[test]
    fn rejects_duplicate_columns() {
        let error = Schema::try_new(
            "users",
            vec![
                column("id", DataType::Integer),
                column("id", DataType::Text),
            ],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "duplicate column 'id' in table 'users'");
    }

    #[test]
    fn named_inserts_must_cover_every_column_once() {
        let schema = Schema::try_new(
            "users",
            vec![
                column("id", DataType::Integer),
                column("name", DataType::Text),
            ],
        )
        .unwrap();

        assert_eq!(
            schema
                .resolve_insert_columns("users", &["name".to_string(), "id".to_string()])
                .unwrap(),
            vec![1, 0]
        );

        let missing = schema
            .resolve_insert_columns("users", &["id".to_string()])
            .unwrap_err();
        assert_eq!(
            missing.to_string(),
            "missing value for column 'name' in table 'users'"
        );

        let duplicated = schema
            .resolve_insert_columns("users", &["id".to_string(), "id".to_string()])
            .unwrap_err();
        assert_eq!(
            duplicated.to_string(),
            "duplicate column 'id' in table 'users'"
        );
    }
}

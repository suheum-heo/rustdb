//! In-memory table storage.

use crate::error::DbError;
use crate::schema::{Column, Schema};
use crate::value::Value;

/// One relation: a fixed schema and an insertion-ordered row list.
///
/// Rows are stored in row-major order. That keeps `INSERT` and tuple
/// projection straightforward. A columnar layout would scan fewer bytes for
/// `SELECT col`, and it can wait until there is a measurement worth chasing.
#[derive(Debug)]
pub(crate) struct Table {
    name: String,
    schema: Schema,
    rows: Vec<Vec<Value>>,
}

impl Table {
    pub(crate) fn new(name: String, columns: Vec<Column>) -> Result<Self, DbError> {
        if name.is_empty() {
            return Err(DbError::invalid("table name must not be empty"));
        }
        let schema = Schema::try_new(&name, columns)?;
        Ok(Self {
            name,
            schema,
            rows: Vec::new(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn schema(&self) -> &Schema {
        &self.schema
    }

    pub(crate) fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// Validate every row, then append them.
    ///
    /// A type or arity error leaves the table unchanged, including when a later
    /// row in a multi-row `INSERT` is the one that fails.
    pub(crate) fn insert(
        &mut self,
        columns: &[String],
        rows: Vec<Vec<Value>>,
    ) -> Result<usize, DbError> {
        if rows.is_empty() {
            return Err(DbError::invalid("INSERT requires at least one row"));
        }

        let positions = self.schema.resolve_insert_columns(&self.name, columns)?;
        let mut prepared = Vec::with_capacity(rows.len());
        for row in rows {
            prepared.push(self.align_row(&positions, row)?);
        }

        let count = prepared.len();
        self.rows.extend(prepared);
        Ok(count)
    }

    fn align_row(&self, positions: &[usize], row: Vec<Value>) -> Result<Vec<Value>, DbError> {
        if row.len() != positions.len() {
            return Err(DbError::ValueCount {
                table: self.name.clone(),
                expected: positions.len(),
                actual: row.len(),
            });
        }

        let mut aligned = vec![None; self.schema.len()];
        for (value, index) in row.into_iter().zip(positions.iter().copied()) {
            let Some(column) = self.schema.column(index) else {
                return Err(DbError::Internal(format!(
                    "insert column index {index} is outside table '{}'",
                    self.name
                )));
            };
            if value.data_type() != column.data_type() {
                return Err(DbError::TypeMismatch {
                    column: column.name().to_string(),
                    expected: column.data_type(),
                    actual: value.data_type(),
                });
            }
            aligned[index] = Some(value);
        }

        let mut stored = Vec::with_capacity(aligned.len());
        for (index, slot) in aligned.into_iter().enumerate() {
            match slot {
                Some(value) => stored.push(value),
                None => {
                    return Err(DbError::Internal(format!(
                        "insert into '{}' left column {index} unset",
                        self.name
                    )));
                }
            }
        }
        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::DataType;

    fn users() -> Table {
        Table::new(
            "users".to_string(),
            vec![
                Column::new("id", DataType::Integer),
                Column::new("name", DataType::Text),
            ],
        )
        .unwrap()
    }

    #[test]
    fn rejects_a_type_mismatch_without_storing_the_row() {
        let mut table = users();
        let error = table
            .insert(
                &[],
                vec![vec![
                    Value::Text("nope".to_string()),
                    Value::Text("Ada".to_string()),
                ]],
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "column 'id' expects INTEGER, got TEXT");
        assert!(table.rows().is_empty());
    }

    #[test]
    fn rejects_the_wrong_number_of_values() {
        let mut table = users();
        let error = table
            .insert(&[], vec![vec![Value::Integer(1)]])
            .unwrap_err();
        assert_eq!(error.to_string(), "table 'users' expects 2 values, got 1");
        assert!(table.rows().is_empty());
    }

    #[test]
    fn multi_row_insert_is_atomic_and_preserves_order() {
        let mut table = users();
        let error = table
            .insert(
                &[],
                vec![
                    vec![Value::Integer(1), Value::Text("Ada".to_string())],
                    vec![Value::Integer(2), Value::Integer(3)],
                ],
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "column 'name' expects TEXT, got INTEGER");
        assert!(table.rows().is_empty());

        let count = table
            .insert(
                &["name".to_string(), "id".to_string()],
                vec![
                    vec![Value::Text("Ada".to_string()), Value::Integer(1)],
                    vec![Value::Text("Bea".to_string()), Value::Integer(2)],
                ],
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            table.rows(),
            &[
                vec![Value::Integer(1), Value::Text("Ada".to_string())],
                vec![Value::Integer(2), Value::Text("Bea".to_string())],
            ]
        );
    }
}

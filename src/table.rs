//! In-memory table storage.
//!
//! Durable databases keep the same row vector while a statement runs. The
//! store publishes those rows into slotted heap pages after the statement
//! succeeds. Each row has an id assigned at insert time. `UPDATE` keeps ids
//! by position, and `DELETE` keeps the ids of the surviving rows.

use crate::error::DbError;
use crate::schema::{Column, Schema};
use crate::storage::{TableImage, ensure_record_fits};
use crate::value::Value;

/// One relation: a fixed schema and an insertion-ordered row list.
#[derive(Debug, Clone)]
pub(crate) struct Table {
    name: String,
    schema: Schema,
    rows: Vec<Vec<Value>>,
    row_ids: Vec<u64>,
    next_row_id: u64,
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
            row_ids: Vec::new(),
            next_row_id: 1,
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
        for row in &prepared {
            ensure_record_fits(row)?;
        }

        let count = prepared.len();
        let count_u64 = u64::try_from(count)
            .map_err(|_| DbError::Storage("row id space is exhausted".to_string()))?;
        let end = self
            .next_row_id
            .checked_add(count_u64)
            .ok_or_else(|| DbError::Storage("row id space is exhausted".to_string()))?;
        self.row_ids.extend(self.next_row_id..end);
        self.next_row_id = end;
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
            if let Some(actual) = value.data_type()
                && actual != column.data_type()
            {
                return Err(DbError::TypeMismatch {
                    column: column.name().to_string(),
                    expected: column.data_type(),
                    actual,
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

    /// Replace every row after a successful `UPDATE`.
    ///
    /// `rows` must contain one value list per existing row, in the same order.
    /// Row ids stay with those positions. A width or size error leaves the
    /// current rows in place.
    pub(crate) fn rewrite_all(&mut self, rows: Vec<Vec<Value>>) -> Result<(), DbError> {
        if rows.len() != self.rows.len() {
            return Err(DbError::Internal(format!(
                "replacement for '{}' has {} rows, expected {}",
                self.name,
                rows.len(),
                self.rows.len()
            )));
        }
        for (index, row) in rows.iter().enumerate() {
            if row.len() != self.schema.len() {
                return Err(DbError::Internal(format!(
                    "replacement row {index} in '{}' has {} values, expected {}",
                    self.name,
                    row.len(),
                    self.schema.len()
                )));
            }
            ensure_record_fits(row)?;
        }
        self.rows = rows;
        Ok(())
    }

    /// Keep rows at `indexes`, which must be strictly increasing.
    ///
    /// Surviving row ids stay attached to those rows. This is how `DELETE`
    /// preserves identity without matching values that may not be unique.
    pub(crate) fn retain_indexes(&mut self, indexes: &[usize]) -> Result<(), DbError> {
        if self.row_ids.len() != self.rows.len() {
            return Err(DbError::Internal(format!(
                "table '{}' has {} row ids for {} rows",
                self.name,
                self.row_ids.len(),
                self.rows.len()
            )));
        }
        let mut kept_rows = Vec::with_capacity(indexes.len());
        let mut kept_ids = Vec::with_capacity(indexes.len());
        let mut previous = None;
        for index in indexes {
            if *index >= self.rows.len() || previous.is_some_and(|seen: usize| *index <= seen) {
                return Err(DbError::Internal(format!(
                    "cannot retain row {index} from table '{}'",
                    self.name
                )));
            }
            previous = Some(*index);
            kept_rows.push(self.rows[*index].clone());
            kept_ids.push(self.row_ids[*index]);
        }
        self.rows = kept_rows;
        self.row_ids = kept_ids;
        Ok(())
    }

    pub(crate) fn from_image(image: TableImage) -> Result<Self, DbError> {
        let schema = Schema::try_new(&image.name, image.columns)?;
        let mut rows = Vec::with_capacity(image.rows.len());
        let mut row_ids = Vec::with_capacity(image.rows.len());
        for (row_id, row) in image.rows {
            if row.len() != schema.len() {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
            row_ids.push(row_id);
            rows.push(row);
        }
        Ok(Self {
            name: image.name,
            schema,
            rows,
            row_ids,
            next_row_id: image.next_row_id,
        })
    }

    pub(crate) fn image(&self) -> TableImage {
        TableImage {
            name: self.name.clone(),
            columns: self.schema.columns().to_vec(),
            table_id: 0,
            next_row_id: self.next_row_id,
            rows: self
                .row_ids
                .iter()
                .copied()
                .zip(self.rows.iter().cloned())
                .collect(),
        }
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

    #[test]
    fn insert_rejects_a_row_id_that_would_overflow() {
        let mut table = users();
        table.next_row_id = u64::MAX;
        let error = table
            .insert(
                &[],
                vec![vec![Value::Integer(1), Value::Text("Ada".to_string())]],
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "row id space is exhausted");
        assert!(table.rows().is_empty());
        assert!(table.row_ids.is_empty());
        assert_eq!(table.next_row_id, u64::MAX);
    }
}

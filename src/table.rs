//! In-memory table storage.
//!
//! Durable databases keep the same row vector while a statement runs. The
//! store publishes those rows into slotted heap pages after the statement
//! succeeds. Each row has an id assigned at insert time. `UPDATE` keeps ids
//! and can replace only the matched rows. `DELETE` keeps the ids of the
//! surviving rows and does not reuse a removed id.
//!
//! `positions` maps those stable ids to the current vector slot. It is rebuilt
//! when `DELETE` compacts the vector and when a failed `DELETE` splices the
//! removed rows back. It is not written to the file.

use std::collections::{HashMap, HashSet};

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
    /// Average O(1) lookup from a stable row id to its current vector slot.
    positions: HashMap<u64, usize>,
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
            positions: HashMap::new(),
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

    /// Current vector slot for a stable row id.
    ///
    /// `None` means the id is not in this table. A slot from before a `DELETE`
    /// is not reused for a different id: compaction rebuilds the map.
    pub(crate) fn position(&self, row_id: u64) -> Option<usize> {
        self.positions.get(&row_id).copied()
    }

    pub(crate) fn row_id_at(&self, position: usize) -> Option<u64> {
        self.row_ids.get(position).copied()
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
        let base = self.rows.len();
        self.row_ids.extend(self.next_row_id..end);
        self.next_row_id = end;
        self.rows.extend(prepared);
        for (offset, row_id) in self.row_ids[base..].iter().copied().enumerate() {
            if self.positions.insert(row_id, base + offset).is_some() {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
        }
        Ok(count)
    }

    /// Undo only the appended suffix. Validate before changing any memory;
    /// retained rows, positions, schema and index definitions are untouched.
    pub(crate) fn restore_insert(&mut self, rows: usize, next_row_id: u64) -> Result<(), DbError> {
        let invalid = || DbError::Storage("insert undo boundary is inconsistent".into());
        let added = self.rows.len().checked_sub(rows).ok_or_else(invalid)?;
        let added = u64::try_from(added).map_err(|_| invalid())?;
        if self.row_ids.len() != self.rows.len()
            || self.positions.len() != self.rows.len()
            || next_row_id.checked_add(added) != Some(self.next_row_id)
        {
            return Err(invalid());
        }
        for (offset, id) in self.row_ids[rows..].iter().copied().enumerate() {
            if next_row_id.checked_add(offset as u64) != Some(id)
                || self.positions.get(&id) != Some(&(rows + offset))
            {
                return Err(invalid());
            }
        }
        for id in &self.row_ids[rows..] {
            self.positions.remove(id);
        }
        self.rows.truncate(rows);
        self.row_ids.truncate(rows);
        self.next_row_id = next_row_id;
        Ok(())
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
            value.ensure_persistent()?;
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

    /// Replace the listed rows after every new image has been checked.
    ///
    /// Row ids stay put. A missing id, a duplicate id, a width error, a type
    /// error, or a record that does not fit leaves every current row in place.
    /// Unlisted rows are not cloned and are not compared.
    pub(crate) fn rewrite_matched(
        &mut self,
        updates: Vec<(u64, Vec<Value>)>,
    ) -> Result<(), DbError> {
        let mut planned = Vec::with_capacity(updates.len());
        let mut seen = HashSet::with_capacity(updates.len());
        for (row_id, values) in updates {
            if !seen.insert(row_id) {
                return Err(DbError::Internal(format!(
                    "update lists row {row_id} twice in '{}'",
                    self.name
                )));
            }
            let Some(position) = self.position(row_id) else {
                return Err(DbError::Internal(format!(
                    "update target {row_id} is not in '{}'",
                    self.name
                )));
            };
            self.check_row(&values)?;
            planned.push((position, values));
        }
        for (position, values) in planned {
            self.rows[position] = values;
        }
        Ok(())
    }

    /// Confirm every saved image still names a live slot.
    ///
    /// This writes nothing. Rollback uses it before moving the images back so
    /// a missing id does not leave a prefix of the statement restored.
    pub(crate) fn check_restore(&self, rows: &[(u64, Vec<Value>)]) -> Result<(), DbError> {
        for (row_id, _) in rows {
            let Some(position) = self.position(*row_id) else {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is not in '{}'",
                    self.name
                )));
            };
            if self.rows.get(position).is_none() {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is not in '{}'",
                    self.name
                )));
            }
        }
        Ok(())
    }

    /// Move saved images back into their slots.
    ///
    /// `check_restore` has already accepted every id. A miss here means the
    /// map changed after that check. Earlier images in `rows` may already
    /// have been written; the caller fences instead of reporting success.
    pub(crate) fn restore_images(&mut self, rows: Vec<(u64, Vec<Value>)>) -> Result<(), DbError> {
        for (row_id, values) in rows {
            let Some(position) = self.position(row_id) else {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is not in '{}'",
                    self.name
                )));
            };
            if self.rows.get(position).is_none() {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is not in '{}'",
                    self.name
                )));
            }
            self.rows[position] = values;
        }
        Ok(())
    }

    /// Check row ids and cell types the way a full publish does, without cloning
    /// or encoding rows this statement does not rewrite.
    pub(crate) fn audit(&self) -> Result<(), DbError> {
        if self.next_row_id == 0 {
            return Err(DbError::Internal(
                "table row-id counter must start at 1".to_string(),
            ));
        }
        if self.row_ids.len() != self.rows.len() {
            return Err(DbError::Internal(format!(
                "table '{}' has {} row ids for {} rows",
                self.name,
                self.row_ids.len(),
                self.rows.len()
            )));
        }
        let mut seen = HashSet::with_capacity(self.rows.len());
        for (row_id, values) in self.row_ids.iter().zip(self.rows.iter()) {
            if *row_id == 0 || *row_id >= self.next_row_id || !seen.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "row id {row_id} is outside the counter for table '{}'",
                    self.name
                )));
            }
            self.check_row_shape(values)?;
        }
        Ok(())
    }

    /// Check every in-memory row the way a full publish does, without cloning it.
    ///
    /// [`Self::audit`] is the check a selective `UPDATE` uses. `DELETE` also
    /// rejects a survivor that does not fit in one record, which is what
    /// `validate` did on the table image. Neither check encodes the row.
    pub(crate) fn audit_for_publish(&self) -> Result<(), DbError> {
        self.audit()?;
        for values in &self.rows {
            ensure_record_fits(values)?;
        }
        Ok(())
    }

    pub(crate) fn row_ids(&self) -> &[u64] {
        &self.row_ids
    }

    pub(crate) fn next_row_id(&self) -> u64 {
        self.next_row_id
    }

    fn check_row(&self, values: &[Value]) -> Result<(), DbError> {
        self.check_row_shape(values)?;
        ensure_record_fits(values)
    }

    /// Width and runtime type. This does not encode the row.
    ///
    /// Publish uses it for every in-memory row, including rows this statement
    /// does not rewrite. A stored row that already fits is not encoded again.
    fn check_row_shape(&self, values: &[Value]) -> Result<(), DbError> {
        if values.len() != self.schema.len() {
            return Err(DbError::Internal(format!(
                "row in '{}' does not match its schema",
                self.name
            )));
        }
        for (value, column) in values.iter().zip(self.schema.columns()) {
            if let Some(actual) = value.data_type()
                && actual != column.data_type()
            {
                return Err(DbError::Internal(format!(
                    "row in '{}' has a value that does not match its column",
                    self.name
                )));
            }
        }
        Ok(())
    }

    /// Keep rows at `indexes`, which must be strictly increasing.
    ///
    /// Surviving row ids stay attached to those rows. This is how `DELETE`
    /// preserves identity without matching values that may not be unique.
    /// Survivor images are moved, not cloned. The returned time is the rebuild
    /// of the row-id map. Keeping every row does not allocate.
    ///
    /// The replacement vectors reserve room for the original row count before
    /// the table changes. A reservation failure leaves this table as it was.
    /// Restore allocates its own vectors; it does not reuse this spare capacity.
    pub(crate) fn retain_indexes(&mut self, indexes: &[usize]) -> Result<u128, DbError> {
        if self.row_ids.len() != self.rows.len() {
            return Err(DbError::Internal(format!(
                "table '{}' has {} row ids for {} rows",
                self.name,
                self.row_ids.len(),
                self.rows.len()
            )));
        }
        if indexes.len() == self.rows.len() {
            for (expected, index) in indexes.iter().enumerate() {
                if *index != expected {
                    return Err(DbError::Internal(format!(
                        "cannot retain row {index} from table '{}'",
                        self.name
                    )));
                }
            }
            return Ok(0);
        }
        let mut previous = None;
        for index in indexes {
            if *index >= self.rows.len() || previous.is_some_and(|seen: usize| *index <= seen) {
                return Err(DbError::Internal(format!(
                    "cannot retain row {index} from table '{}'",
                    self.name
                )));
            }
            previous = Some(*index);
        }
        let original = self.rows.len();
        let mut kept_rows = Vec::new();
        kept_rows
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut kept_ids = Vec::new();
        kept_ids
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut positions = HashMap::new();
        positions
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut seen = HashSet::new();
        seen.try_reserve(indexes.len())
            .map_err(|_| restoration_capacity())?;
        for index in indexes {
            if !seen.insert(self.row_ids[*index]) {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
        }

        let mut rows = std::mem::take(&mut self.rows);
        let row_ids = std::mem::take(&mut self.row_ids);
        for index in indexes {
            kept_rows.push(std::mem::take(&mut rows[*index]));
            kept_ids.push(row_ids[*index]);
        }
        drop(rows);
        let started = std::time::Instant::now();
        for (position, row_id) in kept_ids.iter().copied().enumerate() {
            positions.insert(row_id, position);
        }
        let positions_us = started.elapsed().as_micros();
        self.rows = kept_rows;
        self.row_ids = kept_ids;
        self.positions = positions;
        Ok(positions_us)
    }

    /// Confirm `rows` can be spliced back without writing.
    ///
    /// `rows` is `(original position, row id)` in increasing position. A live
    /// id, a repeated id, or a position that is not a hole left by compaction
    /// fails before any slot changes.
    pub(crate) fn check_delete_restore(&self, rows: &[(usize, u64)]) -> Result<(), DbError> {
        if self.row_ids.len() != self.rows.len() {
            return Err(DbError::Internal(format!(
                "table '{}' has {} row ids for {} rows",
                self.name,
                self.row_ids.len(),
                self.rows.len()
            )));
        }
        let mut live = HashSet::new();
        live.try_reserve(self.row_ids.len().saturating_add(rows.len()))
            .map_err(|_| restoration_capacity())?;
        for row_id in &self.row_ids {
            if !live.insert(*row_id) {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
        }
        let mut previous = None;
        for (index, (position, row_id)) in rows.iter().enumerate() {
            if *row_id == 0 || *row_id >= self.next_row_id {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is outside the counter for table '{}'",
                    self.name
                )));
            }
            if !live.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} is already in '{}'",
                    self.name
                )));
            }
            let limit = self.rows.len() + index;
            if previous.is_some_and(|seen| *position <= seen) || *position > limit {
                return Err(DbError::Internal(format!(
                    "undo row {row_id} cannot return to position {position} in '{}'",
                    self.name
                )));
            }
            previous = Some(*position);
        }
        Ok(())
    }

    /// Splice removed rows back at their original positions.
    ///
    /// Validation, the placement plan, and vector reservation all finish
    /// before a live row is moved. The completed vectors then replace this
    /// table. A failure before that replacement leaves the compacted rows and
    /// `rows` in place. Filling the plan does not return a query error: every
    /// index is in range and every push fits the reservation. An allocator
    /// abort while moving is not a query error. Row ids and `next_row_id` are
    /// not reassigned.
    pub(crate) fn restore_deleted(
        &mut self,
        rows: &mut Vec<(usize, u64, Vec<Value>)>,
    ) -> Result<(), DbError> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut pairs = Vec::new();
        pairs
            .try_reserve(rows.len())
            .map_err(|_| restoration_capacity())?;
        for (position, row_id, _) in rows.iter() {
            pairs.push((*position, *row_id));
        }
        self.check_delete_restore(&pairs)?;
        let survivors = self.rows.len();
        let Some(original) = survivors.checked_add(rows.len()) else {
            return Err(restoration_capacity());
        };
        let mut plan = Vec::new();
        plan.try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut deleted_at = 0usize;
        let mut survivor_at = 0usize;
        for position in 0..original {
            if deleted_at < rows.len() && rows[deleted_at].0 == position {
                plan.push(RestorePlace::Deleted(deleted_at));
                deleted_at += 1;
            } else if survivor_at < survivors {
                plan.push(RestorePlace::Survivor(survivor_at));
                survivor_at += 1;
            } else {
                return Err(DbError::Internal(format!(
                    "undo could not place position {position} in '{}'",
                    self.name
                )));
            }
        }
        if deleted_at != rows.len() || survivor_at != survivors {
            return Err(DbError::Internal(format!(
                "undo restore did not consume table '{}'",
                self.name
            )));
        }
        let mut merged_rows = Vec::new();
        merged_rows
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut merged_ids = Vec::new();
        merged_ids
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut positions = HashMap::new();
        positions
            .try_reserve(original)
            .map_err(|_| restoration_capacity())?;
        let mut deleted_ids = Vec::new();
        deleted_ids
            .try_reserve(rows.len())
            .map_err(|_| restoration_capacity())?;
        let mut deleted_values = Vec::new();
        deleted_values
            .try_reserve(rows.len())
            .map_err(|_| restoration_capacity())?;

        let mut old_rows = std::mem::take(&mut self.rows);
        let old_ids = std::mem::take(&mut self.row_ids);
        for (_, row_id, values) in std::mem::take(rows) {
            deleted_ids.push(row_id);
            deleted_values.push(values);
        }
        for place in plan {
            let (row_id, values) = match place {
                RestorePlace::Survivor(index) => {
                    (old_ids[index], std::mem::take(&mut old_rows[index]))
                }
                RestorePlace::Deleted(index) => (
                    deleted_ids[index],
                    std::mem::take(&mut deleted_values[index]),
                ),
            };
            let slot = merged_rows.len();
            positions.insert(row_id, slot);
            merged_ids.push(row_id);
            merged_rows.push(values);
        }
        self.rows = merged_rows;
        self.row_ids = merged_ids;
        self.positions = positions;
        Ok(())
    }

    fn rebuild_positions(&mut self) -> Result<(), DbError> {
        self.positions.clear();
        self.positions
            .try_reserve(self.row_ids.len())
            .map_err(|_| restoration_capacity())?;
        for (position, row_id) in self.row_ids.iter().copied().enumerate() {
            if self.positions.insert(row_id, position).is_some() {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
        }
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
        let mut table = Self {
            name: image.name,
            schema,
            rows,
            row_ids,
            positions: HashMap::new(),
            next_row_id: image.next_row_id,
        };
        table.rebuild_positions()?;
        Ok(table)
    }

    #[cfg(test)]
    pub(crate) fn overwrite_cell_for_test(
        &mut self,
        position: usize,
        ordinal: usize,
        value: Value,
    ) {
        self.rows[position][ordinal] = value;
    }

    #[cfg(test)]
    pub(crate) fn forget_position_for_test(&mut self, row_id: u64) {
        self.positions.remove(&row_id);
    }

    #[cfg(test)]
    pub(crate) fn move_position_for_test(&mut self, row_id: u64, position: usize) {
        self.positions.insert(row_id, position);
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

enum RestorePlace {
    Survivor(usize),
    Deleted(usize),
}

fn restoration_capacity() -> DbError {
    DbError::Internal("delete could not reserve restoration capacity".to_string())
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
    fn append_restore_checks_boundary_and_restores_extreme_counter() {
        let mut table = users();
        table.next_row_id = u64::MAX - 2;
        let mark = table.next_row_id;
        table
            .insert(
                &[],
                vec![
                    vec![Value::Integer(1), Value::Text("a".into())],
                    vec![Value::Integer(2), Value::Text("b".into())],
                ],
            )
            .unwrap();
        let before = table.clone();
        assert!(table.restore_insert(3, mark).is_err());
        assert!(table.restore_insert(0, mark - 1).is_err());
        assert_eq!(table.rows, before.rows);
        assert_eq!(table.row_ids, before.row_ids);
        assert_eq!(table.positions, before.positions);
        assert_eq!(table.next_row_id, before.next_row_id);
        table.positions.insert(mark, 1);
        assert!(table.restore_insert(0, mark).is_err());
        assert_eq!(table.rows, before.rows);
        table.positions.insert(mark, 0);
        table.restore_insert(0, mark).unwrap();
        assert!(table.rows.is_empty());
        assert!(table.row_ids.is_empty());
        assert!(table.positions.is_empty());
        assert_eq!(table.next_row_id, mark);
        table
            .insert(
                &[],
                vec![vec![Value::Integer(3), Value::Text("retry".into())]],
            )
            .unwrap();
        assert_eq!(table.row_ids, vec![mark]);
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
        assert!(table.positions.is_empty());
        assert_eq!(table.next_row_id, u64::MAX);
    }

    #[test]
    fn positions_follow_insert_update_and_compaction() {
        let mut table = users();
        table
            .insert(
                &[],
                vec![
                    vec![Value::Integer(1), Value::Text("a".to_string())],
                    vec![Value::Integer(2), Value::Text("b".to_string())],
                    vec![Value::Integer(3), Value::Text("c".to_string())],
                ],
            )
            .unwrap();
        assert_eq!(table.position(1), Some(0));
        assert_eq!(table.position(2), Some(1));
        assert_eq!(table.position(3), Some(2));

        table
            .rewrite_all(vec![
                vec![Value::Integer(1), Value::Text("a2".to_string())],
                vec![Value::Integer(2), Value::Text("b".to_string())],
                vec![Value::Integer(3), Value::Text("c".to_string())],
            ])
            .unwrap();
        assert_eq!(table.position(1), Some(0));
        assert_eq!(table.rows()[0][1], Value::Text("a2".to_string()));

        table.retain_indexes(&[0, 2]).unwrap();
        assert_eq!(table.position(1), Some(0));
        assert_eq!(table.position(2), None);
        assert_eq!(table.position(3), Some(1));
        assert_eq!(table.rows().len(), 2);

        table
            .insert(
                &[],
                vec![vec![Value::Integer(4), Value::Text("d".to_string())]],
            )
            .unwrap();
        assert_eq!(table.position(4), Some(2));
        assert_eq!(table.position(3), Some(1));

        let loaded = Table::from_image(table.image()).unwrap();
        assert_eq!(loaded.position(1), Some(0));
        assert_eq!(loaded.position(3), Some(1));
        assert_eq!(loaded.position(4), Some(2));
        assert_eq!(loaded.position(2), None);

        let mut image = table.image();
        image.rows.push(image.rows[0].clone());
        let error = Table::from_image(image).unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
    }

    #[test]
    fn restore_deleted_puts_every_shape_back() {
        let mut table = users();
        table
            .insert(
                &[],
                vec![
                    vec![Value::Integer(1), Value::Text("a".to_string())],
                    vec![Value::Integer(2), Value::Text("b".to_string())],
                    vec![Value::Integer(3), Value::Text("c".to_string())],
                    vec![Value::Integer(4), Value::Text("d".to_string())],
                ],
            )
            .unwrap();
        let next = table.next_row_id();
        let original = table.rows().to_vec();
        let original_ids = table.row_ids().to_vec();

        for kept in [
            vec![1, 2, 3],
            vec![0, 2, 3],
            vec![0, 1, 2],
            vec![0, 3],
            vec![1],
            vec![],
            vec![0, 1, 2, 3],
        ] {
            table.retain_indexes(&kept).unwrap();
            let mut removed: Vec<_> = (0..original.len())
                .filter(|position| !kept.contains(position))
                .map(|position| (position, original_ids[position], original[position].clone()))
                .collect();
            table.restore_deleted(&mut removed).unwrap();
            assert!(removed.is_empty());
            assert_eq!(table.rows(), original.as_slice());
            assert_eq!(table.row_ids(), original_ids.as_slice());
            assert_eq!(table.next_row_id(), next);
            for (position, row_id) in original_ids.iter().copied().enumerate() {
                assert_eq!(table.position(row_id), Some(position));
            }
        }

        table.retain_indexes(&[0, 2, 3]).unwrap();
        let mut duplicate = vec![(1, 1, vec![Value::Integer(9), Value::Text("x".to_string())])];
        let error = table.restore_deleted(&mut duplicate).unwrap_err();
        assert!(error.to_string().contains("already in"), "{error}");
        assert_eq!(duplicate.len(), 1);
        assert_eq!(table.row_ids(), &[1, 3, 4]);
        assert_eq!(table.rows().len(), 3);
        assert_eq!(table.next_row_id(), next);
        assert_eq!(table.position(1), Some(0));
        assert_eq!(table.position(2), None);

        let mut impossible = vec![(9, 2, vec![Value::Integer(2), Value::Text("b".to_string())])];
        let error = table.restore_deleted(&mut impossible).unwrap_err();
        assert!(error.to_string().contains("cannot return"), "{error}");
        assert_eq!(impossible.len(), 1);
        assert_eq!(table.row_ids(), &[1, 3, 4]);
        assert_eq!(table.rows().len(), 3);
        assert_eq!(table.position(2), None);
        assert_eq!(table.next_row_id(), next);
    }
}

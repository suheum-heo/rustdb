//! Catalog of tables, either in memory or backed by a database file.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::error::DbError;
use crate::schema::Column;
use crate::storage::{IndexImage, StepError, Store, TableImage};
use crate::table::Table;
use crate::value::Value;

/// A database holding named tables.
///
/// [`Database::new`] keeps the catalog in memory. [`Database::open`] loads a
/// page file and publishes each successful mutating statement back to that
/// file. One process holds an exclusive lock on that file. There is one catalog.
pub struct Database {
    tables: HashMap<String, Table>,
    indexes: Vec<IndexImage>,
    store: Option<Store>,
    /// Matched row ids from the current `UPDATE`.
    ///
    /// Publish rewrites those rows and leaves the others unread. `None` means
    /// this statement is not an update, so publish compares the whole image.
    selective: Option<(String, Vec<u64>)>,
    /// Deleted row ids from the current `DELETE`.
    ///
    /// These are the ids captured before compaction, copied so the undo journal
    /// keeps the images and the original positions. Publish unlinks this set
    /// through the existing editor and does not clone surviving values. `None`
    /// means this statement is not a selective delete. An empty list is a
    /// delete that matched nothing: memory is still audited, stored rows are
    /// still decoded, and nothing is written when they already match.
    delete_targets: Option<(String, Vec<u64>)>,
    /// Original images for the current selective `UPDATE`.
    ///
    /// `None` means this statement did not capture a journal. An empty journal
    /// means it matched nothing. A second capture refuses to replace images
    /// that are already saved.
    update_undo: Option<UpdateJournal>,
    /// Removed rows for the current selective `DELETE`.
    ///
    /// `None` means this statement did not capture a journal. An empty journal
    /// is a delete that matched nothing. The images are not enough on their
    /// own: each entry also keeps the position the row had before compaction.
    delete_undo: Option<DeleteJournal>,
}

/// Previous images of the rows one `UPDATE` is about to replace.
///
/// The journal is statement-local. It is not a WAL record. `UPDATE` does not
/// change row ids, positions, the row-id counter, or the index catalog, and
/// the committed store changes only after checkpoint succeeds. Restoring these
/// images therefore restores the in-memory database. Heap relocation and index
/// maintenance happen on editor copies and are discarded when publish fails
/// before that commit. `INSERT` and DDL still snapshot the structures they
/// change. `DELETE` journals the rows it removes because compaction also
/// changes order and the row-id map.
struct UpdateJournal {
    table: String,
    rows: Vec<(u64, Vec<Value>)>,
}

/// One row a `DELETE` removed, in the order it had before compaction.
struct DeletedRow {
    position: usize,
    row_id: u64,
    values: Vec<Value>,
}

/// Previous images and positions for one selective `DELETE`.
///
/// The journal is statement-local and is not a WAL record. `next_row_id` is
/// recorded so restore can reject a counter change; `DELETE` does not advance
/// it. `applied` is set only after compaction. A capture that never compacts
/// is discarded instead of spliced back over rows that are still present.
/// Index maintenance stays on the editor copy until checkpoint succeeds.
struct DeleteJournal {
    table: String,
    rows: Vec<DeletedRow>,
    heap_rows: usize,
    next_row_id: u64,
    applied: bool,
}

/// What `capture_update_undo` kept, before byte estimates.
pub(crate) struct CapturedUndo {
    /// Journal rows. Zero when the full snapshot was used instead.
    pub entries: usize,
    /// `true` when this statement kept the pre-statement table clone.
    pub snapshot: bool,
    /// Heap rows at capture time. This is the snapshot's row count.
    pub heap_rows: usize,
    /// Columns in the table.
    pub width: usize,
}

/// What a durable statement changed, used to undo memory if publishing fails.
#[derive(Debug)]
pub(crate) enum DurableChange {
    /// `SELECT`, or a statement that does not reach the catalog.
    None,
    /// `CREATE TABLE` added this name.
    Create(String),
    /// `DROP TABLE` removed these names.
    Drop(Vec<String>),
    /// `INSERT` changed this table.
    ///
    /// A test can also force `UPDATE` or `DELETE` through this path so the old
    /// full-table snapshot stays available for comparison.
    Write(String),
    /// `UPDATE` will restore the row journal captured during install.
    Update,
    /// `DELETE` will restore the row journal captured before compaction.
    Delete,
    /// `CREATE INDEX` or `DROP INDEX` changed the index list.
    Index,
}

/// In-memory snapshot restored when a durable publish fails.
pub(crate) enum Undo {
    /// Nothing was captured because the statement does not mutate storage.
    None,
    /// Remove a table created by the statement and restore the index list.
    Remove {
        /// Table created by the statement.
        table: String,
        /// Index list from before the statement.
        indexes: Vec<IndexImage>,
    },
    /// Put dropped tables and the previous index list back.
    Restore {
        /// Tables removed by the statement.
        tables: Vec<Table>,
        /// Index list from before the statement.
        indexes: Vec<IndexImage>,
    },
    /// Replace one table and restore the index list.
    Replace {
        /// Table from before the statement.
        table: Table,
        /// Index list from before the statement.
        indexes: Vec<IndexImage>,
    },
    /// Restore the index list after `CREATE INDEX` or `DROP INDEX`.
    Indexes(Vec<IndexImage>),
    /// Restore the `UPDATE` journal captured before any row was replaced.
    ///
    /// The images live on the database, not in this value, because the matched
    /// rows are not known when the statement starts.
    Selective,
    /// Restore the `DELETE` journal captured before any row was removed.
    ///
    /// The removed rows live on the database. Their ids are not known when the
    /// statement starts, and restoring them has to put them back at the
    /// positions compaction closed.
    Delete,
}

impl Database {
    /// Create an empty in-memory database.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            indexes: Vec::new(),
            store: None,
            selective: None,
            delete_targets: None,
            update_undo: None,
            delete_undo: None,
        }
    }

    /// Open a database file, creating it when the path is missing or empty.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let (store, images) = Store::open(path.as_ref())?;
        let indexes = store.indexes();
        let mut tables = HashMap::with_capacity(images.len());
        for image in images {
            let table = Table::from_image(image)?;
            let name = table.name().to_string();
            if tables.contains_key(&name) {
                return Err(DbError::Storage("database file is corrupt".to_string()));
            }
            tables.insert(name, table);
        }
        Ok(Self {
            tables,
            indexes,
            store: Some(store),
            selective: None,
            delete_targets: None,
            update_undo: None,
            delete_undo: None,
        })
    }

    /// Rewrite a version 1 file as version 2, or a version 2 file as version 3.
    ///
    /// A version 3 file is left unchanged and this returns an error. New
    /// databases stay on version 2 until this is called.
    pub fn migrate(path: impl AsRef<Path>) -> Result<(), DbError> {
        Store::migrate(path.as_ref())
    }

    pub(crate) fn fenced(&self) -> Option<DbError> {
        self.store.as_ref().and_then(Store::fenced)
    }

    pub(crate) fn stores_on_disk(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) fn prepare_durable(&self, change: &DurableChange) -> Undo {
        let _prepare = crate::storage::Timer::new(|p| &mut p.durable_prepare);
        match change {
            DurableChange::None => Undo::None,
            DurableChange::Create(name) => Undo::Remove {
                table: name.clone(),
                indexes: self.indexes.clone(),
            },
            DurableChange::Drop(names) => {
                let mut tables = Vec::with_capacity(names.len());
                for name in names {
                    let Some(table) = self.tables.get(name) else {
                        return Undo::None;
                    };
                    tables.push(table.clone());
                }
                Undo::Restore {
                    tables,
                    indexes: self.indexes.clone(),
                }
            }
            DurableChange::Write(name) => match self.tables.get(name) {
                Some(table) => {
                    let started = std::time::Instant::now();
                    let undo = Undo::Replace {
                        table: table.clone(),
                        indexes: self.indexes.clone(),
                    };
                    note_table_snapshot(started.elapsed().as_micros());
                    undo
                }
                None => Undo::None,
            },
            DurableChange::Update => Undo::Selective,
            DurableChange::Delete => Undo::Delete,
            DurableChange::Index => Undo::Indexes(self.indexes.clone()),
        }
    }

    /// Save the current images of `proposals` before any of those slots change.
    ///
    /// `snapshot` skips the journal because the caller already cloned the
    /// table. Reservation failure returns before a slot is written. Cloning a
    /// row uses the process allocator; an out-of-memory abort there is not a
    /// query error, and it still happens before the first assignment.
    pub(crate) fn capture_update_undo(
        &mut self,
        table_name: &str,
        proposals: &[(u64, Vec<Value>)],
        snapshot: bool,
    ) -> Result<CapturedUndo, DbError> {
        let _undo = crate::storage::Timer::new(|profile| &mut profile.undo);
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| DbError::TableNotFound(table_name.to_string()))?;
        let heap_rows = table.rows().len();
        let width = table.schema().len();
        if snapshot {
            return Ok(CapturedUndo {
                entries: 0,
                snapshot: true,
                heap_rows,
                width,
            });
        }
        if self.update_undo.is_some() {
            return Err(DbError::Internal(
                "undo journal already holds original rows".to_string(),
            ));
        }
        let mut saved = Vec::new();
        saved.try_reserve(proposals.len()).map_err(|_| {
            DbError::Internal("undo journal could not reserve its entries".to_string())
        })?;
        let mut seen = HashSet::new();
        seen.try_reserve(proposals.len()).map_err(|_| {
            DbError::Internal("undo journal could not reserve its entries".to_string())
        })?;
        for (row_id, _) in proposals {
            if !seen.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "update lists row {row_id} twice in '{table_name}'"
                )));
            }
            let Some(position) = table.position(*row_id) else {
                return Err(DbError::Internal(format!(
                    "update target {row_id} is not in '{table_name}'"
                )));
            };
            let Some(image) = table.rows().get(position) else {
                return Err(DbError::Internal(format!(
                    "update target {row_id} is not in '{table_name}'"
                )));
            };
            saved.push((*row_id, image.clone()));
        }
        self.update_undo = Some(UpdateJournal {
            table: table_name.to_string(),
            rows: saved,
        });
        Ok(CapturedUndo {
            entries: proposals.len(),
            snapshot: false,
            heap_rows,
            width,
        })
    }

    pub(crate) fn clear_update_undo(&mut self) {
        self.update_undo = None;
    }

    /// Save the current images and positions of `targets` before compaction.
    ///
    /// `snapshot` skips the journal because the caller already cloned the
    /// table. `targets` are stable row ids. Duplicate and missing ids return
    /// before the journal is stored and before any row is removed. Reserving
    /// the entry list is a query error. Cloning a row uses the process
    /// allocator; an out-of-memory abort there is not a query error, and it
    /// still happens before the first removal. `next_row_id` is stored and not
    /// changed.
    pub(crate) fn capture_delete_undo(
        &mut self,
        table_name: &str,
        targets: &[u64],
        snapshot: bool,
    ) -> Result<CapturedUndo, DbError> {
        let _undo = crate::storage::Timer::new(|profile| &mut profile.undo);
        let table = self
            .tables
            .get(table_name)
            .ok_or_else(|| DbError::TableNotFound(table_name.to_string()))?;
        let heap_rows = table.rows().len();
        let width = table.schema().len();
        let next_row_id = table.next_row_id();
        if snapshot {
            return Ok(CapturedUndo {
                entries: 0,
                snapshot: true,
                heap_rows,
                width,
            });
        }
        if self.delete_undo.is_some() {
            return Err(DbError::Internal(
                "undo journal already holds original rows".to_string(),
            ));
        }
        let mut saved = Vec::new();
        saved.try_reserve(targets.len()).map_err(|_| {
            DbError::Internal("undo journal could not reserve its entries".to_string())
        })?;
        let mut seen = HashSet::new();
        seen.try_reserve(targets.len()).map_err(|_| {
            DbError::Internal("undo journal could not reserve its entries".to_string())
        })?;
        let mut previous = None;
        for row_id in targets {
            if !seen.insert(*row_id) {
                return Err(DbError::Internal(format!(
                    "delete lists row {row_id} twice in '{table_name}'"
                )));
            }
            let Some(position) = table.position(*row_id) else {
                return Err(DbError::Internal(format!(
                    "delete target {row_id} is not in '{table_name}'"
                )));
            };
            if previous.is_some_and(|seen: usize| position <= seen) {
                return Err(DbError::Internal(format!(
                    "delete target {row_id} is out of order in '{table_name}'"
                )));
            }
            previous = Some(position);
            let Some(image) = table.rows().get(position) else {
                return Err(DbError::Internal(format!(
                    "delete target {row_id} is not in '{table_name}'"
                )));
            };
            saved.push(DeletedRow {
                position,
                row_id: *row_id,
                values: image.clone(),
            });
        }
        self.delete_undo = Some(DeleteJournal {
            table: table_name.to_string(),
            rows: saved,
            heap_rows,
            next_row_id,
            applied: false,
        });
        Ok(CapturedUndo {
            entries: targets.len(),
            snapshot: false,
            heap_rows,
            width,
        })
    }

    /// Mark the delete journal as the compaction that restore must invert.
    ///
    /// A snapshot path has no journal. Calling this is then a no-op.
    pub(crate) fn mark_delete_undo_applied(&mut self) -> Result<(), DbError> {
        let Some(journal) = self.delete_undo.as_mut() else {
            return Ok(());
        };
        journal.applied = true;
        Ok(())
    }

    pub(crate) fn clear_delete_undo(&mut self) {
        self.delete_undo = None;
    }

    /// Restore a journal captured before install.
    ///
    /// `required` is for a publish failure: the journal must exist, including
    /// an empty one. An apply error that happened before capture passes
    /// `false`. A failure here fences a stored database and does not report a
    /// confirmed rollback.
    pub(crate) fn restore_update_journal(&mut self, required: bool) -> Result<(), DbError> {
        if self.update_undo.is_none() {
            if required {
                return Err(self.fence_uncertain("the journal is missing"));
            }
            return Ok(());
        }
        #[cfg(test)]
        if fail_undo_restore() {
            return Err(self.fence_uncertain("restore was forced to fail"));
        }
        let check = match self.update_undo.as_ref() {
            None => Err("the journal is missing".to_string()),
            Some(journal) => match self.tables.get(&journal.table) {
                None => Err("the table is missing".to_string()),
                Some(table) => table
                    .check_restore(&journal.rows)
                    .map_err(|error| error.to_string()),
            },
        };
        if let Err(detail) = check {
            return Err(self.fence_uncertain(&detail));
        }
        let Some(journal) = self.update_undo.take() else {
            return Err(self.fence_uncertain("the journal is missing"));
        };
        let Some(table) = self.tables.get_mut(&journal.table) else {
            return Err(self.fence_uncertain("the table is missing"));
        };
        if let Err(error) = table.restore_images(journal.rows) {
            return Err(self.fence_uncertain(&error.to_string()));
        }
        Ok(())
    }

    /// Restore a delete journal captured before compaction.
    ///
    /// `required` is for a publish failure: the journal must exist and
    /// compaction must have run, including a delete that removed nothing. An
    /// apply error before compaction passes `false` and drops the unused
    /// journal. The restored vectors replace the table only after every slot
    /// is filled. A failure before that puts the journal back, leaves the
    /// compacted rows in place, fences a stored database, and does not report
    /// a confirmed rollback.
    pub(crate) fn restore_delete_journal(&mut self, required: bool) -> Result<(), DbError> {
        let applied = match self.delete_undo.as_ref() {
            None => {
                if required {
                    return Err(self.fence_uncertain("the journal is missing"));
                }
                return Ok(());
            }
            Some(journal) => journal.applied,
        };
        if !applied {
            if required {
                return Err(self.fence_uncertain("the journal was not applied"));
            }
            self.delete_undo = None;
            return Ok(());
        }
        #[cfg(test)]
        if fail_undo_restore() {
            return Err(self.fence_uncertain("restore was forced to fail"));
        }
        let check = match self.delete_undo.as_ref() {
            None => Err("the journal is missing".to_string()),
            Some(journal) => match self.tables.get(&journal.table) {
                None => Err("the table is missing".to_string()),
                Some(table) => {
                    if table.next_row_id() != journal.next_row_id {
                        Err("the row-id counter changed".to_string())
                    } else if table.rows().len() + journal.rows.len() != journal.heap_rows {
                        Err("the table length changed".to_string())
                    } else {
                        let pairs: Vec<(usize, u64)> = journal
                            .rows
                            .iter()
                            .map(|row| (row.position, row.row_id))
                            .collect();
                        table
                            .check_delete_restore(&pairs)
                            .map_err(|error| error.to_string())
                    }
                }
            },
        };
        if let Err(detail) = check {
            return Err(self.fence_uncertain(&detail));
        }
        let Some(mut journal) = self.delete_undo.take() else {
            return Err(self.fence_uncertain("the journal is missing"));
        };
        let mut rows = Vec::new();
        if rows.try_reserve(journal.rows.len()).is_err() {
            self.delete_undo = Some(journal);
            return Err(self.fence_uncertain("undo journal could not reserve its entries"));
        }
        for row in journal.rows.drain(..) {
            rows.push((row.position, row.row_id, row.values));
        }
        let table_name = journal.table.clone();
        let Some(table) = self.tables.get_mut(&table_name) else {
            journal.rows = rows
                .into_iter()
                .map(|(position, row_id, values)| DeletedRow {
                    position,
                    row_id,
                    values,
                })
                .collect();
            self.delete_undo = Some(journal);
            return Err(self.fence_uncertain("the table is missing"));
        };
        if let Err(error) = table.restore_deleted(&mut rows) {
            if !rows.is_empty() {
                journal.rows = rows
                    .into_iter()
                    .map(|(position, row_id, values)| DeletedRow {
                        position,
                        row_id,
                        values,
                    })
                    .collect();
                self.delete_undo = Some(journal);
            }
            return Err(self.fence_uncertain(&error.to_string()));
        }
        Ok(())
    }

    fn fence_uncertain(&mut self, detail: &str) -> DbError {
        let error = DbError::Storage(format!(
            "undo could not restore the statement ({detail}); the outcome is unknown and this is not a confirmed rollback. Reopen the file before running another statement."
        ));
        if let Some(store) = self.store.as_mut() {
            store.fence_with(error.clone());
        }
        error
    }

    pub(crate) fn rollback(&mut self, undo: Undo) -> Result<(), DbError> {
        self.selective = None;
        match undo {
            Undo::None => {
                self.update_undo = None;
                self.delete_undo = None;
                Ok(())
            }
            Undo::Selective => {
                self.delete_undo = None;
                self.restore_update_journal(true)
            }
            Undo::Delete => {
                self.update_undo = None;
                self.restore_delete_journal(true)
            }
            Undo::Remove { table, indexes } => {
                self.update_undo = None;
                self.delete_undo = None;
                self.tables.remove(&table);
                self.indexes = indexes;
                Ok(())
            }
            Undo::Restore { tables, indexes } => {
                self.update_undo = None;
                self.delete_undo = None;
                for table in tables {
                    self.tables.insert(table.name().to_string(), table);
                }
                self.indexes = indexes;
                Ok(())
            }
            Undo::Replace { table, indexes } => {
                self.update_undo = None;
                self.delete_undo = None;
                self.tables.insert(table.name().to_string(), table);
                self.indexes = indexes;
                Ok(())
            }
            Undo::Indexes(indexes) => {
                self.update_undo = None;
                self.delete_undo = None;
                self.indexes = indexes;
                Ok(())
            }
        }
    }

    /// Remember the rows an `UPDATE` changed so publish can skip the others.
    ///
    /// The ids are stable and were fixed before any assignment. An empty list
    /// is a match of nothing: publish still audits memory and writes nothing.
    pub(crate) fn stage_selective_update(&mut self, table: &str, row_ids: Vec<u64>) {
        self.selective = Some((table.to_string(), row_ids));
    }

    /// Remember ids to unlink. This does not take the undo journal.
    ///
    /// `row_ids` is a copy of the ids captured before compaction. The journal
    /// still owns the removed images and their positions. An empty list is a
    /// real no-match, not a skipped publish.
    pub(crate) fn stage_selective_delete(&mut self, table: &str, row_ids: Vec<u64>) {
        self.delete_targets = Some((table.to_string(), row_ids));
    }

    pub(crate) fn clear_selective(&mut self) {
        self.selective = None;
        self.delete_targets = None;
    }

    pub(crate) fn publish(&mut self) -> Result<(), StepError> {
        let _publish = crate::storage::Timer::new(|p| &mut p.publish_total);
        let selective = self.selective.take();
        let deleted = self.delete_targets.take();
        if self.store.is_none() {
            return Ok(());
        }
        if let Some((name, row_ids)) = selective
            && self.selective_identity_matches(&name)
        {
            for table in self.tables.values() {
                table.audit().map_err(StepError::BeforeCommit)?;
            }
            let changes = self.selective_changes(&name, &row_ids)?;
            let Some(store) = self.store.as_mut() else {
                return Err(Self::missing_store());
            };
            return store.publish_row_changes(&name, &changes);
        }
        if let Some((name, deleted_ids)) = &deleted
            && self.delete_targets_match(name, deleted_ids)
        {
            return self.publish_deleted(name, deleted_ids);
        }
        let prepare = crate::storage::publish_timer();
        let images: Vec<TableImage> = {
            let _build = crate::storage::Timer::new(|p| &mut p.image_build);
            self.tables.values().map(Table::image).collect()
        };
        crate::storage::add_publish_prepare(prepare);
        #[cfg(test)]
        let images = crate::storage::ProfiledDrop::new(images, |p| &mut p.image_drop);
        #[cfg(test)]
        note_publish_images(images.len());
        let Some(store) = self.store.as_mut() else {
            return Err(Self::missing_store());
        };
        store.publish_indexes(&images, &self.indexes)
    }

    /// Audit memory, then unlink `deleted` without cloning survivors.
    ///
    /// Tables whose stored ids still match memory are decoded and compared.
    /// The table being unlinked is not one of them, so its survivors are not
    /// read. A no-match has an empty `deleted` list and does compare that
    /// table. A stored row that already matches does not dirty a page.
    fn publish_deleted(&mut self, name: &str, deleted: &[u64]) -> Result<(), StepError> {
        let prepare = crate::storage::publish_timer();
        for table in self.tables.values() {
            table.audit_for_publish().map_err(StepError::BeforeCommit)?;
        }
        crate::storage::add_publish_prepare(prepare);
        let Database {
            tables,
            store,
            indexes,
            ..
        } = self;
        let Some(store) = store.as_mut() else {
            return Err(Self::missing_store());
        };
        let mut scans = Vec::new();
        for (other, table) in tables.iter() {
            if other.as_str() == name && !deleted.is_empty() {
                continue;
            }
            scans.push(crate::storage::MemoryRows {
                name: other.as_str(),
                rows: table.rows(),
            });
        }
        store.publish_deletes(name, deleted, &scans, indexes)
    }

    /// True when unlinking `deleted` from the stored ids leaves this table.
    ///
    /// The comparison uses ids only. It does not clone row values. A mismatch
    /// falls back to the full image, which is the previous publication path.
    fn delete_targets_match(&self, name: &str, deleted: &[u64]) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        let Some(table) = self.tables.get(name) else {
            return false;
        };
        let Some(stored_ids) = store.stored_row_ids(name) else {
            return false;
        };
        let Some(stored_next) = store.stored_next_row_id(name) else {
            return false;
        };
        if stored_next != table.next_row_id() {
            return false;
        }
        for (other, other_table) in &self.tables {
            if other.as_str() == name {
                continue;
            }
            if !store.matches_row_identity(other, other_table.row_ids(), other_table.next_row_id())
            {
                return false;
            }
        }
        let mut pending = HashSet::with_capacity(deleted.len());
        for id in deleted {
            if *id == 0 || !pending.insert(*id) {
                return false;
            }
        }
        let memory = table.row_ids();
        let mut memory_index = 0;
        for id in stored_ids {
            if pending.remove(id) {
                continue;
            }
            if memory.get(memory_index) != Some(id) {
                return false;
            }
            memory_index += 1;
        }
        pending.is_empty() && memory_index == memory.len()
    }

    fn missing_store() -> StepError {
        StepError::BeforeCommit(DbError::Internal(
            "database file disappeared before publish".to_string(),
        ))
    }

    fn selective_identity_matches(&self, name: &str) -> bool {
        let Some(store) = self.store.as_ref() else {
            return false;
        };
        let Some(table) = self.tables.get(name) else {
            return false;
        };
        store.matches_row_identity(name, table.row_ids(), table.next_row_id())
    }

    fn selective_changes(
        &self,
        name: &str,
        row_ids: &[u64],
    ) -> Result<Vec<(u64, Vec<Value>)>, StepError> {
        let Some(table) = self.tables.get(name) else {
            return Err(StepError::BeforeCommit(DbError::Internal(format!(
                "updated table '{name}' is not in the catalog"
            ))));
        };
        let mut changes = Vec::with_capacity(row_ids.len());
        for row_id in row_ids {
            let Some(position) = table.position(*row_id) else {
                return Err(StepError::BeforeCommit(DbError::Internal(format!(
                    "updated row {row_id} is not in '{name}'"
                ))));
            };
            let Some(values) = table.rows().get(position) else {
                return Err(StepError::BeforeCommit(DbError::Internal(format!(
                    "updated row {row_id} is not in '{name}'"
                ))));
            };
            changes.push((*row_id, values.clone()));
        }
        Ok(changes)
    }

    pub(crate) fn create_table(
        &mut self,
        name: String,
        columns: Vec<Column>,
    ) -> Result<(), DbError> {
        let table = Table::new(name.clone(), columns)?;
        if self.tables.contains_key(table.name()) {
            return Err(DbError::TableAlreadyExists(name));
        }
        self.tables.insert(name, table);
        Ok(())
    }

    pub(crate) fn drop_tables(&mut self, names: &[String]) -> Result<(), DbError> {
        for name in names {
            if !self.tables.contains_key(name) {
                return Err(DbError::TableNotFound(name.clone()));
            }
        }
        for name in names {
            self.tables.remove(name);
        }
        self.indexes
            .retain(|index| !names.iter().any(|name| name == &index.table));
        Ok(())
    }

    pub(crate) fn create_index(
        &mut self,
        name: String,
        table: String,
        column: String,
    ) -> Result<(), DbError> {
        let Some(store) = self.store.as_ref() else {
            return Err(DbError::unsupported(
                "CREATE INDEX requires a database file",
            ));
        };
        if store.format_version() < 3 {
            return Err(DbError::Storage(format!(
                "database format version 2 requires an explicit migration: rustdb --migrate {}",
                store.path().display()
            )));
        }
        if self.indexes.iter().any(|index| index.name == name) {
            return Err(DbError::invalid(format!("index '{name}' already exists")));
        }
        let schema = self.table(&table)?.schema();
        let Some(position) = schema.index_of(&column) else {
            return Err(DbError::ColumnNotFound { table, column });
        };
        let Some(definition) = schema.column(position) else {
            return Err(DbError::ColumnNotFound { table, column });
        };
        if definition.data_type() != crate::value::DataType::Integer {
            return Err(DbError::unsupported(format!(
                "index column '{column}' must be INTEGER"
            )));
        }
        self.indexes.push(IndexImage {
            name,
            table,
            column,
        });
        Ok(())
    }

    pub(crate) fn drop_indexes(&mut self, names: &[String]) -> Result<(), DbError> {
        if self.store.is_none() {
            return Err(DbError::unsupported("DROP INDEX requires a database file"));
        }
        for name in names {
            if !self.indexes.iter().any(|index| index.name == *name) {
                return Err(DbError::invalid(format!("index '{name}' does not exist")));
            }
        }
        self.indexes
            .retain(|index| !names.iter().any(|name| name == &index.name));
        Ok(())
    }

    pub(crate) fn index_images(&self) -> &[IndexImage] {
        &self.indexes
    }

    /// Read one index. `equal` selects one key. Otherwise the inclusive range
    /// `low..=high` is read, and a missing side is unbounded.
    pub(crate) fn read_indexed_keys(
        &mut self,
        index: &str,
        equal: Option<i64>,
        low: Option<i64>,
        high: Option<i64>,
    ) -> Result<(u32, Vec<(i64, u64)>), DbError> {
        let store = self
            .store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?;
        let root = store.index_root(index)?;
        let entries = if let Some(key) = equal {
            store.read_index_equal(root, key)?
        } else {
            store.read_index_range(root, low, high)?
        };
        Ok((root, entries))
    }

    /// Height and reachable page count of one stored index.
    #[cfg(test)]
    pub(crate) fn index_shape(&mut self, index: &str) -> Result<(u32, usize), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .index_shape(index)
    }

    /// Minimum and maximum keys stored in `index`, or `None` when the tree is empty.
    pub(crate) fn index_key_span(&mut self, index: &str) -> Result<Option<(i64, i64)>, DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .index_key_span(index)
    }

    pub(crate) fn table(&self, name: &str) -> Result<&Table, DbError> {
        self.tables
            .get(name)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))
    }

    pub(crate) fn table_mut(&mut self, name: &str) -> Result<&mut Table, DbError> {
        self.tables
            .get_mut(name)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))
    }

    #[cfg(test)]
    pub(crate) fn patch_root_leaf_entry(
        &mut self,
        name: &str,
        slot: usize,
        key: Option<i64>,
        row_id: Option<u64>,
    ) -> Result<(), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .patch_root_leaf_entry(name, slot, key, row_id)
    }

    #[cfg(test)]
    pub(crate) fn drop_root_leaf_entry(&mut self, name: &str, slot: usize) -> Result<(), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .drop_root_leaf_entry(name, slot)
    }

    #[cfg(test)]
    pub(crate) fn corrupt_index_root_type(&mut self, name: &str) -> Result<(), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .corrupt_index_root_type(name)
    }

    #[cfg(test)]
    pub(crate) fn zero_leftmost_leaf_right(&mut self, name: &str) -> Result<(), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .zero_leftmost_leaf_right(name)
    }

    #[cfg(test)]
    pub(crate) fn set_inject(&mut self, inject: crate::storage::Inject) {
        self.store
            .as_mut()
            .expect("disk database")
            .set_inject(inject);
    }

    #[cfg(test)]
    pub(crate) fn clear_page_cache(&mut self) {
        if let Some(store) = self.store.as_mut() {
            store.clear_page_cache();
        }
    }
}

fn note_table_snapshot(elapsed_us: u128) {
    #[cfg(test)]
    {
        TABLE_SNAPSHOTS.with(|count| count.set(count.get().saturating_add(1)));
        SNAPSHOT_US.with(|slot| slot.set(elapsed_us));
    }
    #[cfg(not(test))]
    let _ = elapsed_us;
}

#[cfg(test)]
fn fail_undo_restore() -> bool {
    FAIL_UNDO_RESTORE.with(std::cell::Cell::get)
}

#[cfg(test)]
fn note_publish_images(count: usize) {
    let count = u64::try_from(count).unwrap_or(u64::MAX);
    PUBLISH_IMAGES.with(|slot| slot.set(slot.get().saturating_add(count)));
}

#[cfg(test)]
thread_local! {
    static TABLE_SNAPSHOTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SNAPSHOT_US: std::cell::Cell<u128> = const { std::cell::Cell::new(0) };
    static FAIL_UNDO_RESTORE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PUBLISH_IMAGES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts table clones taken for undo.
///
/// `INSERT` still clones. `UPDATE` and `DELETE` clone only when a test forces
/// the full snapshot.
#[cfg(test)]
pub(crate) fn reset_table_snapshots() {
    TABLE_SNAPSHOTS.with(|count| count.set(0));
    SNAPSHOT_US.with(|slot| slot.set(0));
}

/// Tables cloned for undo since the last [`reset_table_snapshots`].
#[cfg(test)]
pub(crate) fn table_snapshots() -> u64 {
    TABLE_SNAPSHOTS.with(std::cell::Cell::get)
}

/// Microseconds spent cloning the last undo snapshot in this thread.
#[cfg(test)]
pub(crate) fn snapshot_us() -> u128 {
    SNAPSHOT_US.with(std::cell::Cell::get)
}

/// Clears the count of table images built for publish.
#[cfg(test)]
pub(crate) fn reset_publish_images() {
    PUBLISH_IMAGES.with(|slot| slot.set(0));
}

/// Table images built by the full publish path since the last reset.
///
/// A selective `DELETE` leaves this at zero. The full-image guard increments
/// it by the number of tables.
#[cfg(test)]
pub(crate) fn publish_images() -> u64 {
    PUBLISH_IMAGES.with(std::cell::Cell::get)
}

/// Makes the next undo restore fence the process instead of writing images.
///
/// The journal is left unused. The statement error says the outcome is not a
/// confirmed rollback.
#[cfg(test)]
pub(crate) struct FailUndoRestore;

#[cfg(test)]
impl FailUndoRestore {
    pub(crate) fn new() -> Self {
        FAIL_UNDO_RESTORE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for FailUndoRestore {
    fn drop(&mut self) {
        FAIL_UNDO_RESTORE.with(|flag| flag.set(false));
    }
}

impl Default for Database {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatementResult;
    use crate::storage::Inject;
    use crate::value::Value;

    fn scratch(label: &str) -> (crate::test_dir::TestDir, std::path::PathBuf) {
        let dir = crate::test_dir::TestDir::new(label);
        let path = dir.join("db");
        (dir, path)
    }

    #[test]
    fn a_failed_checkpoint_blocks_every_statement_until_reopen() {
        let (dir, path) = scratch("fence");
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .store
            .as_mut()
            .expect("disk database")
            .set_inject(Inject::AfterLogSync);
        let error = database
            .execute("INSERT INTO users VALUES (1, 'Ada');")
            .unwrap_err();
        assert!(
            error.to_string().contains("checkpoint did not finish"),
            "{error}"
        );
        let error = database.execute("SELECT name FROM users;").unwrap_err();
        assert!(
            error.to_string().contains("checkpoint did not finish"),
            "{error}"
        );
        drop(database);

        let mut database = Database::open(&path).unwrap();
        let results = database.execute("SELECT name FROM users;").unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        assert_eq!(query.rows, vec![vec![Value::Text("Ada".to_string())]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ambiguous_commit_sync_is_not_reported_as_a_rollback() {
        let (dir, path) = scratch("ambiguous");
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .store
            .as_mut()
            .expect("disk database")
            .set_inject(Inject::CommitSyncFailed);
        let error = database
            .execute("INSERT INTO users VALUES (1, 'Ada');")
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("not a confirmed rollback"), "{message}");
        assert!(message.contains("outcome is unknown"), "{message}");
        assert!(
            !message.contains("rolled back"),
            "must not claim a rollback: {message}"
        );
        let mut wal = path.clone().into_os_string();
        wal.push(".wal");
        assert!(std::fs::metadata(&wal).unwrap().len() > 0);
        drop(database);

        let mut database = Database::open(&path).unwrap();
        let results = database.execute("SELECT name FROM users;").unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        assert_eq!(query.rows, vec![vec![Value::Text("Ada".to_string())]]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sql_recovery_stress_replays_dml_and_holds_the_lock() {
        fn column(database: &mut Database, sql: &str) -> Vec<String> {
            let results = database.execute(sql).unwrap();
            let StatementResult::Query(query) = &results[0] else {
                panic!("expected a query");
            };
            query
                .rows
                .iter()
                .map(|row| match &row[0] {
                    Value::Text(text) => text.clone(),
                    other => panic!("unexpected cell {other:?}"),
                })
                .collect()
        }
        fn expect_rows(path: &std::path::Path, names: &[&str], indexed_id: i64, indexed: &[&str]) {
            for _ in 0..3 {
                let mut database = Database::open(path).unwrap();
                let got = column(&mut database, "SELECT name FROM users;");
                assert_eq!(got, names);
                let sql = format!("SELECT name FROM users WHERE id = {indexed_id};");
                assert_eq!(column(&mut database, &sql), indexed);
                let explained = database
                    .execute("EXPLAIN SELECT name FROM users WHERE id = 1;")
                    .unwrap();
                let StatementResult::Query(plan) = &explained[0] else {
                    panic!("expected EXPLAIN");
                };
                let text = plan
                    .rows
                    .iter()
                    .flat_map(|row| row.iter())
                    .any(|cell| matches!(cell, Value::Text(text) if text == "IndexScan"));
                assert!(text, "index scan missing from {plan:?}");
                drop(database);
            }
            let mut wal = path.as_os_str().to_owned();
            wal.push(".wal");
            assert_eq!(std::fs::metadata(wal).unwrap().len(), 0);
        }

        let (dir, path) = scratch("m17-stress");
        {
            let mut database = Database::open(&path).unwrap();
            database
                .execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            database
                .execute("INSERT INTO users VALUES (1, 'Ada');")
                .unwrap();
        }
        Database::migrate(&path).unwrap();
        {
            let mut database = Database::open(&path).unwrap();
            database
                .execute("CREATE INDEX idx_users_id ON users(id);")
                .unwrap();
        }

        let additions = ["Bea", "Cy", "Dee", "Eve"];
        let mut names = vec!["Ada"];
        for (step, name) in additions.iter().enumerate() {
            {
                let mut database = Database::open(&path).unwrap();
                database
                    .store
                    .as_mut()
                    .expect("disk database")
                    .set_inject(Inject::DuringCheckpoint((step as u32) % 2));
                let error = database
                    .execute(&format!(
                        "INSERT INTO users VALUES ({}, '{name}');",
                        step + 2
                    ))
                    .unwrap_err();
                assert!(
                    error.to_string().contains("checkpoint did not finish"),
                    "{error}"
                );
                let blocked = database.execute("SELECT name FROM users;").unwrap_err();
                assert!(
                    blocked.to_string().contains("checkpoint did not finish"),
                    "{blocked}"
                );
            }
            names.push(name);
            expect_rows(&path, &names, 1, &["Ada"]);
        }

        {
            let mut database = Database::open(&path).unwrap();
            database
                .store
                .as_mut()
                .expect("disk database")
                .set_inject(Inject::CommitSyncFailed);
            let error = database
                .execute("UPDATE users SET name = 'Ann' WHERE id = 1;")
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("durability is unknown"), "{message}");
            assert!(message.contains("not a confirmed rollback"), "{message}");
            assert!(!message.contains("rolled back"), "{message}");
        }
        names[0] = "Ann";
        expect_rows(&path, &names, 1, &["Ann"]);

        {
            let mut database = Database::open(&path).unwrap();
            database
                .store
                .as_mut()
                .expect("disk database")
                .set_inject(Inject::AfterLogSync);
            let error = database
                .execute("DELETE FROM users WHERE id = 5;")
                .unwrap_err();
            assert!(
                error.to_string().contains("checkpoint did not finish"),
                "{error}"
            );
        }
        names.pop();
        expect_rows(&path, &names, 2, &["Bea"]);

        {
            let mut database = Database::open(&path).unwrap();
            database
                .execute("INSERT INTO users VALUES (1, 'Twin');")
                .unwrap();
            database
                .store
                .as_mut()
                .expect("disk database")
                .set_inject(Inject::DuringCheckpoint(1));
            let error = database
                .execute("INSERT INTO users VALUES (6, 'Fay');")
                .unwrap_err();
            assert!(
                error.to_string().contains("checkpoint did not finish"),
                "{error}"
            );
        }
        names.push("Twin");
        names.push("Fay");
        expect_rows(&path, &names, 1, &["Ann", "Twin"]);

        {
            let mut database = Database::open(&path).unwrap();
            let error = match Database::open(&path) {
                Ok(_) => panic!("second open should fail while recovery holds the lock"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("is locked"), "{error}");
            database
                .execute("INSERT INTO users VALUES (7, 'Gus');")
                .unwrap();
            drop(database);
        }
        names.push("Gus");
        expect_rows(&path, &names, 7, &["Gus"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn aged_index(label: &str) -> (crate::test_dir::TestDir, Database) {
        let (dir, path) = scratch(label);
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, age INT, name TEXT);")
            .unwrap();
        database
            .execute("INSERT INTO users VALUES (1, 25, 'Ada'), (2, 25, 'Bea'), (3, NULL, 'Cy');")
            .unwrap();
        drop(database);
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_users_age ON users(age);")
            .unwrap();
        let results = database
            .execute("SELECT name FROM users WHERE age = 25;")
            .unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        assert_eq!(
            query.rows,
            vec![
                vec![Value::Text("Ada".to_string())],
                vec![Value::Text("Bea".to_string())],
            ]
        );
        (dir, database)
    }

    #[test]
    fn an_index_entry_that_disagrees_with_the_row_is_an_error() {
        let (dir, mut database) = aged_index("mismatch");
        database
            .table_mut("users")
            .unwrap()
            .overwrite_cell_for_test(0, 1, Value::Integer(99));
        let error = database
            .execute("SELECT name FROM users WHERE age = 25;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_row_id_from_an_index_is_an_error() {
        let (dir, mut database) = aged_index("missing-row");
        database
            .table_mut("users")
            .unwrap()
            .forget_position_for_test(1);
        let error = database
            .execute("SELECT name FROM users WHERE age = 25;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_index_page_fails_the_select() {
        let (dir, mut database) = aged_index("bad-page");
        database.corrupt_index_root_type("idx_users_age").unwrap();
        let error = database
            .execute("SELECT name FROM users WHERE age = 25;")
            .unwrap_err();
        assert!(error.to_string().contains("is corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_duplicated_index_row_id_is_an_error() {
        let (dir, mut database) = aged_index("duplicate-row");
        database
            .execute("INSERT INTO users VALUES (4, 40, 'Dee');")
            .unwrap();
        database
            .patch_root_leaf_entry("idx_users_age", 1, Some(26), Some(1))
            .unwrap();
        let error = database
            .execute("SELECT name FROM users WHERE age >= 25 AND age <= 26;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_index_key_that_disagrees_with_the_heap_is_an_error() {
        let (dir, mut database) = aged_index("bad-key");
        database
            .patch_root_leaf_entry("idx_users_age", 0, Some(26), None)
            .unwrap();
        database
            .patch_root_leaf_entry("idx_users_age", 1, Some(27), None)
            .unwrap();
        let error = database
            .execute("SELECT name FROM users WHERE age = 26;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsorted_leaf_is_an_error_instead_of_a_partial_result() {
        let (dir, mut database) = aged_index("unsorted");
        database
            .patch_root_leaf_entry("idx_users_age", 0, Some(30), None)
            .unwrap();
        let error = database
            .execute("SELECT name FROM users WHERE age = 25;")
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn joined_index(label: &str) -> (crate::test_dir::TestDir, Database) {
        let (dir, path) = scratch(&format!("join-{label}"));
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (id INT, user_id INT, total INT);")
            .unwrap();
        database
            .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
            .unwrap();
        database
            .execute("INSERT INTO orders VALUES (10, 1, 10), (11, 1, 30), (12, 2, 5);")
            .unwrap();
        drop(database);
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
            .unwrap();
        let results = database
            .execute(
                "SELECT users.name, orders.total
                 FROM users JOIN orders ON users.id = orders.user_id;",
            )
            .unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        assert_eq!(
            query.rows,
            vec![
                vec![Value::Text("Ada".to_string()), Value::Integer(10)],
                vec![Value::Text("Ada".to_string()), Value::Integer(30)],
                vec![Value::Text("Bea".to_string()), Value::Integer(5)],
            ]
        );
        (dir, database)
    }

    #[test]
    fn a_join_probe_rejects_a_missing_row_id() {
        let (dir, mut database) = joined_index("missing-row");
        database
            .table_mut("orders")
            .unwrap()
            .forget_position_for_test(1);
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_a_stale_row_position() {
        let (dir, mut database) = joined_index("stale-position");
        database
            .table_mut("orders")
            .unwrap()
            .move_position_for_test(1, 2);
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_an_indexed_value_that_disagrees_with_the_heap() {
        let (dir, mut database) = joined_index("bad-key");
        database
            .table_mut("orders")
            .unwrap()
            .overwrite_cell_for_test(0, 1, Value::Integer(99));
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("does not match"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_a_duplicated_index_row_id() {
        let (dir, mut database) = joined_index("duplicate-row");
        database
            .patch_root_leaf_entry("idx_orders_user", 2, Some(1), Some(1))
            .unwrap();
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Heap rows are schema-valid, so the index probe still runs. The nested
    /// loop would return the matching names. A corrupt leaf must not be
    /// replaced by that success.
    #[test]
    fn a_schema_valid_corrupt_index_is_not_replaced_by_the_nested_loop() {
        let (dir, mut database) = joined_index("m21-keep-corrupt");
        database
            .patch_root_leaf_entry("idx_orders_user", 0, Some(30), None)
            .unwrap();
        let nested = {
            let _force = crate::executor::ForceNestedJoin::new();
            database
                .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
                .unwrap()
        };
        let crate::StatementResult::Query(nested) = &nested[0] else {
            panic!("expected a query");
        };
        assert_eq!(nested.rows.len(), 3, "{:?}", nested.rows);
        let error = {
            let _force = crate::executor::ForceIndexJoin::new();
            database
                .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
                .unwrap_err()
        };
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_a_corrupt_index_page() {
        let (dir, mut database) = joined_index("bad-page");
        database.corrupt_index_root_type("idx_orders_user").unwrap();
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_an_unsorted_leaf_instead_of_skipping_a_match() {
        let (dir, mut database) = joined_index("unsorted-join");
        database
            .patch_root_leaf_entry("idx_orders_user", 0, Some(30), None)
            .unwrap();
        let _force = crate::executor::ForceIndexJoin::new();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_join_probe_rejects_a_broken_sibling_instead_of_a_short_result() {
        let (dir, mut database) = joined_index("broken-sibling");
        for id in 13..=310 {
            database
                .execute(&format!("INSERT INTO orders VALUES ({id}, 1, 1);"))
                .unwrap();
        }
        let results = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        assert_eq!(query.rows.len(), 301);
        database
            .zero_leftmost_leaf_right("idx_orders_user")
            .unwrap();
        let error = database
            .execute("SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;")
            .unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "release profile of index maintenance"]
    fn profile_index_maintenance() {
        for size in [100_usize, 1_000] {
            let plain = index_profile(size, false);
            let indexed = index_profile(size, true);
            println!(
                "size={size} build_ms={:.1} plain_insert_us={} indexed_insert_us={} plain_update_us={} indexed_update_us={} plain_delete_us={} indexed_delete_us={} plain_bytes={} indexed_bytes={} select_rows={}",
                indexed.build.as_secs_f64() * 1_000.0,
                plain.insert.as_micros(),
                indexed.insert.as_micros(),
                plain.update.as_micros(),
                indexed.update.as_micros(),
                plain.delete.as_micros(),
                indexed.delete.as_micros(),
                plain.bytes,
                indexed.bytes,
                indexed.selected
            );
        }
    }

    struct IndexProfile {
        build: std::time::Duration,
        insert: std::time::Duration,
        update: std::time::Duration,
        delete: std::time::Duration,
        bytes: u64,
        selected: usize,
    }

    fn index_profile(size: usize, with_index: bool) -> IndexProfile {
        let (dir, path) = scratch(&format!("index-profile-{size}-{}", with_index as u8));
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE bench (id INT, age INT, email TEXT, note TEXT, active BOOLEAN);")
            .unwrap();
        for id in 1..=size {
            database
                .execute(&format!(
                    "INSERT INTO bench VALUES ({id}, {age}, 'user{id}@example.com', 'note', TRUE);",
                    age = id % 80
                ))
                .unwrap();
        }
        drop(database);
        if with_index {
            Database::migrate(&path).unwrap();
        }
        let mut database = Database::open(&path).unwrap();
        let build_started = std::time::Instant::now();
        if with_index {
            database
                .execute("CREATE INDEX idx_bench_age ON bench(age);")
                .unwrap();
        }
        let build = build_started.elapsed();
        let insert = std::time::Instant::now();
        database
            .execute(&format!(
                "INSERT INTO bench VALUES ({}, 3, 'extra@example.com', 'note', FALSE);",
                size + 1
            ))
            .unwrap();
        let insert = insert.elapsed();
        let update = std::time::Instant::now();
        database
            .execute("UPDATE bench SET age = 42 WHERE id = 1;")
            .unwrap();
        let update = update.elapsed();
        let delete = std::time::Instant::now();
        database.execute("DELETE FROM bench WHERE id = 1;").unwrap();
        let delete = delete.elapsed();
        let selected = database.execute("SELECT age FROM bench;").unwrap();
        let crate::StatementResult::Query(query) = &selected[0] else {
            panic!("expected a query");
        };
        let bytes = std::fs::metadata(&path).unwrap().len();
        drop(database);
        let _ = std::fs::remove_dir_all(&dir);
        IndexProfile {
            build,
            insert,
            update,
            delete,
            bytes,
            selected: query.rows.len(),
        }
    }

    #[test]
    #[ignore = "release profile of index-aware SELECT"]
    fn profile_index_select() {
        print_select_host();
        for rows in [1_000_i64, 10_000, 100_000] {
            let (indexed_dir, mut indexed) = load_select_database(rows, true);
            let (plain_dir, mut plain) = load_select_database(rows, false);
            for (kind, sql) in select_queries() {
                let indexed_sample = measure_select(&mut indexed, sql);
                let plain_sample = measure_select(&mut plain, sql);
                assert_eq!(indexed_sample.rows, plain_sample.rows, "{kind}");
                assert_eq!(indexed_sample.sum, plain_sample.sum, "{kind}");
                let expected = expected_hits(rows, kind);
                assert_eq!(indexed_sample.rows, expected, "{kind}");
                let selectivity = indexed_sample.rows as f64 / rows as f64;
                let warm_speedup = plain_sample.warm_us as f64 / indexed_sample.warm_us as f64;
                let cold_speedup = plain_sample.cold_us as f64 / indexed_sample.cold_us as f64;
                let path = explain_access(&mut indexed, sql);
                println!(
                    "select_profile n={rows} kind={kind} path={path} rows={} selectivity={selectivity:.4} indexed_cold_us={} indexed_warm_us={} seq_cold_us={} seq_warm_us={} cold_speedup={cold_speedup:.3} warm_speedup={warm_speedup:.3} checksum={}",
                    indexed_sample.rows,
                    indexed_sample.cold_us,
                    indexed_sample.warm_us,
                    plain_sample.cold_us,
                    plain_sample.warm_us,
                    indexed_sample.sum
                );
            }
            drop(indexed);
            drop(plain);
            let _ = std::fs::remove_dir_all(&indexed_dir);
            let _ = std::fs::remove_dir_all(&plain_dir);
        }
    }

    fn explain_access(database: &mut Database, sql: &str) -> String {
        let results = database
            .execute(&format!("EXPLAIN {sql}"))
            .expect("explain");
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected EXPLAIN rows");
        };
        match &query.rows[0][0] {
            Value::Text(access) => access.clone(),
            other => panic!("unexpected access {other:?}"),
        }
    }

    fn select_queries() -> [(&'static str, &'static str); 4] {
        [
            ("eq", "SELECT id FROM bench WHERE age = 7;"),
            (
                "range10",
                "SELECT id FROM bench WHERE age >= 0 AND age <= 9;",
            ),
            ("range50", "SELECT id FROM bench WHERE age >= 50;"),
            ("all", "SELECT id FROM bench WHERE age >= 0;"),
        ]
    }

    fn expected_hits(rows: i64, kind: &str) -> usize {
        match kind {
            "eq" => (1..=rows).filter(|id| id % 100 == 7).count(),
            "range10" => (1..=rows).filter(|id| id % 100 <= 9).count(),
            "range50" => (1..=rows).filter(|id| id % 100 >= 50).count(),
            "all" => usize::try_from(rows).unwrap(),
            _ => panic!("unknown query {kind}"),
        }
    }

    struct SelectSample {
        cold_us: u128,
        warm_us: u128,
        rows: usize,
        sum: i64,
    }

    fn measure_select(database: &mut Database, sql: &str) -> SelectSample {
        database.clear_page_cache();
        let (cold_us, rows, sum) = timed_select(database, sql);
        let mut warm = Vec::with_capacity(9);
        for _ in 0..9 {
            let (elapsed, next_rows, next_sum) = timed_select(database, sql);
            assert_eq!(next_rows, rows);
            assert_eq!(next_sum, sum);
            warm.push(elapsed);
        }
        warm.sort_unstable();
        SelectSample {
            cold_us,
            warm_us: warm[warm.len() / 2],
            rows,
            sum,
        }
    }

    fn timed_select(database: &mut Database, sql: &str) -> (u128, usize, i64) {
        let started = std::time::Instant::now();
        let results = database.execute(sql).unwrap();
        let StatementResult::Query(query) = &results[0] else {
            panic!("expected a query");
        };
        let mut sum = 0i64;
        for row in &query.rows {
            let Value::Integer(id) = row[0] else {
                panic!("expected an integer id");
            };
            sum = sum.wrapping_add(id);
        }
        let rows = query.rows.len();
        std::hint::black_box(sum);
        std::hint::black_box(rows);
        (started.elapsed().as_micros(), rows, sum)
    }

    fn load_select_database(rows: i64, indexed: bool) -> (crate::test_dir::TestDir, Database) {
        let (dir, path) = scratch(&format!("select-{indexed}"));
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE bench (id INT, age INT, note TEXT);")
            .unwrap();
        let mut start = 1i64;
        while start <= rows {
            let end = (start + 499).min(rows);
            let mut sql = String::from("INSERT INTO bench VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                sql.push_str(&format!("({id}, {}, 'n')", id % 100));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
        drop(database);
        if indexed {
            Database::migrate(&path).unwrap();
            let mut database = Database::open(&path).unwrap();
            database
                .execute("CREATE INDEX idx_bench_age ON bench(age);")
                .unwrap();
            drop(database);
        }
        println!("select_profile loaded n={rows} indexed={indexed}");
        (dir, Database::open(&path).unwrap())
    }

    fn print_select_host() {
        let cpu = command_text("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let memory = command_text("sysctl", &["-n", "hw.memsize"]);
        let rustc = command_text("rustc", &["--version"]);
        println!("select_profile cpu={cpu} mem_bytes={memory} rustc={rustc}");
    }

    fn command_text(program: &str, args: &[&str]) -> String {
        let Ok(output) = std::process::Command::new(program).args(args).output() else {
            return "unknown".to_string();
        };
        if !output.status.success() {
            return "unknown".to_string();
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }
}

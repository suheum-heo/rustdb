//! Catalog of tables, either in memory or backed by a database file.

use std::collections::HashMap;
use std::path::Path;

use crate::error::DbError;
use crate::schema::Column;
use crate::storage::{IndexImage, StepError, Store, TableImage};
use crate::table::Table;

/// A database holding named tables.
///
/// [`Database::new`] keeps the catalog in memory. [`Database::open`] loads a
/// page file and publishes each successful mutating statement back to that
/// file. One process holds an exclusive lock on that file. There is one catalog.
pub struct Database {
    tables: HashMap<String, Table>,
    indexes: Vec<IndexImage>,
    store: Option<Store>,
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
    /// `INSERT`, `UPDATE`, or `DELETE` changed this table.
    Write(String),
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
}

impl Database {
    /// Create an empty in-memory database.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            indexes: Vec::new(),
            store: None,
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
                Some(table) => Undo::Replace {
                    table: table.clone(),
                    indexes: self.indexes.clone(),
                },
                None => Undo::None,
            },
            DurableChange::Index => Undo::Indexes(self.indexes.clone()),
        }
    }

    pub(crate) fn rollback(&mut self, undo: Undo) {
        match undo {
            Undo::None => {}
            Undo::Remove { table, indexes } => {
                self.tables.remove(&table);
                self.indexes = indexes;
            }
            Undo::Restore { tables, indexes } => {
                for table in tables {
                    self.tables.insert(table.name().to_string(), table);
                }
                self.indexes = indexes;
            }
            Undo::Replace { table, indexes } => {
                self.tables.insert(table.name().to_string(), table);
                self.indexes = indexes;
            }
            Undo::Indexes(indexes) => {
                self.indexes = indexes;
            }
        }
    }

    pub(crate) fn publish(&mut self) -> Result<(), StepError> {
        let images: Vec<TableImage> = self.tables.values().map(Table::image).collect();
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        store.publish_indexes(&images, &self.indexes)
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

    #[test]
    fn a_failed_checkpoint_blocks_every_statement_until_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "rustdb-fence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
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
        let dir = std::env::temp_dir().join(format!(
            "rustdb-ambiguous-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
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
        let dir = std::env::temp_dir().join(format!(
            "rustdb-index-profile-{}-{}-{}",
            std::process::id(),
            size,
            with_index as u8
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db");
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
}

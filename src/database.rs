//! Catalog of tables, either in memory or backed by a database file.

use std::collections::HashMap;
use std::path::Path;

use crate::error::DbError;
use crate::schema::Column;
use crate::storage::{StepError, Store, TableImage};
use crate::table::Table;

/// A database holding named tables.
///
/// [`Database::new`] keeps the catalog in memory. [`Database::open`] loads a
/// page file and publishes each successful mutating statement back to that
/// file. One process holds an exclusive lock on that file. There is one catalog.
pub struct Database {
    tables: HashMap<String, Table>,
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
}

/// In-memory snapshot restored when a durable publish fails.
pub(crate) enum Undo {
    /// Nothing was captured because the statement does not mutate storage.
    None,
    /// Remove a table created by the statement.
    Remove(String),
    /// Put dropped tables back.
    Restore(Vec<Table>),
    /// Replace one table with the copy taken before the statement.
    Replace(Table),
}

impl Database {
    /// Create an empty in-memory database.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            store: None,
        }
    }

    /// Open a database file, creating it when the path is missing or empty.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let (store, images) = Store::open(path.as_ref())?;
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
            store: Some(store),
        })
    }

    /// Rewrite a version 1 file as version 2. A version 2 file is left unchanged
    /// and this returns an error.
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
            DurableChange::Create(name) => Undo::Remove(name.clone()),
            DurableChange::Drop(names) => {
                let mut tables = Vec::with_capacity(names.len());
                for name in names {
                    let Some(table) = self.tables.get(name) else {
                        return Undo::None;
                    };
                    tables.push(table.clone());
                }
                Undo::Restore(tables)
            }
            DurableChange::Write(name) => match self.tables.get(name) {
                Some(table) => Undo::Replace(table.clone()),
                None => Undo::None,
            },
        }
    }

    pub(crate) fn rollback(&mut self, undo: Undo) {
        match undo {
            Undo::None => {}
            Undo::Remove(name) => {
                self.tables.remove(&name);
            }
            Undo::Restore(tables) => {
                for table in tables {
                    self.tables.insert(table.name().to_string(), table);
                }
            }
            Undo::Replace(table) => {
                self.tables.insert(table.name().to_string(), table);
            }
        }
    }

    pub(crate) fn publish(&mut self) -> Result<(), StepError> {
        let images: Vec<TableImage> = self.tables.values().map(Table::image).collect();
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        store.publish(&images)
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
}

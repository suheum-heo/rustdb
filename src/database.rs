//! Catalog of tables, either in memory or backed by a database file.

use std::collections::HashMap;
use std::path::Path;

use crate::error::DbError;
use crate::schema::Column;
use crate::storage::{Store, TableImage};
use crate::table::Table;

/// A database holding named tables.
///
/// [`Database::new`] keeps the catalog in memory. [`Database::open`] loads a
/// page file and publishes each successful mutating statement back to that
/// file. There is one catalog and no locking.
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

    pub(crate) fn publish(&mut self) -> Result<(), DbError> {
        let images: Vec<TableImage> = self.tables.values().map(Table::image).collect();
        let Some(store) = self.store.as_mut() else {
            return Ok(());
        };
        store.publish(&images)
    }

    pub(crate) fn publication_visible(&self) -> bool {
        self.store.as_ref().is_some_and(Store::publication_visible)
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

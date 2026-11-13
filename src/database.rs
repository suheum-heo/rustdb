//! In-memory catalog of tables.

use std::collections::HashMap;

use crate::error::DbError;
use crate::schema::Column;
use crate::table::Table;

/// A database holding named tables.
///
/// There is one catalog and no sessions, locks, or files. Each successful
/// statement mutates this catalog immediately.
#[derive(Debug, Default)]
pub struct Database {
    tables: HashMap<String, Table>,
}

impl Database {
    /// Create an empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

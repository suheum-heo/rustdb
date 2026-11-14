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
    pub(crate) fn corrupt_index_root_type(&mut self, name: &str) -> Result<(), DbError> {
        self.store
            .as_mut()
            .ok_or_else(|| DbError::Internal("index scan requires a database file".to_string()))?
            .corrupt_index_root_type(name)
    }

    #[cfg(test)]
    pub(crate) fn clear_page_cache(&mut self) {
        if let Some(store) = self.store.as_mut() {
            store.clear_page_cache();
        }
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

    fn aged_index(label: &str) -> (std::path::PathBuf, Database) {
        let dir = std::env::temp_dir().join(format!(
            "rustdb-{label}-{}-{}",
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

    fn load_select_database(rows: i64, indexed: bool) -> (std::path::PathBuf, Database) {
        let dir = std::env::temp_dir().join(format!(
            "rustdb-select-{indexed}-{}-{}",
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

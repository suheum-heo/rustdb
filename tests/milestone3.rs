//! Restart, isolation, and file-format failures for the page store.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustdb::{Database, MAX_RECORD_LEN, PAGE_SIZE, StatementResult, Value};

fn temp_dir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("rustdb-m3-{}-{}-{n}", std::process::id(), nanos));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        Self { dir: temp_dir() }
    }

    fn path(&self) -> PathBuf {
        self.dir.join("test.db")
    }

    fn open(&self) -> Database {
        Database::open(self.path()).unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn exec(db: &mut Database, sql: &str) -> StatementResult {
    let mut results = db.execute(sql).unwrap();
    assert_eq!(results.len(), 1, "{sql}");
    results.pop().unwrap()
}

fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match exec(db, sql) {
        StatementResult::Query(result) => result.rows,
        other => panic!("expected a query from {sql}, got {other:?}"),
    }
}

fn failure(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

fn open_error(path: &Path) -> String {
    match Database::open(path) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("expected {} to fail", path.display()),
    }
}

#[test]
fn create_insert_update_delete_and_drop_survive_restart() {
    let file = TempDb::new();
    {
        let mut db = file.open();
        exec(
            &mut db,
            "CREATE TABLE users (id INT, name TEXT, age INT, active BOOLEAN);",
        );
        exec(
            &mut db,
            "INSERT INTO users VALUES (1, 'Ada', 36, true), (2, NULL, NULL, NULL);",
        );
        exec(
            &mut db,
            "UPDATE users SET age = 37, name = 'Augusta' WHERE id = 1;",
        );
        exec(&mut db, "DELETE FROM users WHERE name IS NULL;");
        exec(&mut db, "CREATE TABLE empty (id INT);");
    }

    let bytes_before_select = fs::read(file.path()).unwrap();
    {
        let mut db = file.open();
        assert_eq!(
            query(
                &mut db,
                "SELECT id, name, age, active FROM users ORDER BY id;"
            ),
            vec![vec![
                Value::Integer(1),
                Value::Text("Augusta".to_string()),
                Value::Integer(37),
                Value::Boolean(true),
            ]]
        );
        assert!(query(&mut db, "SELECT * FROM empty;").is_empty());
        assert_eq!(fs::read(file.path()).unwrap(), bytes_before_select);
        exec(&mut db, "DROP TABLE users;");
    }

    let mut db = file.open();
    assert_eq!(
        failure(&mut db, "SELECT * FROM users;"),
        "table 'users' does not exist"
    );
    assert!(query(&mut db, "SELECT * FROM empty;").is_empty());
    exec(&mut db, "INSERT INTO empty VALUES (4);");
    drop(db);
    let mut db = file.open();
    assert_eq!(
        query(&mut db, "SELECT * FROM empty;"),
        vec![vec![Value::Integer(4)]]
    );
}

#[test]
fn tables_stay_isolated_and_failed_statements_do_not_publish() {
    let file = TempDb::new();
    let mut db = file.open();
    exec(&mut db, "CREATE TABLE a (id INT, name TEXT);");
    exec(&mut db, "CREATE TABLE b (id INT, name TEXT);");
    exec(&mut db, "INSERT INTO a VALUES (1, 'one');");
    exec(&mut db, "INSERT INTO b VALUES (2, 'two');");
    let published = fs::read(file.path()).unwrap();

    assert_eq!(
        failure(&mut db, "UPDATE a SET id = 'x' WHERE id = 1;"),
        "column 'id' expects INTEGER, got TEXT"
    );
    assert_eq!(
        failure(&mut db, "INSERT INTO a VALUES (3, 'ok'), (4, 5);"),
        "column 'name' expects TEXT, got INTEGER"
    );
    assert_eq!(
        failure(&mut db, "DELETE FROM b WHERE name > 1;"),
        "cannot apply operator '>' to TEXT and INTEGER"
    );
    assert_eq!(fs::read(file.path()).unwrap(), published);
    assert_eq!(
        query(&mut db, "SELECT * FROM a;"),
        vec![vec![Value::Integer(1), Value::Text("one".to_string())]]
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM b;"),
        vec![vec![Value::Integer(2), Value::Text("two".to_string())]]
    );

    exec(&mut db, "DELETE FROM a;");
    drop(db);
    let mut db = file.open();
    assert!(query(&mut db, "SELECT * FROM a;").is_empty());
    assert_eq!(
        query(&mut db, "SELECT name FROM b;"),
        vec![vec![Value::Text("two".to_string())]]
    );
}

#[test]
fn rejects_oversized_rows_without_storing_them() {
    let file = TempDb::new();
    let mut db = file.open();
    exec(&mut db, "CREATE TABLE notes (body TEXT);");
    let before = fs::read(file.path()).unwrap();
    let body = "x".repeat(MAX_RECORD_LEN);
    let error = failure(&mut db, &format!("INSERT INTO notes VALUES ('{body}');"));
    assert_eq!(
        error,
        format!("row exceeds the maximum record size of {MAX_RECORD_LEN} bytes")
    );
    assert!(query(&mut db, "SELECT * FROM notes;").is_empty());
    assert_eq!(fs::read(file.path()).unwrap(), before);
}

#[test]
fn stores_rows_across_multiple_pages() {
    let file = TempDb::new();
    {
        let mut db = file.open();
        exec(&mut db, "CREATE TABLE notes (id INT, body TEXT);");
        for id in 0..8 {
            let body = format!("{id:04}");
            let padding = "y".repeat(1200);
            exec(
                &mut db,
                &format!("INSERT INTO notes VALUES ({id}, '{body}{padding}');"),
            );
        }
    }
    let length = fs::metadata(file.path()).unwrap().len();
    assert!(length > u64::try_from(PAGE_SIZE * 3).unwrap());
    let mut db = file.open();
    let rows = query(&mut db, "SELECT id FROM notes ORDER BY id;");
    let ids: Vec<i64> = rows
        .into_iter()
        .map(|row| match row[0] {
            Value::Integer(id) => id,
            _ => panic!("expected an integer"),
        })
        .collect();
    assert_eq!(ids, (0..8).collect::<Vec<_>>());
}

#[test]
fn rejects_invalid_truncated_and_corrupt_files() {
    let file = TempDb::new();
    {
        let mut db = file.open();
        exec(&mut db, "CREATE TABLE t (id INT);");
        exec(&mut db, "INSERT INTO t VALUES (1);");
    }
    assert_eq!(
        fs::metadata(file.path()).unwrap().len() % u64::try_from(PAGE_SIZE).unwrap(),
        0
    );

    let garbage = file.dir.join("garbage.db");
    fs::write(&garbage, vec![0u8; 100]).unwrap();
    let error = open_error(&garbage);
    assert_eq!(error.to_string(), "database file is truncated");

    let zeros = file.dir.join("zeros.db");
    fs::write(&zeros, vec![0u8; PAGE_SIZE * 2]).unwrap();
    let error = open_error(&zeros);
    assert_eq!(error.to_string(), "database page 0 is corrupt");

    let mut bytes = fs::read(file.path()).unwrap();
    bytes.truncate(PAGE_SIZE);
    let short = file.dir.join("short.db");
    fs::write(&short, &bytes).unwrap();
    let error = open_error(&short);
    assert_eq!(error.to_string(), "database file is truncated");

    let mut bytes = fs::read(file.path()).unwrap();
    bytes.push(1);
    let torn = file.dir.join("torn.db");
    fs::write(&torn, &bytes).unwrap();
    let error = open_error(&torn);
    assert_eq!(error.to_string(), "database file is truncated");

    let empty = file.dir.join("empty.db");
    fs::write(&empty, b"").unwrap();
    let mut db = Database::open(&empty).unwrap();
    exec(&mut db, "CREATE TABLE t (id INT);");
    assert!(query(&mut db, "SELECT * FROM t;").is_empty());
}

#[test]
fn an_empty_new_file_is_two_pages() {
    let file = TempDb::new();
    let _db = file.open();
    assert_eq!(
        fs::metadata(file.path()).unwrap().len(),
        u64::try_from(PAGE_SIZE * 2).unwrap()
    );
}

#[test]
fn catalog_entries_span_multiple_pages_and_reload() {
    let file = TempDb::new();
    {
        let mut db = file.open();
        for id in 0..160 {
            exec(&mut db, &format!("CREATE TABLE t{id} (c INT);"));
        }
        exec(&mut db, "INSERT INTO t159 VALUES (7);");
    }
    assert!(fs::metadata(file.path()).unwrap().len() > u64::try_from(PAGE_SIZE * 2).unwrap());
    let mut db = file.open();
    assert!(query(&mut db, "SELECT * FROM t0;").is_empty());
    assert_eq!(
        query(&mut db, "SELECT c FROM t159;"),
        vec![vec![Value::Integer(7)]]
    );
}

#[test]
fn an_oversized_table_definition_is_rejected_without_publishing() {
    let file = TempDb::new();
    let mut db = file.open();
    let before = fs::read(file.path()).unwrap();
    let name = "t".repeat(5000);
    let error = failure(&mut db, &format!("CREATE TABLE \"{name}\" (c INT);"));
    assert_eq!(
        error,
        format!("table definition exceeds the maximum record size of {MAX_RECORD_LEN} bytes")
    );
    assert_eq!(fs::read(file.path()).unwrap(), before);
    exec(&mut db, "CREATE TABLE ok (id INT);");
    assert!(query(&mut db, "SELECT * FROM ok;").is_empty());
    let missing = failure(&mut db, &format!("SELECT * FROM \"{name}\";"));
    assert!(missing.contains("does not exist"), "{missing}");
}

#[test]
fn a_partial_temp_file_is_ignored_on_open() {
    let file = TempDb::new();
    {
        let mut db = file.open();
        exec(&mut db, "CREATE TABLE users (id INT, name TEXT);");
        exec(&mut db, "INSERT INTO users VALUES (1, 'Ann');");
    }
    fs::write(file.dir.join(".test.db.crash.tmp"), b"not a database").unwrap();
    let mut db = file.open();
    assert_eq!(
        query(&mut db, "SELECT name FROM users;"),
        vec![vec![Value::Text("Ann".to_string())]]
    );
}

#[cfg(unix)]
#[test]
fn a_failed_publish_restores_memory_and_keeps_the_previous_file() {
    use std::os::unix::fs::PermissionsExt;

    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            if let Ok(metadata) = fs::metadata(&self.0) {
                let mut permissions = metadata.permissions();
                permissions.set_mode(0o755);
                let _ = fs::set_permissions(&self.0, permissions);
            }
        }
    }

    let file = TempDb::new();
    let mut db = file.open();
    exec(&mut db, "CREATE TABLE users (id INT, name TEXT);");
    exec(&mut db, "INSERT INTO users VALUES (1, 'Ann');");
    let before = fs::read(file.path()).unwrap();
    let _restore = Restore(file.dir.clone());
    let mut permissions = fs::metadata(&file.dir).unwrap().permissions();
    permissions.set_mode(0o555);
    fs::set_permissions(&file.dir, permissions).unwrap();

    let error = failure(&mut db, "INSERT INTO users VALUES (2, 'Bea');");
    assert!(error.contains("failed to write database file"), "{error}");
    assert_eq!(
        query(&mut db, "SELECT name FROM users;"),
        vec![vec![Value::Text("Ann".to_string())]]
    );
    assert_eq!(fs::read(file.path()).unwrap(), before);
    drop(_restore);
    drop(db);
    let mut again = file.open();
    assert_eq!(
        query(&mut again, "SELECT name FROM users;"),
        vec![vec![Value::Text("Ann".to_string())]]
    );
}

//! SQL index statements, migration, and restart behavior.

use rustdb::Database;

fn temporary() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "rustdb-m5-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db");
    (dir, path)
}

#[test]
fn create_index_on_version_2_asks_for_migration() {
    let (dir, path) = temporary();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, age INT, name TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 30, 'Ada'), (2, NULL, 'Bea');")
        .unwrap();
    let error = database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap_err();
    assert!(error.to_string().contains("explicit migration"), "{error}");
    assert!(error.to_string().contains("--migrate"), "{error}");
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap();
    database
        .execute("CREATE INDEX idx_users_id ON users(id);")
        .unwrap();
    let error = database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
    let error = database
        .execute("CREATE INDEX idx_users_name ON users(name);")
        .unwrap_err();
    assert!(error.to_string().contains("INTEGER"), "{error}");
    let rows = database
        .execute("SELECT age FROM users ORDER BY age;")
        .unwrap();
    let rendered = rustdb::render_result(&rows[0]);
    assert!(rendered.contains("NULL"), "{rendered}");
    assert!(rendered.contains("30"), "{rendered}");
    database
        .execute("UPDATE users SET age = 40 WHERE id = 1;")
        .unwrap();
    database
        .execute("UPDATE users SET age = NULL WHERE id = 2;")
        .unwrap();
    database
        .execute("UPDATE users SET age = 7 WHERE id = 2;")
        .unwrap();
    database.execute("DELETE FROM users WHERE id = 1;").unwrap();
    database.execute("DROP INDEX idx_users_age;").unwrap();
    database.execute("DROP TABLE users;").unwrap();
    let error = database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap_err();
    assert!(error.to_string().contains("does not exist"), "{error}");
    drop(database);
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, age INT, name TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (3, 7, 'Bea');")
        .unwrap();
    let rendered = rustdb::render_result(&database.execute("SELECT name FROM users;").unwrap()[0]);
    assert!(rendered.contains("Bea"), "{rendered}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn in_memory_databases_reject_index_statements() {
    let mut database = Database::new();
    database.execute("CREATE TABLE users (age INT);").unwrap();
    let error = database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap_err();
    assert!(error.to_string().contains("database file"), "{error}");
    let error = database.execute("DROP INDEX idx_users_age;").unwrap_err();
    assert!(error.to_string().contains("database file"), "{error}");
}

#[test]
fn rejects_unsupported_index_syntax() {
    let (dir, path) = temporary();
    Database::migrate({
        let database = Database::open(&path).unwrap();
        drop(database);
        &path
    })
    .unwrap();
    let mut database = Database::open(&path).unwrap();
    database.execute("CREATE TABLE users (age INT);").unwrap();
    for sql in [
        "CREATE UNIQUE INDEX idx ON users(age);",
        "CREATE INDEX idx ON users(age, age);",
        "CREATE INDEX idx ON users(age DESC);",
        "DROP INDEX IF EXISTS idx;",
    ] {
        let error = database.execute(sql).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("not supported") || message.contains("exactly one column"),
            "{sql}: {message}"
        );
    }
    let _ = std::fs::remove_dir_all(dir);
}

fn index_pages(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .chunks(4096)
        .filter(|page| {
            let kind = u16::from_le_bytes([page[0], page[1]]);
            kind == 6 || kind == 7
        })
        .map(<[u8]>::to_vec)
        .collect()
}

#[test]
fn statements_maintain_every_index_or_leave_the_file_unchanged() {
    let (dir, path) = temporary();
    {
        let database = Database::open(&path).unwrap();
        drop(database);
        Database::migrate(&path).unwrap();
    }
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, age INT, name TEXT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES (1, 10, 'a'), (2, 20, 'b'), (3, NULL, 'c'), (4, 20, 'd');",
        )
        .unwrap();
    database
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap();
    database
        .execute("CREATE INDEX idx_users_id ON users(id);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (5, NULL, 'e'), (6, 20, 'f');")
        .unwrap();
    let before_name = std::fs::read(&path).unwrap();
    database.execute("UPDATE users SET name = 'z';").unwrap();
    let after_name = std::fs::read(&path).unwrap();
    assert_ne!(before_name, after_name);
    assert_eq!(index_pages(&before_name), index_pages(&after_name));
    database.execute("UPDATE users SET age = 5;").unwrap();
    database
        .execute("UPDATE users SET age = NULL WHERE id = 1;")
        .unwrap();
    database
        .execute("UPDATE users SET age = NULL WHERE id = 3;")
        .unwrap();
    database
        .execute("UPDATE users SET age = 8 WHERE id = 1;")
        .unwrap();
    let committed = std::fs::read(&path).unwrap();
    let error = database
        .execute("UPDATE users SET age = 'no', name = 'x';")
        .unwrap_err();
    assert!(!error.to_string().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), committed);
    let names = rustdb::render_result(&database.execute("SELECT name FROM users;").unwrap()[0]);
    assert!(names.contains('z'), "{names}");
    assert!(!names.contains('x'), "{names}");
    database.execute("DELETE FROM users WHERE id = 2;").unwrap();
    database.execute("DROP INDEX idx_users_age;").unwrap();
    drop(database);
    let mut database = Database::open(&path).unwrap();
    database
        .execute("UPDATE users SET id = 9 WHERE id = 4;")
        .unwrap();
    database.execute("DROP TABLE users;").unwrap();
    drop(database);
    Database::open(&path).unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

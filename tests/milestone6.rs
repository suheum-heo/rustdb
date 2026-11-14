//! Index-aware `SELECT`, `EXPLAIN`, and agreement with a sequential scan.

use std::sync::atomic::{AtomicU64, Ordering};

use rustdb::{Database, StatementResult, Value};

fn temporary(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "rustdb-m6-{label}-{}-{}-{n}",
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

fn query(database: &mut Database, sql: &str) -> String {
    let results = database.execute(sql).expect(sql);
    assert_eq!(results.len(), 1, "{sql}");
    rustdb::render_result(&results[0])
}

fn same(indexed: &mut Database, plain: &mut Database, sql: &str) {
    assert_eq!(query(indexed, sql), query(plain, sql), "{sql}");
}

fn both(indexed: &mut Database, plain: &mut Database, sql: &str) {
    indexed.execute(sql).expect(sql);
    plain.execute(sql).expect(sql);
}

fn explain_row(database: &mut Database, sql: &str) -> (String, String, String) {
    let results = database.execute(sql).expect(sql);
    let StatementResult::Query(result) = &results[0] else {
        panic!("expected EXPLAIN rows");
    };
    assert_eq!(
        result.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string()
        ]
    );
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    (text(&row[0]), text(&row[1]), text(&row[2]))
}

fn text(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Null => "NULL".to_string(),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn open_pair() -> (std::path::PathBuf, Database, std::path::PathBuf, Database) {
    let (indexed_dir, indexed_path) = temporary("indexed");
    let (plain_dir, plain_path) = temporary("plain");
    let mut indexed = Database::open(&indexed_path).unwrap();
    let mut plain = Database::open(&plain_path).unwrap();
    for database in [&mut indexed, &mut plain] {
        database
            .execute("CREATE TABLE users (id INT, age INT, name TEXT);")
            .unwrap();
        database
            .execute(
                "INSERT INTO users VALUES
                (1, 25, 'Ada'),
                (2, NULL, 'Bea'),
                (3, 25, 'Cy'),
                (4, 30, 'Dee'),
                (5, 10, 'Eve'),
                (6, 40, 'Fay');",
            )
            .unwrap();
    }
    drop(indexed);
    Database::migrate(&indexed_path).unwrap();
    let mut indexed = Database::open(&indexed_path).unwrap();
    indexed
        .execute("CREATE INDEX idx_users_age ON users(age);")
        .unwrap();
    indexed
        .execute("CREATE INDEX idx_users_id ON users(id);")
        .unwrap();
    both(
        &mut indexed,
        &mut plain,
        "INSERT INTO users VALUES (7, 25, 'Gia'), (8, 18, 'Hal');",
    );
    (indexed_dir, indexed, plain_dir, plain)
}

#[test]
fn indexed_select_matches_a_sequential_scan() {
    let (indexed_dir, mut indexed, plain_dir, mut plain) = open_pair();
    let queries = [
        "SELECT * FROM users;",
        "SELECT * FROM users WHERE age = 25;",
        "SELECT name FROM users WHERE age < 25;",
        "SELECT name FROM users WHERE age <= 25;",
        "SELECT name FROM users WHERE age > 25;",
        "SELECT name FROM users WHERE age >= 25;",
        "SELECT name FROM users WHERE 25 < age;",
        "SELECT name FROM users WHERE 25 > age;",
        "SELECT name FROM users WHERE age >= 18 AND age <= 30 AND name != 'Cy';",
        "SELECT name FROM users WHERE age = 25 OR name = 'Fay';",
        "SELECT name FROM users WHERE age IS NULL;",
        "SELECT name FROM users WHERE age IS NOT NULL;",
        "SELECT name FROM users WHERE age != 25;",
        "SELECT name FROM users WHERE NOT (age = 25);",
        "SELECT name FROM users WHERE name = 'Ada';",
        "SELECT name FROM users WHERE age = 25 AND name = 'Ada';",
        "SELECT id, name FROM users WHERE age >= 10 ORDER BY age ASC, name ASC;",
        "SELECT name FROM users WHERE age >= 10 ORDER BY age DESC NULLS LAST LIMIT 3;",
        "SELECT name FROM users WHERE age > 0 LIMIT 2;",
        "SELECT name FROM users WHERE age < -9223372036854775808;",
        "SELECT name FROM users WHERE age > 9223372036854775807;",
        "SELECT name FROM users WHERE age > 30 AND age < 10;",
    ];
    for sql in queries {
        same(&mut indexed, &mut plain, sql);
    }

    both(
        &mut indexed,
        &mut plain,
        "DELETE FROM users WHERE name = 'Cy';",
    );
    same(&mut indexed, &mut plain, "SELECT name FROM users;");
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age = 25;",
    );
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users ORDER BY age ASC NULLS FIRST, id ASC LIMIT 4;",
    );

    let _ = std::fs::remove_dir_all(&indexed_dir);
    let _ = std::fs::remove_dir_all(&plain_dir);
}

#[test]
fn explain_names_the_scan_and_rejects_other_forms() {
    let (indexed_dir, mut indexed, plain_dir, mut plain) = open_pair();
    assert_eq!(
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE age = 25;"),
        (
            "IndexScan".to_string(),
            "idx_users_age".to_string(),
            "age = 25".to_string()
        )
    );
    assert_eq!(
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE 25 < age;"),
        (
            "IndexScan".to_string(),
            "idx_users_age".to_string(),
            "age >= 26".to_string()
        )
    );
    assert_eq!(
        explain_row(
            &mut indexed,
            "EXPLAIN SELECT * FROM users WHERE age >= 18 AND age < 31 AND name = 'Dee';"
        ),
        (
            "IndexScan".to_string(),
            "idx_users_age".to_string(),
            "age >= 18 AND age <= 30".to_string()
        )
    );
    assert_eq!(
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE id = 4;"),
        (
            "IndexScan".to_string(),
            "idx_users_id".to_string(),
            "id = 4".to_string()
        )
    );
    assert_eq!(
        explain_row(
            &mut indexed,
            "EXPLAIN SELECT * FROM users WHERE id = 4 AND age = 30;"
        ),
        (
            "IndexScan".to_string(),
            "idx_users_age".to_string(),
            "age = 30".to_string()
        )
    );
    assert_eq!(
        explain_row(
            &mut indexed,
            "EXPLAIN SELECT * FROM users WHERE id < 5 AND age < 20;"
        ),
        (
            "IndexScan".to_string(),
            "idx_users_age".to_string(),
            "age <= 19".to_string()
        )
    );
    for sql in [
        "EXPLAIN SELECT * FROM users;",
        "EXPLAIN SELECT * FROM users WHERE age = 25 OR name = 'Fay';",
        "EXPLAIN SELECT * FROM users WHERE age IS NULL;",
        "EXPLAIN SELECT * FROM users WHERE age IS NOT NULL;",
        "EXPLAIN SELECT * FROM users WHERE age != 25;",
        "EXPLAIN SELECT * FROM users WHERE NOT (age = 25);",
        "EXPLAIN SELECT * FROM users WHERE name = 'Ada';",
    ] {
        let (access, index, bounds) = explain_row(&mut indexed, sql);
        assert_eq!(access, "SeqScan", "{sql}");
        assert_eq!(index, "NULL", "{sql}");
        assert_eq!(bounds, "NULL", "{sql}");
    }
    assert_eq!(
        explain_row(
            &mut indexed,
            "EXPLAIN SELECT * FROM users WHERE age > 9223372036854775807;"
        )
        .2,
        "none"
    );

    let (access, _, _) = explain_row(&mut plain, "EXPLAIN SELECT * FROM users WHERE age = 25;");
    assert_eq!(access, "SeqScan");

    for sql in [
        "EXPLAIN ANALYZE SELECT * FROM users;",
        "EXPLAIN QUERY PLAN SELECT * FROM users;",
        "DESC SELECT * FROM users;",
        "EXPLAIN INSERT INTO users VALUES (9, 1, 'Ivy');",
    ] {
        let error = indexed.execute(sql).unwrap_err();
        assert!(
            error.to_string().contains("not supported") || error.to_string().contains("EXPLAIN"),
            "{sql}: {error}"
        );
    }
    let error = indexed
        .execute("SELECT * FROM users WHERE age = 'nope';")
        .unwrap_err();
    assert!(error.to_string().contains("INTEGER"), "{error}");
    let error = indexed
        .execute("EXPLAIN SELECT * FROM users WHERE age = 'nope';")
        .unwrap_err();
    assert!(error.to_string().contains("INTEGER"), "{error}");

    indexed.execute("DROP INDEX idx_users_age;").unwrap();
    let (access, _, _) = explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE age = 25;");
    assert_eq!(access, "SeqScan");
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age = 25;",
    );
    let (access, index, _) = explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE id = 7;");
    assert_eq!(access, "IndexScan");
    assert_eq!(index, "idx_users_id");

    let _ = std::fs::remove_dir_all(&indexed_dir);
    let _ = std::fs::remove_dir_all(&plain_dir);
}

#[test]
fn an_in_memory_database_still_scans() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    let (access, index, bounds) = explain_row(
        &mut database,
        "EXPLAIN SELECT name FROM users WHERE id = 1;",
    );
    assert_eq!(access, "SeqScan");
    assert_eq!(index, "NULL");
    assert_eq!(bounds, "NULL");
    assert!(query(&mut database, "SELECT name FROM users WHERE id = 1;").contains("Ada"));
}

#[test]
fn restart_uses_the_stored_index() {
    let (indexed_dir, indexed, plain_dir, mut plain) = open_pair();
    let sql = "SELECT name FROM users WHERE age >= 18 AND age <= 30 ORDER BY name ASC;";
    let expected = query(&mut plain, sql);
    drop(indexed);
    let mut indexed = Database::open(indexed_dir.join("db")).unwrap();
    assert_eq!(query(&mut indexed, sql), expected);
    let (access, index, bounds) = explain_row(
        &mut indexed,
        "EXPLAIN SELECT name FROM users WHERE age >= 18 AND age <= 30;",
    );
    assert_eq!(access, "IndexScan");
    assert_eq!(index, "idx_users_age");
    assert_eq!(bounds, "age >= 18 AND age <= 30");
    let _ = std::fs::remove_dir_all(&indexed_dir);
    let _ = std::fs::remove_dir_all(&plain_dir);
}

#[test]
fn randomized_indexes_match_sequential_scans() {
    for seed in [1_u64, 42, 0x1234_5678] {
        let (indexed_dir, indexed_path) = temporary(&format!("rand-indexed-{seed}"));
        let (plain_dir, plain_path) = temporary(&format!("rand-plain-{seed}"));
        let mut indexed = Database::open(&indexed_path).unwrap();
        let mut plain = Database::open(&plain_path).unwrap();
        both(
            &mut indexed,
            &mut plain,
            "CREATE TABLE users (id INT, age INT, name TEXT);",
        );
        let mut state = seed;
        let mut values = String::from("INSERT INTO users VALUES ");
        for id in 1..=80 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            if id != 1 {
                values.push(',');
            }
            let name = format!("n{}", state % 5);
            if state % 11 == 0 {
                values.push_str(&format!("({id}, NULL, '{name}')"));
            } else {
                let age = (state % 17) as i64 - 3;
                values.push_str(&format!("({id}, {age}, '{name}')"));
            }
        }
        both(&mut indexed, &mut plain, &values);
        drop(indexed);
        Database::migrate(&indexed_path).unwrap();
        let mut indexed = Database::open(&indexed_path).unwrap();
        indexed
            .execute("CREATE INDEX idx_users_age ON users(age);")
            .unwrap();
        let queries = [
            "SELECT * FROM users;",
            "SELECT * FROM users WHERE age = 1;",
            "SELECT * FROM users WHERE age < 2;",
            "SELECT * FROM users WHERE age <= 2;",
            "SELECT * FROM users WHERE age > 2;",
            "SELECT * FROM users WHERE age >= 2;",
            "SELECT * FROM users WHERE 2 < age;",
            "SELECT * FROM users WHERE age >= 0 AND age <= 4 AND name = 'n1';",
            "SELECT * FROM users WHERE age = 1 OR name = 'n0';",
            "SELECT * FROM users WHERE age IS NULL;",
            "SELECT * FROM users WHERE age IS NOT NULL;",
            "SELECT * FROM users WHERE age != 1;",
            "SELECT name FROM users WHERE age >= 0 ORDER BY age ASC, name ASC LIMIT 5;",
            "SELECT name FROM users WHERE age > -3 LIMIT 3;",
            "SELECT * FROM users WHERE NOT (age = 1);",
            "SELECT * FROM users ORDER BY age ASC NULLS FIRST, id DESC;",
        ];
        for sql in queries {
            same(&mut indexed, &mut plain, sql);
        }
        both(
            &mut indexed,
            &mut plain,
            "DELETE FROM users WHERE id = 4 OR id = 8 OR id = 12;",
        );
        both(
            &mut indexed,
            &mut plain,
            "UPDATE users SET age = NULL WHERE id = 5 OR id = 10 OR id = 15;",
        );
        both(
            &mut indexed,
            &mut plain,
            "UPDATE users SET age = 1 WHERE age = 2;",
        );
        both(
            &mut indexed,
            &mut plain,
            "INSERT INTO users VALUES (81, 1, 'n0'), (82, NULL, 'n1'), (83, -4, 'n2');",
        );
        for sql in queries {
            same(&mut indexed, &mut plain, sql);
        }
        drop(indexed);
        let mut indexed = Database::open(&indexed_path).unwrap();
        for sql in queries {
            same(&mut indexed, &mut plain, sql);
        }
        let _ = std::fs::remove_dir_all(&indexed_dir);
        let _ = std::fs::remove_dir_all(&plain_dir);
    }
}

#[test]
fn broad_ranges_scan_the_table_and_narrow_ranges_use_the_index() {
    let (indexed_dir, mut indexed, plain_dir, mut plain) = open_pair();
    let (access, _, _) = explain_row(
        &mut indexed,
        "EXPLAIN SELECT * FROM users WHERE age >= -9223372036854775808;",
    );
    assert_eq!(access, "SeqScan");
    let (access, _, _) = explain_row(
        &mut indexed,
        "EXPLAIN SELECT * FROM users WHERE age <= 9223372036854775807;",
    );
    assert_eq!(access, "SeqScan");
    let (access, index, _) =
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE age >= 10;");
    assert_eq!(access, "SeqScan");
    assert_eq!(index, "NULL");
    let (access, index, bounds) =
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE age > 10;");
    assert_eq!(access, "IndexScan");
    assert_eq!(index, "idx_users_age");
    assert_eq!(bounds, "age >= 11");
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age >= 10;",
    );
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age > 10;",
    );
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age >= -9223372036854775808;",
    );

    both(
        &mut indexed,
        &mut plain,
        "INSERT INTO users VALUES (9, -5, 'Ivy');",
    );
    let (access, index, bounds) =
        explain_row(&mut indexed, "EXPLAIN SELECT * FROM users WHERE age >= 0;");
    assert_eq!(access, "IndexScan");
    assert_eq!(index, "idx_users_age");
    assert_eq!(bounds, "age >= 0");
    same(
        &mut indexed,
        &mut plain,
        "SELECT name FROM users WHERE age >= 0 ORDER BY id ASC;",
    );
    let _ = std::fs::remove_dir_all(&indexed_dir);
    let _ = std::fs::remove_dir_all(&plain_dir);
}

#[test]
fn row_identity_survives_repeated_edits_and_drop() {
    let (indexed_dir, mut indexed, plain_dir, mut plain) = open_pair();
    for step in 0..4 {
        let id = 20 + step;
        both(
            &mut indexed,
            &mut plain,
            &format!("INSERT INTO users VALUES ({id}, {step}, 'n{step}');"),
        );
        both(
            &mut indexed,
            &mut plain,
            &format!("DELETE FROM users WHERE id = {id};"),
        );
    }
    both(
        &mut indexed,
        &mut plain,
        "INSERT INTO users VALUES (30, 25, 'Jo'), (31, 25, 'Kai'), (32, NULL, 'Lee');",
    );
    both(
        &mut indexed,
        &mut plain,
        "UPDATE users SET age = 7, name = 'edited' WHERE age = 25;",
    );
    both(
        &mut indexed,
        &mut plain,
        "UPDATE users SET age = NULL WHERE name = 'edited';",
    );
    both(
        &mut indexed,
        &mut plain,
        "UPDATE users SET age = 12 WHERE name = 'Eve';",
    );
    both(
        &mut indexed,
        &mut plain,
        "CREATE TABLE notes (id INT, body TEXT);",
    );
    both(
        &mut indexed,
        &mut plain,
        "INSERT INTO notes VALUES (1, 'a');",
    );
    indexed
        .execute("CREATE INDEX idx_notes_id ON notes(id);")
        .unwrap();
    both(&mut indexed, &mut plain, "DROP TABLE notes;");
    let missing = indexed.execute("SELECT * FROM notes;").unwrap_err();
    assert!(missing.to_string().contains("does not exist"), "{missing}");
    let queries = [
        "SELECT id, name, age FROM users;",
        "SELECT name FROM users WHERE age = 7;",
        "SELECT name FROM users WHERE age IS NULL;",
        "SELECT name FROM users WHERE id = 5;",
        "SELECT name FROM users WHERE age >= 12 AND age <= 40 AND name != 'Dee';",
        "SELECT name FROM users WHERE age = 10 OR name = 'Fay';",
        "SELECT id FROM users ORDER BY age ASC NULLS FIRST, id ASC LIMIT 6;",
    ];
    for sql in queries {
        same(&mut indexed, &mut plain, sql);
    }
    drop(indexed);
    let mut indexed = Database::open(indexed_dir.join("db")).unwrap();
    for sql in queries {
        same(&mut indexed, &mut plain, sql);
    }
    let (access, index, _) =
        explain_row(&mut indexed, "EXPLAIN SELECT id FROM users WHERE id = 5;");
    assert_eq!(access, "IndexScan");
    assert_eq!(index, "idx_users_id");
    let _ = std::fs::remove_dir_all(&indexed_dir);
    let _ = std::fs::remove_dir_all(&plain_dir);
}

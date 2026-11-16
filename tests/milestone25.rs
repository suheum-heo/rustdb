//! Indexed `UPDATE` and `DELETE`, checked against an independent walker.
//!
//! The walker does not use the B+ tree, the heap, or the production planner.

use rustdb::{Database, StatementResult, Value};

#[path = "support/temp_dir.rs"]
mod temp_dir;

fn step(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn generated(seed: u64, count: usize) -> Vec<Vec<Value>> {
    let mut state = seed;
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let draw = step(&mut state);
        let id = if draw.is_multiple_of(11) {
            Value::Null
        } else {
            integer((draw % 17) as i64)
        };
        rows.push(vec![
            id,
            text(&format!("n{}", draw % 5)),
            Value::Boolean(draw.is_multiple_of(2)),
            integer((index % 3) as i64),
        ]);
    }
    rows
}

fn temporary(label: &str) -> (temp_dir::KeepDir, std::path::PathBuf) {
    let dir = temp_dir::KeepDir::new(label);
    let path = dir.join("db");
    (dir, path)
}

fn changed(database: &mut Database, sql: &str) -> usize {
    match database
        .execute(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"))
        .pop()
        .unwrap()
    {
        StatementResult::Update { count } | StatementResult::Delete { count } => count,
        other => panic!("{sql}: {other:?}"),
    }
}

fn rows_of(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    let results = database.execute(sql).unwrap();
    match results.into_iter().next().unwrap() {
        StatementResult::Query(result) => result.rows,
        other => panic!("{other:?}"),
    }
}

fn insert_users(database: &mut Database, rows: &[Vec<Value>]) {
    if rows.is_empty() {
        return;
    }
    let mut sql = String::from("INSERT INTO users VALUES ");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        for (cell_index, cell) in row.iter().enumerate() {
            if cell_index > 0 {
                sql.push(',');
            }
            match cell {
                Value::Null => sql.push_str("NULL"),
                Value::Integer(value) => sql.push_str(&value.to_string()),
                Value::Boolean(true) => sql.push_str("TRUE"),
                Value::Boolean(false) => sql.push_str("FALSE"),
                Value::Text(value) => {
                    sql.push('\'');
                    sql.push_str(value);
                    sql.push('\'');
                }
                Value::Ratio { .. } => panic!("ratio"),
            }
        }
        sql.push(')');
    }
    sql.push(';');
    database.execute(&sql).unwrap();
}

fn open_users(
    label: &str,
    rows: &[Vec<Value>],
    indexed: bool,
) -> (temp_dir::KeepDir, std::path::PathBuf, Database) {
    let (dir, path) = temporary(label);
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN, bucket INT);")
            .unwrap();
        insert_users(&mut database, rows);
    }
    if indexed {
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_users_bucket ON users(bucket);")
            .unwrap();
        database
            .execute("CREATE INDEX idx_users_id ON users(id);")
            .unwrap();
        return (dir, path, database);
    }
    let database = Database::open(&path).unwrap();
    (dir, path, database)
}

fn int_eq(row: &[Value], ordinal: usize, key: i64) -> bool {
    matches!(row.get(ordinal), Some(Value::Integer(value)) if *value == key)
}

fn int_lt(row: &[Value], ordinal: usize, key: i64) -> bool {
    matches!(row.get(ordinal), Some(Value::Integer(value)) if *value < key)
}

fn is_null(row: &[Value], ordinal: usize) -> bool {
    matches!(row.get(ordinal), Some(Value::Null))
}

fn text_eq(row: &[Value], ordinal: usize, key: &str) -> bool {
    matches!(row.get(ordinal), Some(Value::Text(value)) if value == key)
}

fn bool_eq(row: &[Value], ordinal: usize, key: bool) -> bool {
    matches!(row.get(ordinal), Some(Value::Boolean(value)) if *value == key)
}

fn update_where(
    rows: &mut [Vec<Value>],
    pred: impl Fn(&[Value]) -> bool,
    assign: impl Fn(&mut Vec<Value>),
) -> usize {
    let mut count = 0;
    for row in rows {
        if pred(row) {
            assign(row);
            count += 1;
        }
    }
    count
}

fn delete_where(rows: &mut Vec<Vec<Value>>, pred: impl Fn(&[Value]) -> bool) -> usize {
    let before = rows.len();
    rows.retain(|row| !pred(row));
    before - rows.len()
}

fn expect(
    plain: &mut Database,
    indexed: &mut Database,
    walker: &[Vec<Value>],
    sql: &str,
    walked: usize,
) {
    assert_eq!(changed(plain, sql), walked, "{sql}");
    assert_eq!(changed(indexed, sql), walked, "{sql}");
    assert_same(plain, walker);
    assert_same(indexed, walker);
}
fn assert_same(database: &mut Database, rows: &[Vec<Value>]) {
    let scanned = rows_of(
        database,
        "SELECT id, name, active, bucket FROM users WHERE TRUE OR id = 0;",
    );
    assert_eq!(scanned, rows);
    for key in 0..17 {
        let selected = rows_of(
            database,
            &format!("SELECT id, name, active, bucket FROM users WHERE id = {key};"),
        );
        let expected: Vec<_> = rows
            .iter()
            .filter(|row| int_eq(row, 0, key))
            .cloned()
            .collect();
        assert_eq!(selected, expected, "id = {key}");
    }
}

#[test]
fn seeded_updates_and_deletes_match_the_walker() {
    for (seed, count) in [
        (0x2525_u64, 0_usize),
        (0x2525, 1),
        (0x2525, 8),
        (0x25a1, 17),
        (0x25b2, 4),
    ] {
        let original = generated(seed, count);
        let mut walker = original.clone();
        let (_plain_dir, _plain_path, mut plain) =
            open_users(&format!("m25-plain-{seed:x}-{count}"), &original, false);
        let (_index_dir, index_path, mut indexed) =
            open_users(&format!("m25-index-{seed:x}-{count}"), &original, true);

        let walked = update_where(
            &mut walker,
            |row| int_eq(row, 0, 3),
            |row| row[1] = text("Z"),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET name = 'Z' WHERE id = 3;",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| int_eq(row, 0, 3),
            |row| row[0] = integer(4),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET id = 4 WHERE id = 3;",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| int_eq(row, 0, 4) && text_eq(row, 1, "Z"),
            |row| row[2] = Value::Boolean(false),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET active = FALSE WHERE id = 4 AND name = 'Z';",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| int_eq(row, 0, 4),
            |row| row[0] = Value::Null,
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET id = NULL WHERE id = 4;",
            walked,
        );

        let walked = update_where(&mut walker, |row| is_null(row, 0), |row| row[1] = text("N"));
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET name = 'N' WHERE id IS NULL;",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| text_eq(row, 1, "n0"),
            |row| row[1] = text("T"),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET name = 'T' WHERE name = 'n0';",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| bool_eq(row, 2, false),
            |row| row[2] = Value::Boolean(true),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET active = TRUE WHERE active = FALSE;",
            walked,
        );

        let walked = delete_where(&mut walker, |row| int_eq(row, 0, 1));
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "DELETE FROM users WHERE id = 1;",
            walked,
        );

        let walked = delete_where(&mut walker, |row| {
            int_eq(row, 0, 2) && bool_eq(row, 2, true)
        });
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "DELETE FROM users WHERE id = 2 AND active = TRUE;",
            walked,
        );

        let walked = delete_where(&mut walker, |row| int_lt(row, 0, 2));
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "DELETE FROM users WHERE id < 2;",
            walked,
        );

        let walked = delete_where(&mut walker, |row| int_eq(row, 0, 5) || int_eq(row, 3, 0));
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "DELETE FROM users WHERE id = 5 OR bucket = 0;",
            walked,
        );

        let walked = update_where(
            &mut walker,
            |row| int_eq(row, 0, 3),
            |row| row[1] = text("Z"),
        );
        expect(
            &mut plain,
            &mut indexed,
            &walker,
            "UPDATE users SET name = 'Z' WHERE id = 3;",
            walked,
        );

        walker.push(vec![
            integer(3),
            text("back"),
            Value::Boolean(true),
            integer(0),
        ]);
        plain
            .execute("INSERT INTO users VALUES (3, 'back', TRUE, 0);")
            .unwrap();
        indexed
            .execute("INSERT INTO users VALUES (3, 'back', TRUE, 0);")
            .unwrap();
        assert_same(&mut plain, &walker);
        assert_same(&mut indexed, &walker);

        drop(indexed);
        let mut indexed = Database::open(&index_path).unwrap();
        assert_same(&mut indexed, &walker);
        let _ = seed;
    }
}

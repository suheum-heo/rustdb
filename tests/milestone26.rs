//! Selective `UPDATE` checked against an independent walker.
//!
//! The walker does not use the heap, the B+ tree, the planner, or either
//! publish path. The full-table replacement is compared with this walker in
//! the library tests, through the full-rewrite guard.

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

fn sql_value(sql: &mut String, cell: &Value) {
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
            sql_value(&mut sql, cell);
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

fn text_eq(row: &[Value], ordinal: usize, key: &str) -> bool {
    matches!(row.get(ordinal), Some(Value::Text(value)) if value == key)
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

fn expect(database: &mut Database, walker: &[Vec<Value>], sql: &str, walked: usize) {
    assert_eq!(changed(database, sql), walked, "{sql}");
    assert_same(database, walker);
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
    for key in 0..3 {
        let selected = rows_of(
            database,
            &format!("SELECT id, name, active, bucket FROM users WHERE bucket = {key};"),
        );
        let expected: Vec<_> = rows
            .iter()
            .filter(|row| int_eq(row, 3, key))
            .cloned()
            .collect();
        assert_eq!(selected, expected, "bucket = {key}");
    }
}

fn apply_script(seed: u64, count: usize) {
    let original = generated(seed, count);
    let mut walker = original.clone();
    let (_scan_dir, _scan_path, mut scanned) =
        open_users(&format!("m26-scan-{seed:x}-{count}"), &original, false);
    let (_index_dir, index_path, mut indexed) =
        open_users(&format!("m26-index-{seed:x}-{count}"), &original, true);

    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 0, 1),
        |row| row[1] = text("z"),
    );
    let sql = "UPDATE users SET name = 'z' WHERE id = 1;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 3, 0),
        |row| {
            row[1] = text("many");
            row[2] = Value::Boolean(false);
        },
    );
    let sql = "UPDATE users SET name = 'many', active = FALSE WHERE bucket = 0;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 0, 4),
        |row| row[0] = Value::Null,
    );
    let sql = "UPDATE users SET id = NULL WHERE id = 4;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let walked = update_where(
        &mut walker,
        |row| matches!(row.first(), Some(Value::Null)),
        |row| row[0] = integer(6),
    );
    let sql = "UPDATE users SET id = 6 WHERE id IS NULL;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let walked = update_where(
        &mut walker,
        |row| text_eq(row, 1, "z"),
        |row| row[1] = text("z"),
    );
    let sql = "UPDATE users SET name = 'z' WHERE name = 'z';";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let walked = update_where(&mut walker, |row| int_eq(row, 0, 1), |_| {});
    let sql = "UPDATE users SET name = 'z' WHERE id = 1;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    let error = indexed
        .execute("UPDATE users SET name = 1 WHERE id = 1;")
        .unwrap_err()
        .to_string();
    assert!(error.contains("type") || error.contains("TEXT"), "{error}");
    assert_same(&mut indexed, &walker);
    assert_same(&mut scanned, &walker);

    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 0, 6),
        |row| row[1] = text(&"w".repeat(40)),
    );
    let wide = "w".repeat(40);
    let sql = format!("UPDATE users SET name = '{wide}' WHERE id = 6;");
    expect(&mut scanned, &walker, &sql, walked);
    expect(&mut indexed, &walker, &sql, walked);

    let walked = update_where(
        &mut walker,
        |row| text_eq(row, 1, &wide),
        |row| row[1] = text("s"),
    );
    let sql = format!("UPDATE users SET name = 's' WHERE name = '{wide}';");
    expect(&mut scanned, &walker, &sql, walked);
    expect(&mut indexed, &walker, &sql, walked);

    let walked = delete_where(&mut walker, |row| int_eq(row, 3, 1));
    let sql = "DELETE FROM users WHERE bucket = 1;";
    expect(&mut scanned, &walker, sql, walked);
    expect(&mut indexed, &walker, sql, walked);

    drop(indexed);
    let mut reopened = Database::open(&index_path).unwrap();
    assert_same(&mut reopened, &walker);
    drop(reopened);
    let mut reopened = Database::open(&index_path).unwrap();
    assert_same(&mut reopened, &walker);
}

#[test]
fn seeded_selective_updates_match_the_walker() {
    for (seed, count) in [
        (0x2626_u64, 0_usize),
        (0x2626, 1),
        (0x2626, 8),
        (0x26a1, 17),
        (0x26b2, 4),
    ] {
        apply_script(seed, count);
    }
}

#[test]
fn a_row_near_the_page_limit_matches_the_walker() {
    let wide = "w".repeat(1_800);
    let wider = "w".repeat(3_200);
    let original = vec![
        vec![integer(1), text(&wide), Value::Boolean(true), integer(1)],
        vec![integer(1), text(&wide), Value::Boolean(false), integer(1)],
        vec![integer(2), text("Bea"), Value::Boolean(true), integer(2)],
    ];
    let mut walker = original.clone();
    let (_dir, path, mut indexed) = open_users("m26-page", &original, true);
    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 0, 1),
        |row| row[1] = text(&wider),
    );
    let sql = format!("UPDATE users SET name = '{wider}' WHERE id = 1;");
    expect(&mut indexed, &walker, &sql, walked);
    let walked = update_where(
        &mut walker,
        |row| int_eq(row, 0, 1),
        |row| row[1] = text("s"),
    );
    expect(
        &mut indexed,
        &walker,
        "UPDATE users SET name = 's' WHERE id = 1;",
        walked,
    );
    drop(indexed);
    let mut reopened = Database::open(&path).unwrap();
    assert_same(&mut reopened, &walker);
}

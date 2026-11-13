//! End-to-end SQL workflows against the public database API.

use rustdb::{Database, DbError, StatementResult, Value};

fn exec(db: &mut Database, sql: &str) -> Vec<StatementResult> {
    db.execute(sql).unwrap()
}

fn one(db: &mut Database, sql: &str) -> StatementResult {
    let mut results = exec(db, sql);
    assert_eq!(results.len(), 1, "expected one statement in {sql}");
    results.pop().unwrap()
}

fn query(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match one(db, sql) {
        StatementResult::Query(result) => (result.columns, result.rows),
        other => panic!("expected a query from {sql}, got {other:?}"),
    }
}

fn failure(db: &mut Database, sql: &str) -> DbError {
    db.execute(sql).unwrap_err()
}

fn users_with_rows(db: &mut Database) {
    one(
        db,
        "CREATE TABLE users (id INT, name TEXT, age INT, active BOOLEAN);",
    );
    one(
        db,
        "INSERT INTO users VALUES
            (1, 'Alice', 24, true),
            (2, 'Bob', 30, false),
            (3, 'Cara', 30, true);",
    );
}

#[test]
fn creates_inserts_selects_and_drops() {
    let mut db = Database::new();
    assert_eq!(
        one(&mut db, "CREATE TABLE users (id INT, name TEXT, age INT);"),
        StatementResult::Ok
    );
    assert_eq!(
        one(&mut db, "INSERT INTO users VALUES (1, 'Alice', 24);"),
        StatementResult::Insert { count: 1 }
    );
    assert_eq!(
        one(&mut db, "INSERT INTO users VALUES (2, 'Bob', 30);"),
        StatementResult::Insert { count: 1 }
    );

    let (columns, rows) = query(&mut db, "SELECT name FROM users WHERE age > 25;");
    assert_eq!(columns, vec!["name".to_string()]);
    assert_eq!(rows, vec![vec![Value::Text("Bob".to_string())]]);

    let (columns, rows) = query(&mut db, "SELECT * FROM users;");
    assert_eq!(
        columns,
        vec!["id".to_string(), "name".to_string(), "age".to_string()]
    );
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Text("Alice".to_string()),
                Value::Integer(24),
            ],
            vec![
                Value::Integer(2),
                Value::Text("Bob".to_string()),
                Value::Integer(30),
            ],
        ]
    );

    assert_eq!(one(&mut db, "DROP TABLE users;"), StatementResult::Ok);
    assert_eq!(
        failure(&mut db, "SELECT * FROM users;").to_string(),
        "table 'users' does not exist"
    );
}

#[test]
fn filters_with_comparisons_boolean_logic_and_text_order() {
    let mut db = Database::new();
    users_with_rows(&mut db);

    let cases = [
        ("SELECT id FROM users WHERE age = 30;", vec![2, 3]),
        ("SELECT id FROM users WHERE age != 30;", vec![1]),
        ("SELECT id FROM users WHERE age <> 24;", vec![2, 3]),
        ("SELECT id FROM users WHERE age < 30;", vec![1]),
        ("SELECT id FROM users WHERE age <= 24;", vec![1]),
        ("SELECT id FROM users WHERE age > 24;", vec![2, 3]),
        ("SELECT id FROM users WHERE age >= 30;", vec![2, 3]),
        (
            "SELECT id FROM users WHERE age > 24 AND active = TRUE;",
            vec![3],
        ),
        (
            "SELECT id FROM users WHERE age < 25 OR name = 'Bob';",
            vec![1, 2],
        ),
        ("SELECT id FROM users WHERE name >= 'Bob';", vec![2, 3]),
        ("SELECT id FROM users WHERE name = name;", vec![1, 2, 3]),
        ("SELECT id FROM users WHERE 1 = 0;", vec![]),
        ("SELECT id FROM users WHERE TRUE;", vec![1, 2, 3]),
    ];

    for (sql, expected) in cases {
        let (_, rows) = query(&mut db, sql);
        let ids: Vec<i64> = rows
            .into_iter()
            .map(|row| match &row[0] {
                Value::Integer(id) => *id,
                other => panic!("expected an id, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, expected, "query: {sql}");
    }
}

#[test]
fn projects_requested_columns_and_qualified_names() {
    let mut db = Database::new();
    users_with_rows(&mut db);
    let (columns, rows) = query(&mut db, "SELECT users.name, age FROM users WHERE id = 1;");
    assert_eq!(columns, vec!["name".to_string(), "age".to_string()]);
    assert_eq!(
        rows,
        vec![vec![Value::Text("Alice".to_string()), Value::Integer(24)]]
    );
}

#[test]
fn inserts_with_a_reordered_column_list() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE users (id INT, name TEXT);");
    assert_eq!(
        one(
            &mut db,
            "INSERT INTO users (name, id) VALUES ('Ada', 7), ('Bea', 8);"
        ),
        StatementResult::Insert { count: 2 }
    );
    let (_, rows) = query(&mut db, "SELECT * FROM users;");
    assert_eq!(
        rows,
        vec![
            vec![Value::Integer(7), Value::Text("Ada".to_string())],
            vec![Value::Integer(8), Value::Text("Bea".to_string())],
        ]
    );
}

#[test]
fn keeps_tables_isolated() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE a (id INT);");
    one(&mut db, "CREATE TABLE b (id INT);");
    one(&mut db, "INSERT INTO a VALUES (1);");
    one(&mut db, "INSERT INTO b VALUES (2);");
    let (_, rows) = query(&mut db, "SELECT * FROM a;");
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
}

#[test]
fn folds_unquoted_identifiers_and_keeps_quoted_case() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE Users (Age INT);");
    one(&mut db, "INSERT INTO users VALUES (41);");
    let (columns, rows) = query(&mut db, "SELECT age FROM USERS WHERE AGE = 41;");
    assert_eq!(columns, vec!["age".to_string()]);
    assert_eq!(rows, vec![vec![Value::Integer(41)]]);

    one(&mut db, "CREATE TABLE \"Users\" (\"Age\" INT);");
    one(&mut db, "INSERT INTO \"Users\" VALUES (9);");
    let (columns, rows) = query(&mut db, "SELECT \"Age\" FROM \"Users\";");
    assert_eq!(columns, vec!["Age".to_string()]);
    assert_eq!(rows, vec![vec![Value::Integer(9)]]);
    assert_eq!(
        failure(&mut db, "SELECT * FROM \"USERS\";").to_string(),
        "table 'USERS' does not exist"
    );
}

#[test]
fn stores_integer_bounds_and_empty_text() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE bounds (n BIGINT, note VARCHAR(8));");
    one(
        &mut db,
        "INSERT INTO bounds VALUES (9223372036854775807, ''), (-9223372036854775808, 'ok');",
    );
    let (_, rows) = query(&mut db, "SELECT * FROM bounds WHERE note = '';");
    assert_eq!(
        rows,
        vec![vec![Value::Integer(i64::MAX), Value::Text(String::new())]]
    );
}

#[test]
fn multi_row_insert_does_not_keep_a_partial_batch() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE t (id INT);");
    let error = failure(&mut db, "INSERT INTO t VALUES (1), ('x');");
    assert_eq!(error.to_string(), "column 'id' expects INTEGER, got TEXT");
    let (_, rows) = query(&mut db, "SELECT * FROM t;");
    assert!(rows.is_empty());
}

#[test]
fn earlier_statement_persists_when_a_later_statement_fails() {
    let mut db = Database::new();
    let error = failure(
        &mut db,
        "CREATE TABLE t (id INT); INSERT INTO t VALUES ('no');",
    );
    assert_eq!(error.to_string(), "column 'id' expects INTEGER, got TEXT");
    assert_eq!(
        one(&mut db, "INSERT INTO t VALUES (1);"),
        StatementResult::Insert { count: 1 }
    );
}

#[test]
fn runs_several_successful_statements_together() {
    let mut db = Database::new();
    let results = exec(
        &mut db,
        "CREATE TABLE t (id INT); INSERT INTO t VALUES (1), (2); SELECT id FROM t WHERE id >= 2;",
    );
    assert_eq!(results[0], StatementResult::Ok);
    assert_eq!(results[1], StatementResult::Insert { count: 2 });
    match &results[2] {
        StatementResult::Query(result) => {
            assert_eq!(result.rows, vec![vec![Value::Integer(2)]]);
        }
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn rejects_invalid_sql_and_unknown_objects() {
    let mut db = Database::new();
    users_with_rows(&mut db);

    assert!(matches!(
        failure(&mut db, "SELEC * FROM users;"),
        DbError::Parse(_)
    ));
    assert_eq!(
        failure(&mut db, "INSERT INTO missing VALUES (1);").to_string(),
        "table 'missing' does not exist"
    );
    assert_eq!(
        failure(&mut db, "SELECT title FROM users;").to_string(),
        "column 'title' does not exist in table 'users'"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM users WHERE missing = 1;").to_string(),
        "column 'missing' does not exist in table 'users'"
    );
    assert_eq!(
        failure(&mut db, "DROP TABLE missing;").to_string(),
        "table 'missing' does not exist"
    );
    assert_eq!(
        failure(&mut db, "CREATE TABLE users (id INT);").to_string(),
        "table 'users' already exists"
    );
}

#[test]
fn rejects_schema_and_value_mistakes() {
    let mut db = Database::new();
    assert_eq!(
        failure(&mut db, "CREATE TABLE t (id INT, id TEXT);").to_string(),
        "duplicate column 'id' in table 't'"
    );
    assert_eq!(
        failure(&mut db, "CREATE TABLE t ();").to_string(),
        "CREATE TABLE requires at least one column"
    );
    one(&mut db, "CREATE TABLE t (id INT, name TEXT);");
    assert_eq!(
        failure(&mut db, "INSERT INTO t VALUES (1);").to_string(),
        "table 't' expects 2 values, got 1"
    );
    assert_eq!(
        failure(&mut db, "INSERT INTO t VALUES ('x', 'y');").to_string(),
        "column 'id' expects INTEGER, got TEXT"
    );
    assert_eq!(
        failure(&mut db, "INSERT INTO t (id) VALUES (1);").to_string(),
        "missing value for column 'name' in table 't'"
    );
    assert_eq!(
        failure(&mut db, "INSERT INTO t (id, id) VALUES (1, 2);").to_string(),
        "duplicate column 'id' in table 't'"
    );
}

#[test]
fn rejects_unsupported_sql_instead_of_ignoring_it() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE t (id INT, name TEXT);");
    let messages = [
        ("UPDATE t SET name = 'x';", "UPDATE is not supported"),
        ("DELETE FROM t;", "DELETE is not supported"),
        ("SELECT * FROM t ORDER BY id;", "ORDER BY is not supported"),
        ("SELECT * FROM t LIMIT 1;", "LIMIT is not supported"),
        (
            "SELECT * FROM t JOIN t u ON t.id = u.id;",
            "JOIN is not supported",
        ),
        (
            "CREATE TABLE u (id INT PRIMARY KEY);",
            "column constraints and defaults are not supported",
        ),
        (
            "CREATE TABLE IF NOT EXISTS u (id INT);",
            "CREATE TABLE IF NOT EXISTS is not supported",
        ),
        ("INSERT INTO t VALUES (NULL);", "NULL is not supported"),
        (
            "INSERT INTO t VALUES (1.5, 'x');",
            "floating-point literals are not supported",
        ),
        (
            "SELECT * FROM t WHERE id > true;",
            "cannot apply operator '>' to INTEGER and BOOLEAN",
        ),
        (
            "SELECT * FROM t WHERE true < false;",
            "operator '<' is not supported for BOOLEAN",
        ),
        (
            "SELECT * FROM t WHERE name AND id = 1;",
            "operator 'AND' requires BOOLEAN operands, got TEXT",
        ),
        ("SELECT * FROM t WHERE NOT id = 1;", "NOT is not supported"),
        ("SELECT id + 1 FROM t;", "operator + is not supported"),
        (
            "SELECT * FROM schema.t;",
            "qualified names are not supported",
        ),
        (
            "CREATE TABLE u (id FLOAT);",
            "data type FLOAT is not supported",
        ),
    ];

    for (sql, message) in messages {
        assert_eq!(failure(&mut db, sql).to_string(), message, "query: {sql}");
    }
}

#[test]
fn drops_every_named_table_or_none_of_them() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE a (id INT);");
    one(&mut db, "CREATE TABLE b (id INT);");
    assert_eq!(
        failure(&mut db, "DROP TABLE a, missing;").to_string(),
        "table 'missing' does not exist"
    );
    let (_, rows) = query(&mut db, "SELECT * FROM a;");
    assert!(rows.is_empty());
    assert_eq!(one(&mut db, "DROP TABLE b, a;"), StatementResult::Ok);
    assert_eq!(
        failure(&mut db, "SELECT * FROM b;").to_string(),
        "table 'b' does not exist"
    );
}

#[test]
fn reports_a_where_clause_that_is_not_boolean() {
    let mut db = Database::new();
    one(&mut db, "CREATE TABLE t (name TEXT);");
    assert_eq!(
        failure(&mut db, "SELECT * FROM t WHERE name;").to_string(),
        "WHERE clause must evaluate to BOOLEAN, got TEXT"
    );
}

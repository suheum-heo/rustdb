//! NULL, UPDATE, DELETE, ORDER BY, and LIMIT.

use rustdb::{Database, StatementResult, Value};

fn exec(db: &mut Database, sql: &str) -> StatementResult {
    let mut results = db.execute(sql).unwrap();
    assert_eq!(results.len(), 1, "expected one statement in {sql}");
    results.pop().unwrap()
}

fn query(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    match exec(db, sql) {
        StatementResult::Query(result) => (result.columns, result.rows),
        other => panic!("expected a query from {sql}, got {other:?}"),
    }
}

fn failure(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

fn sample(db: &mut Database) {
    exec(
        db,
        "CREATE TABLE users (id INT, name TEXT, age INT, active BOOLEAN);",
    );
    exec(
        db,
        "INSERT INTO users VALUES
            (1, 'Ann', 10, true),
            (2, 'Bob', NULL, NULL),
            (3, NULL, 30, false),
            (4, 'Cara', 30, true);",
    );
}

fn ids(db: &mut Database, sql: &str) -> Vec<i64> {
    let (_, rows) = query(db, sql);
    rows.into_iter()
        .map(|row| match &row[0] {
            Value::Integer(id) => *id,
            other => panic!("expected an id, got {other:?} from {sql}"),
        })
        .collect()
}

#[test]
fn stores_null_and_prints_it() {
    let mut db = Database::new();
    sample(&mut db);
    let (columns, rows) = query(&mut db, "SELECT name, age FROM users WHERE id = 2;");
    assert_eq!(columns, vec!["name".to_string(), "age".to_string()]);
    assert_eq!(
        rows,
        vec![vec![Value::Text("Bob".to_string()), Value::Null]]
    );

    let rendered = rustdb::render_result(&StatementResult::Query(rustdb::QueryResult {
        columns,
        rows,
    }));
    assert_eq!(
        rendered,
        "\
+------+------+
| name | age  |
+------+------+
| Bob  | NULL |
+------+------+
1 row returned"
    );
}

#[test]
fn comparisons_with_null_are_unknown_and_do_not_match() {
    let mut db = Database::new();
    sample(&mut db);

    let cases = [
        ("SELECT id FROM users WHERE age = NULL;", vec![]),
        ("SELECT id FROM users WHERE age = age;", vec![1, 3, 4]),
        ("SELECT id FROM users WHERE NULL = 1;", vec![]),
        ("SELECT id FROM users WHERE NULL;", vec![]),
        ("SELECT id FROM users WHERE NOT NULL;", vec![]),
        ("SELECT id FROM users WHERE age > 18;", vec![3, 4]),
        ("SELECT id FROM users WHERE age IS NULL;", vec![2]),
        (
            "SELECT id FROM users WHERE name IS NOT NULL;",
            vec![1, 2, 4],
        ),
        (
            "SELECT id FROM users WHERE NOT (age IS NULL);",
            vec![1, 3, 4],
        ),
        ("SELECT id FROM users WHERE NOT active;", vec![3]),
        ("SELECT id FROM users WHERE NOT age = 10;", vec![3, 4]),
        (
            "SELECT id FROM users WHERE true OR age = NULL;",
            vec![1, 2, 3, 4],
        ),
        ("SELECT id FROM users WHERE false AND age = 10;", vec![]),
        (
            "SELECT id FROM users WHERE age > 18 OR name IS NULL;",
            vec![3, 4],
        ),
        (
            "SELECT id FROM users WHERE active IS NULL OR age IS NULL;",
            vec![2],
        ),
        ("SELECT id FROM users WHERE NOT false AND false;", vec![]),
        (
            "SELECT id FROM users WHERE NOT (false AND false);",
            vec![1, 2, 3, 4],
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(ids(&mut db, sql), expected, "query: {sql}");
    }
}

#[test]
fn rejects_null_comparisons_that_still_have_a_concrete_type_conflict() {
    let mut db = Database::new();
    sample(&mut db);
    assert_eq!(
        failure(&mut db, "SELECT * FROM users WHERE age > 'x';"),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM users WHERE NULL > true;"),
        "operator '>' is not supported for BOOLEAN"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM users WHERE NULL AND age;"),
        "operator 'AND' requires BOOLEAN operands, got INTEGER"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM users WHERE NOT age;"),
        "operator 'NOT' requires BOOLEAN operands, got INTEGER"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM empty WHERE age IS NULL;"),
        "table 'empty' does not exist"
    );
    exec(&mut db, "CREATE TABLE empty (age INT);");
    assert_eq!(
        ids(&mut db, "SELECT age FROM empty WHERE age IS NULL;"),
        Vec::<i64>::new()
    );
}

#[test]
fn updates_with_original_row_values_and_counts_matches() {
    let mut db = Database::new();
    sample(&mut db);

    assert_eq!(
        exec(&mut db, "UPDATE users SET age = 18 WHERE id = 2;"),
        StatementResult::Update { count: 1 }
    );
    assert_eq!(
        query(&mut db, "SELECT age FROM users WHERE id = 2;").1,
        vec![vec![Value::Integer(18)]]
    );

    assert_eq!(
        exec(
            &mut db,
            "UPDATE users SET name = 'Unknown' WHERE name IS NULL;"
        ),
        StatementResult::Update { count: 1 }
    );
    assert_eq!(
        query(&mut db, "SELECT name FROM users WHERE id = 3;").1,
        vec![vec![Value::Text("Unknown".to_string())]]
    );

    assert_eq!(
        exec(&mut db, "UPDATE users SET active = false;"),
        StatementResult::Update { count: 4 }
    );

    exec(&mut db, "CREATE TABLE swap (a INT, b INT);");
    exec(&mut db, "INSERT INTO swap VALUES (1, 2);");
    exec(&mut db, "UPDATE swap SET a = b, b = a;");
    assert_eq!(
        query(&mut db, "SELECT * FROM swap;").1,
        vec![vec![Value::Integer(2), Value::Integer(1)]]
    );

    assert_eq!(
        exec(&mut db, "UPDATE swap SET a = 2 WHERE a = 2;"),
        StatementResult::Update { count: 1 }
    );
}

#[test]
fn update_is_atomic_when_validation_fails() {
    let mut db = Database::new();
    exec(&mut db, "CREATE TABLE swap (a INT, b INT);");
    exec(&mut db, "INSERT INTO swap VALUES (1, 2), (3, 4);");

    assert_eq!(
        failure(&mut db, "UPDATE swap SET a = 9, b = 'x';"),
        "column 'b' expects INTEGER, got TEXT"
    );
    assert_eq!(
        failure(&mut db, "UPDATE swap SET a = b, a = 1;"),
        "column 'a' is assigned more than once in table 'swap'"
    );
    assert_eq!(
        failure(&mut db, "UPDATE swap SET missing = 1;"),
        "column 'missing' does not exist in table 'swap'"
    );
    assert_eq!(
        failure(&mut db, "UPDATE missing SET a = 1;"),
        "table 'missing' does not exist"
    );
    assert_eq!(
        query(&mut db, "SELECT * FROM swap;").1,
        vec![
            vec![Value::Integer(1), Value::Integer(2)],
            vec![Value::Integer(3), Value::Integer(4)],
        ]
    );

    exec(&mut db, "CREATE TABLE empty (age INT);");
    assert_eq!(
        failure(&mut db, "UPDATE empty SET age = 'x';"),
        "column 'age' expects INTEGER, got TEXT"
    );
    assert_eq!(
        exec(&mut db, "UPDATE empty SET age = NULL;"),
        StatementResult::Update { count: 0 }
    );
}

#[test]
fn deletes_matching_rows_and_preserves_order() {
    let mut db = Database::new();
    sample(&mut db);

    assert_eq!(
        exec(&mut db, "DELETE FROM users WHERE age < 18;"),
        StatementResult::Delete { count: 1 }
    );
    assert_eq!(ids(&mut db, "SELECT id FROM users;"), vec![2, 3, 4]);

    assert_eq!(
        exec(&mut db, "DELETE FROM users WHERE name IS NULL;"),
        StatementResult::Delete { count: 1 }
    );
    assert_eq!(ids(&mut db, "SELECT id FROM users;"), vec![2, 4]);

    assert_eq!(
        exec(&mut db, "DELETE FROM users;"),
        StatementResult::Delete { count: 2 }
    );
    assert_eq!(
        exec(&mut db, "DELETE FROM users;"),
        StatementResult::Delete { count: 0 }
    );
    let (columns, rows) = query(&mut db, "SELECT id, name FROM users;");
    assert_eq!(columns, vec!["id".to_string(), "name".to_string()]);
    assert!(rows.is_empty());
}

#[test]
fn delete_is_atomic_when_the_predicate_is_invalid() {
    let mut db = Database::new();
    sample(&mut db);
    assert_eq!(
        failure(&mut db, "DELETE FROM users WHERE age > 'x';"),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert_eq!(
        failure(&mut db, "DELETE FROM missing;"),
        "table 'missing' does not exist"
    );
    assert_eq!(ids(&mut db, "SELECT id FROM users;"), vec![1, 2, 3, 4]);

    exec(&mut db, "CREATE TABLE empty (age INT);");
    assert_eq!(
        exec(&mut db, "DELETE FROM empty WHERE age IS NULL;"),
        StatementResult::Delete { count: 0 }
    );
}

#[test]
fn orders_nulls_as_greater_than_every_value_and_breaks_ties_stably() {
    let mut db = Database::new();
    exec(&mut db, "CREATE TABLE people (id INT, name TEXT, age INT);");
    exec(
        &mut db,
        "INSERT INTO people VALUES
            (1, 'Cara', 30),
            (2, 'Bob', NULL),
            (3, 'Ann', 10),
            (4, 'Amy', 30),
            (5, 'Ann', 10);",
    );

    assert_eq!(
        ids(&mut db, "SELECT id FROM people ORDER BY age ASC, name ASC;"),
        vec![3, 5, 4, 1, 2]
    );
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM people ORDER BY age DESC, name ASC;"
        ),
        vec![2, 4, 1, 3, 5]
    );
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM people ORDER BY age ASC NULLS FIRST, name ASC;"
        ),
        vec![2, 3, 5, 4, 1]
    );
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM people ORDER BY age DESC NULLS LAST;"
        ),
        vec![1, 4, 3, 5, 2]
    );
    // The sort column is not in the projection. Equal (age, name) pairs keep
    // insertion order: id 3 before id 5.
    assert_eq!(
        query(
            &mut db,
            "SELECT name FROM people ORDER BY age ASC, name ASC;"
        )
        .1
        .into_iter()
        .take(2)
        .collect::<Vec<_>>(),
        vec![
            vec![Value::Text("Ann".to_string())],
            vec![Value::Text("Ann".to_string())],
        ]
    );
}

#[test]
fn applies_where_before_order_by_and_limit() {
    let mut db = Database::new();
    exec(&mut db, "CREATE TABLE people (id INT, name TEXT, age INT);");
    exec(
        &mut db,
        "INSERT INTO people VALUES
            (1, 'Cara', 30),
            (2, 'Bob', NULL),
            (3, 'Ann', 10),
            (4, 'Amy', 30);",
    );

    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM people
             WHERE age >= 30 OR age IS NULL
             ORDER BY age ASC NULLS LAST, name ASC
             LIMIT 2;"
        ),
        vec![4, 1]
    );
    assert_eq!(
        ids(&mut db, "SELECT id FROM people ORDER BY age ASC LIMIT 0;"),
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(
            &mut db,
            "SELECT id FROM people ORDER BY name DESC LIMIT 100;"
        ),
        vec![1, 2, 3, 4]
    );
    let (columns, rows) = query(&mut db, "SELECT name FROM people WHERE id = 0 LIMIT 5;");
    assert_eq!(columns, vec!["name".to_string()]);
    assert!(rows.is_empty());

    exec(&mut db, "CREATE TABLE empty (age INT, name TEXT);");
    let (columns, rows) = query(&mut db, "SELECT name FROM empty ORDER BY age DESC LIMIT 5;");
    assert_eq!(columns, vec!["name".to_string()]);
    assert!(rows.is_empty());
}

#[test]
fn rejects_bad_sort_and_limit_clauses() {
    let mut db = Database::new();
    exec(&mut db, "CREATE TABLE people (id INT, name TEXT);");
    assert_eq!(
        failure(&mut db, "SELECT name FROM people ORDER BY missing;"),
        "column 'missing' does not exist in table 'people'"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM people ORDER BY id + 1;"),
        "operator + is not supported"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM people LIMIT -1;"),
        "LIMIT requires a nonnegative integer"
    );
    assert_eq!(
        failure(&mut db, "SELECT * FROM people LIMIT id;"),
        "LIMIT expressions are not supported"
    );
    exec(&mut db, "CREATE TABLE empty (id INT);");
    assert_eq!(
        failure(&mut db, "SELECT * FROM empty ORDER BY missing LIMIT 1;"),
        "column 'missing' does not exist in table 'empty'"
    );
}

#[test]
fn update_and_delete_use_the_same_filter_as_select() {
    let mut db = Database::new();
    exec(&mut db, "CREATE TABLE flags (id INT, active BOOLEAN);");
    exec(
        &mut db,
        "INSERT INTO flags VALUES (1, true), (2, NULL), (3, false);",
    );
    exec(
        &mut db,
        "UPDATE flags SET active = false WHERE active IS NULL;",
    );
    exec(&mut db, "DELETE FROM flags WHERE NOT active;");
    assert_eq!(ids(&mut db, "SELECT id FROM flags;"), vec![1]);
}

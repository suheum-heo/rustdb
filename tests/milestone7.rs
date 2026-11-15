//! Two-table `INNER JOIN`: binding, nested-loop order, and `EXPLAIN`.

use rustdb::{Database, StatementResult, Value};

#[path = "support/temp_dir.rs"]
mod temp_dir;

fn temporary(label: &str) -> (temp_dir::KeepDir, std::path::PathBuf) {
    let dir = temp_dir::KeepDir::new(label);
    let path = dir.join("db");
    (dir, path)
}

fn query(database: &mut Database, sql: &str) -> rustdb::QueryResult {
    let results = database
        .execute(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error}"));
    assert_eq!(results.len(), 1, "{sql}");
    match results.into_iter().next().unwrap() {
        StatementResult::Query(result) => result,
        other => panic!("{sql}: expected a query, got {other:?}"),
    }
}

fn rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    query(database, sql).rows
}

fn failure(database: &mut Database, sql: &str) -> String {
    database.execute(sql).expect_err(sql).to_string()
}

fn seed(database: &mut Database) {
    database
        .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT, total INT, paid BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES
                (1, 'Ada', TRUE),
                (2, 'Bea', FALSE),
                (NULL, 'Cy', TRUE),
                (1, 'Dee', TRUE);",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (10, 1, 10, TRUE),
                (11, 1, 30, FALSE),
                (12, 2, NULL, TRUE),
                (13, NULL, 5, FALSE),
                (14, 9, 7, TRUE);",
        )
        .unwrap();
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn pair(name: &str, total: Option<i64>) -> Vec<Value> {
    vec![
        text(name),
        match total {
            Some(total) => integer(total),
            None => Value::Null,
        },
    ]
}

const EQUAL: &str = "SELECT users.name, orders.total
    FROM users
    INNER JOIN orders
    ON users.id = orders.user_id;";

fn equal_pairs() -> Vec<Vec<Value>> {
    vec![
        pair("Ada", Some(10)),
        pair("Ada", Some(30)),
        pair("Bea", None),
        pair("Dee", Some(10)),
        pair("Dee", Some(30)),
    ]
}

#[test]
fn basic_inner_join_keeps_insertion_order() {
    let mut database = Database::new();
    seed(&mut database);
    let result = query(&mut database, EQUAL);
    assert_eq!(
        result.columns,
        vec!["name".to_string(), "total".to_string()]
    );
    assert_eq!(result.rows, equal_pairs());
}

#[test]
fn join_shorthand_matches_inner_join() {
    let mut database = Database::new();
    seed(&mut database);
    let shorthand = "SELECT users.name, orders.total
        FROM users
        JOIN orders
        ON users.id = orders.user_id;";
    assert_eq!(rows(&mut database, shorthand), equal_pairs());
}

#[test]
fn table_aliases_hide_the_catalog_name() {
    let mut database = Database::new();
    seed(&mut database);
    let explicit = "SELECT u.name, o.total
        FROM users AS u
        INNER JOIN orders AS o
        ON u.id = o.user_id;";
    assert_eq!(rows(&mut database, explicit), equal_pairs());
    let implicit = "SELECT u.name, o.total
        FROM users u
        JOIN orders o
        ON u.id = o.user_id;";
    assert_eq!(rows(&mut database, implicit), equal_pairs());

    let hidden = failure(
        &mut database,
        "SELECT users.name FROM users AS u JOIN orders o ON u.id = o.user_id;",
    );
    assert!(hidden.contains("not in scope"), "{hidden}");

    let self_join = rows(
        &mut database,
        "SELECT a.name, b.name
            FROM users a
            JOIN users b
            ON a.id = b.id
            ORDER BY a.name, b.name;",
    );
    assert_eq!(
        self_join,
        vec![
            vec![text("Ada"), text("Ada")],
            vec![text("Ada"), text("Dee")],
            vec![text("Bea"), text("Bea")],
            vec![text("Dee"), text("Ada")],
            vec![text("Dee"), text("Dee")],
        ]
    );
    let duplicate = failure(
        &mut database,
        "SELECT * FROM users u JOIN orders u ON u.id = u.id;",
    );
    assert!(duplicate.contains("duplicate table alias"), "{duplicate}");
}

#[test]
fn qualified_and_unqualified_columns_follow_the_scope() {
    let mut database = Database::new();
    seed(&mut database);
    let result = query(
        &mut database,
        "SELECT name, total
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            WHERE name = 'Ada';",
    );
    assert_eq!(
        result.rows,
        vec![pair("Ada", Some(10)), pair("Ada", Some(30))]
    );
}

#[test]
fn ambiguous_column_names_are_rejected() {
    let mut database = Database::new();
    seed(&mut database);
    let error = failure(
        &mut database,
        "SELECT id FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(error, "column 'id' is ambiguous");
    let error = failure(
        &mut database,
        "SELECT name FROM users JOIN orders ON id = orders.user_id;",
    );
    assert_eq!(error, "column 'id' is ambiguous");
}

#[test]
fn unknown_aliases_and_columns_are_rejected() {
    let mut database = Database::new();
    seed(&mut database);
    let missing_table = failure(
        &mut database,
        "SELECT users.name FROM users JOIN missing ON users.id = missing.id;",
    );
    assert_eq!(missing_table, "table 'missing' does not exist");
    let missing_alias = failure(
        &mut database,
        "SELECT z.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(missing_alias, "table 'z' is not in scope");
    let missing_column = failure(
        &mut database,
        "SELECT users.missing FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(
        missing_column,
        "column 'missing' does not exist in table 'users'"
    );
    let unqualified = failure(
        &mut database,
        "SELECT missing FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(unqualified, "column 'missing' does not exist");
}

#[test]
fn duplicate_join_keys_emit_every_pair() {
    let mut database = Database::new();
    seed(&mut database);
    let names = rows(
        &mut database,
        "SELECT users.name, orders.id
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            WHERE users.id = 1;",
    );
    assert_eq!(
        names,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(11)],
            vec![text("Dee"), integer(10)],
            vec![text("Dee"), integer(11)],
        ]
    );
}

#[test]
fn null_join_keys_do_not_match() {
    let mut database = Database::new();
    seed(&mut database);
    let names: Vec<_> = rows(&mut database, EQUAL)
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    assert!(!names.contains(&text("Cy")));
    let null_key = rows(
        &mut database,
        "SELECT users.name, orders.id
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            WHERE orders.user_id IS NULL OR users.id IS NULL;",
    );
    assert!(null_key.is_empty(), "{null_key:?}");
}

#[test]
fn empty_tables_return_no_rows() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    let result = query(
        &mut database,
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(
        result.columns,
        vec!["name".to_string(), "total".to_string()]
    );
    assert!(result.rows.is_empty());

    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    assert!(
        rows(
            &mut database,
            "SELECT name FROM users JOIN orders ON users.id = orders.user_id;",
        )
        .is_empty()
    );
}

#[test]
fn nonmatching_rows_are_omitted() {
    let mut database = Database::new();
    seed(&mut database);
    let result = rows(
        &mut database,
        "SELECT users.name, orders.id FROM users JOIN orders ON users.id = orders.id;",
    );
    assert!(result.is_empty(), "{result:?}");
}

#[test]
fn on_and_or_and_a_non_equality_use_three_valued_logic() {
    let mut database = Database::new();
    seed(&mut database);
    let filtered = rows(
        &mut database,
        "SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id AND (orders.total > 15 OR orders.total IS NULL);",
    );
    assert_eq!(
        filtered,
        vec![
            pair("Ada", Some(30)),
            pair("Bea", None),
            pair("Dee", Some(30)),
        ]
    );

    let wider = rows(
        &mut database,
        "SELECT users.name, orders.user_id
            FROM users
            JOIN orders
            ON users.id < orders.user_id;",
    );
    assert_eq!(
        wider,
        vec![
            vec![text("Ada"), integer(2)],
            vec![text("Ada"), integer(9)],
            vec![text("Bea"), integer(9)],
            vec![text("Dee"), integer(2)],
            vec![text("Dee"), integer(9)],
        ]
    );
}

#[test]
fn where_runs_after_the_join() {
    let mut database = Database::new();
    seed(&mut database);
    let filtered = rows(
        &mut database,
        "SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            WHERE orders.total > 15 OR orders.total IS NULL;",
    );
    assert_eq!(
        filtered,
        vec![
            pair("Ada", Some(30)),
            pair("Bea", None),
            pair("Dee", Some(30)),
        ]
    );
}

#[test]
fn order_by_reads_joined_columns() {
    let mut database = Database::new();
    seed(&mut database);
    let ordered = rows(
        &mut database,
        "SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            ORDER BY orders.total ASC, users.name ASC;",
    );
    assert_eq!(
        ordered,
        vec![
            pair("Ada", Some(10)),
            pair("Dee", Some(10)),
            pair("Ada", Some(30)),
            pair("Dee", Some(30)),
            pair("Bea", None),
        ]
    );
}

#[test]
fn limit_keeps_the_first_nested_loop_rows() {
    let mut database = Database::new();
    seed(&mut database);
    let limited = rows(
        &mut database,
        "SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id
            LIMIT 2;",
    );
    assert_eq!(limited, vec![pair("Ada", Some(10)), pair("Ada", Some(30))]);
}

#[test]
fn select_star_lists_outer_columns_then_inner_columns() {
    let mut database = Database::new();
    seed(&mut database);
    let result = query(
        &mut database,
        "SELECT * FROM users JOIN orders ON users.id = orders.user_id LIMIT 1;",
    );
    assert_eq!(
        result.columns,
        vec![
            "id".to_string(),
            "name".to_string(),
            "active".to_string(),
            "id".to_string(),
            "user_id".to_string(),
            "total".to_string(),
            "paid".to_string(),
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            integer(1),
            text("Ada"),
            Value::Boolean(true),
            integer(10),
            integer(1),
            integer(10),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn select_alias_star_lists_one_input() {
    let mut database = Database::new();
    seed(&mut database);
    let result = query(
        &mut database,
        "SELECT o.* FROM users u JOIN orders o ON u.id = o.user_id LIMIT 1;",
    );
    assert_eq!(
        result.columns,
        vec![
            "id".to_string(),
            "user_id".to_string(),
            "total".to_string(),
            "paid".to_string(),
        ]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            integer(10),
            integer(1),
            integer(10),
            Value::Boolean(true),
        ]]
    );
}

#[test]
fn mixed_column_types_compare_only_when_the_types_agree() {
    let mut database = Database::new();
    seed(&mut database);
    let matched = rows(
        &mut database,
        "SELECT users.name, orders.total, users.active, orders.paid
            FROM users
            JOIN orders
            ON users.id = orders.user_id AND users.active = orders.paid;",
    );
    assert_eq!(
        matched,
        vec![
            vec![
                text("Ada"),
                integer(10),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
            vec![
                text("Dee"),
                integer(10),
                Value::Boolean(true),
                Value::Boolean(true),
            ],
        ]
    );
    let error = failure(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.name = orders.total;",
    );
    assert_eq!(error, "cannot apply operator '=' to TEXT and INTEGER");
}

#[test]
fn restart_keeps_the_join_rows_and_uses_the_inner_index() {
    let (_dir, path) = temporary("join");
    let mut database = Database::open(&path).unwrap();
    seed(&mut database);
    let before = rows(&mut database, EQUAL);
    assert_eq!(before, equal_pairs());
    drop(database);

    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, EQUAL), before);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    database
        .execute("CREATE INDEX idx_users_id ON users(id);")
        .unwrap();
    assert_eq!(rows(&mut database, EQUAL), before);

    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
    let single = query(
        &mut database,
        "EXPLAIN SELECT name FROM users WHERE id = 1;",
    );
    assert_eq!(
        single.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string()
        ]
    );
    assert_eq!(single.rows[0][0], text("IndexScan"));
    assert_eq!(
        rows(&mut database, "SELECT name FROM users WHERE id = 1;"),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
}

#[test]
fn single_table_queries_stay_compatible() {
    let mut database = Database::new();
    seed(&mut database);
    assert_eq!(
        rows(
            &mut database,
            "SELECT name FROM users WHERE id = 1 ORDER BY name LIMIT 1;",
        ),
        vec![vec![text("Ada")]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT name FROM users WHERE id = 1;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string()
        ]
    );
    assert_eq!(explained.rows[0][0], text("SeqScan"));
    let error = failure(&mut database, "SELECT * FROM users WHERE id > 'x';");
    assert_eq!(error, "cannot apply operator '>' to INTEGER and TEXT");
    database
        .execute("UPDATE users SET name = 'Ada Lovelace' WHERE name = 'Ada';")
        .unwrap();
    database
        .execute("DELETE FROM orders WHERE user_id IS NULL;")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT name FROM users WHERE id = 1;"),
        vec![vec![text("Ada Lovelace")], vec![text("Dee")]]
    );
}

#[test]
fn explain_names_the_nested_loop() {
    let mut database = Database::new();
    seed(&mut database);
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name, orders.total
            FROM users
            JOIN orders
            ON users.id = orders.user_id;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "condition".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(
        explained.rows,
        vec![vec![
            text("NestedLoopJoin"),
            text("users"),
            text("orders"),
            text("users.id = orders.user_id"),
            Value::Null,
        ]]
    );

    let filtered = query(
        &mut database,
        "EXPLAIN SELECT u.name
            FROM users u
            JOIN orders o
            ON u.id = o.user_id
            WHERE o.total > 15;",
    );
    assert_eq!(
        filtered.rows,
        vec![vec![
            text("NestedLoopJoin"),
            text("u (users)"),
            text("o (orders)"),
            text("u.id = o.user_id"),
            text("o.total > 15"),
        ]]
    );
    let error = failure(
        &mut database,
        "EXPLAIN SELECT name FROM users JOIN orders ON users.id = orders.total AND orders.paid = 'no';",
    );
    assert!(error.contains("BOOLEAN"), "{error}");
}

#[test]
fn unsupported_joins_are_rejected() {
    let mut database = Database::new();
    seed(&mut database);
    let cases = [
        (
            "SELECT * FROM users LEFT JOIN orders ON users.id = orders.user_id;",
            "LEFT JOIN is not supported",
        ),
        (
            "SELECT * FROM users RIGHT JOIN orders ON users.id = orders.user_id;",
            "RIGHT JOIN is not supported",
        ),
        (
            "SELECT * FROM users FULL JOIN orders ON users.id = orders.user_id;",
            "FULL JOIN is not supported",
        ),
        (
            "SELECT * FROM users CROSS JOIN orders;",
            "CROSS JOIN is not supported",
        ),
        (
            "SELECT * FROM users NATURAL JOIN orders;",
            "NATURAL JOIN is not supported",
        ),
        (
            "SELECT * FROM users JOIN orders USING (id);",
            "USING is not supported",
        ),
        (
            "SELECT * FROM users JOIN orders ON users.id = orders.user_id JOIN users u ON u.id = orders.user_id;",
            "multiple JOINs are not supported",
        ),
        (
            "SELECT * FROM (SELECT id FROM users) u;",
            "subqueries in FROM are not supported",
        ),
        (
            "SELECT * FROM users, orders;",
            "comma joins are not supported",
        ),
        (
            "SELECT * FROM users JOIN orders;",
            "JOIN requires an ON condition",
        ),
    ];
    for (sql, message) in cases {
        assert_eq!(failure(&mut database, sql), message, "{sql}");
    }
}

#[test]
fn a_reference_model_matches_the_nested_loop() {
    let mut state = 0x7eed_u64;
    let mut users = Vec::new();
    let mut orders = Vec::new();
    for index in 0..40 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let id = if state.is_multiple_of(7) {
            None
        } else {
            Some((state % 5) as i64)
        };
        users.push((id, format!("u{index}")));
    }
    for index in 0..50 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let user_id = if state.is_multiple_of(6) {
            None
        } else {
            Some((state % 5) as i64)
        };
        orders.push((user_id, index as i64));
    }

    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, note INT);")
        .unwrap();
    insert_users(&mut database, &users);
    insert_orders(&mut database, &orders);

    let mut expected = Vec::new();
    for (id, name) in &users {
        for (user_id, note) in &orders {
            if id.is_some() && id == user_id {
                expected.push(vec![text(name), integer(*note)]);
            }
        }
    }
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.note
                FROM users
                JOIN orders
                ON users.id = orders.user_id;",
        ),
        expected
    );
}

fn insert_users(database: &mut Database, rows: &[(Option<i64>, String)]) {
    let mut sql = String::from("INSERT INTO users VALUES ");
    for (index, (id, name)) in rows.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        match id {
            Some(id) => sql.push_str(&format!("({id}, '{name}')")),
            None => sql.push_str(&format!("(NULL, '{name}')")),
        }
    }
    database.execute(&sql).unwrap();
}

fn insert_orders(database: &mut Database, rows: &[(Option<i64>, i64)]) {
    let mut sql = String::from("INSERT INTO orders VALUES ");
    for (index, (user_id, note)) in rows.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        match user_id {
            Some(user_id) => sql.push_str(&format!("({user_id}, {note})")),
            None => sql.push_str(&format!("(NULL, {note})")),
        }
    }
    database.execute(&sql).unwrap();
}

#[test]
fn binding_rejects_hidden_names_and_checks_empty_tables() {
    let mut database = Database::new();
    seed(&mut database);
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u WHERE u.id = 1 ORDER BY u.name;",
        ),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
    let hidden = failure(&mut database, "SELECT users.id FROM users u;");
    assert_eq!(hidden, "table qualifier 'users' does not match 'u'");
    let missing = failure(&mut database, "SELECT u.missing FROM users u;");
    assert_eq!(missing, "column 'missing' does not exist in table 'u'");

    let star = query(
        &mut database,
        "SELECT a.* FROM users a JOIN users b ON a.id = b.id WHERE a.name = 'Ada';",
    );
    assert_eq!(
        star.columns,
        vec!["id".to_string(), "name".to_string(), "active".to_string()]
    );
    assert_eq!(
        star.rows,
        vec![
            vec![integer(1), text("Ada"), Value::Boolean(true)],
            vec![integer(1), text("Ada"), Value::Boolean(true)],
        ]
    );
    let both_ids = query(
        &mut database,
        "SELECT a.id, b.id FROM users a JOIN users b ON a.id = b.id WHERE a.name = 'Bea';",
    );
    assert_eq!(both_ids.columns, vec!["id".to_string(), "id".to_string()]);
    assert_eq!(both_ids.rows, vec![vec![integer(2), integer(2)]]);

    let duplicate = failure(&mut database, "SELECT * FROM users JOIN users ON id = id;");
    assert_eq!(duplicate, "duplicate table alias 'users'");
    let ambiguous = failure(
        &mut database,
        "SELECT name FROM users a JOIN users b ON id = b.id;",
    );
    assert_eq!(ambiguous, "column 'id' is ambiguous");
    let order = failure(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id ORDER BY id;",
    );
    assert_eq!(order, "column 'id' is ambiguous");

    let mut empty = Database::new();
    empty
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    empty
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    let result = query(
        &mut empty,
        "SELECT * FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(
        result.columns,
        vec![
            "id".to_string(),
            "name".to_string(),
            "user_id".to_string(),
            "total".to_string(),
        ]
    );
    assert!(result.rows.is_empty());
    let typed = failure(
        &mut empty,
        "SELECT users.name FROM users JOIN orders ON users.name = orders.total;",
    );
    assert_eq!(typed, "cannot apply operator '=' to TEXT and INTEGER");
    let limited = failure(
        &mut empty,
        "SELECT users.name FROM users JOIN orders ON users.name = orders.total LIMIT 0;",
    );
    assert_eq!(limited, "cannot apply operator '=' to TEXT and INTEGER");
}

#[test]
fn truth_values_match_a_reference_and_limits_keep_order() {
    let mut database = Database::new();
    seed(&mut database);
    let users = rows(&mut database, "SELECT id, name FROM users;");
    let orders = rows(&mut database, "SELECT user_id, total, id FROM orders;");

    let cases = [
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders ON users.id = orders.user_id;",
            reference(&users, &orders, |user, order| {
                sql_eq(&user[0], &order[0]) == Some(true)
            }),
        ),
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders ON users.id IS NULL;",
            reference(&users, &orders, |user, _| user[0].is_null()),
        ),
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders
                ON users.id IS NULL AND orders.user_id IS NULL;",
            reference(&users, &orders, |user, order| {
                sql_and(Some(user[0].is_null()), Some(order[0].is_null())) == Some(true)
            }),
        ),
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders ON NOT (users.id = orders.user_id);",
            reference(&users, &orders, |user, order| {
                sql_not(sql_eq(&user[0], &order[0])) == Some(true)
            }),
        ),
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders
                ON users.id = orders.user_id OR users.id IS NULL;",
            reference(&users, &orders, |user, order| {
                sql_or(sql_eq(&user[0], &order[0]), Some(user[0].is_null())) == Some(true)
            }),
        ),
        (
            "SELECT users.name, orders.total
                FROM users JOIN orders ON TRUE WHERE users.id IS NULL;",
            reference(&users, &orders, |user, _| user[0].is_null()),
        ),
        (
            "SELECT users.name, orders.total FROM users JOIN orders ON FALSE;",
            Vec::new(),
        ),
        (
            "SELECT users.name, orders.total FROM users JOIN orders ON NULL;",
            Vec::new(),
        ),
    ];
    for (sql, expected) in cases {
        assert_eq!(rows(&mut database, sql), expected, "{sql}");
    }

    let none = query(
        &mut database,
        "SELECT users.name, orders.total
            FROM users JOIN orders ON users.id = orders.user_id LIMIT 0;",
    );
    assert_eq!(none.columns, vec!["name".to_string(), "total".to_string()]);
    assert!(none.rows.is_empty());
    let all = rows(
        &mut database,
        "SELECT users.name, orders.total
            FROM users JOIN orders ON users.id = orders.user_id LIMIT 100;",
    );
    assert_eq!(all, equal_pairs());
    let ordered = rows(
        &mut database,
        "SELECT users.name
            FROM users JOIN orders ON users.id = orders.user_id
            ORDER BY orders.id ASC;",
    );
    assert_eq!(
        ordered,
        vec![
            vec![text("Ada")],
            vec![text("Dee")],
            vec![text("Ada")],
            vec![text("Dee")],
            vec![text("Bea")],
        ]
    );
}

fn reference(
    users: &[Vec<Value>],
    orders: &[Vec<Value>],
    keep: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let mut expected = Vec::new();
    for user in users {
        for order in orders {
            if keep(user, order) {
                expected.push(vec![user[1].clone(), order[1].clone()]);
            }
        }
    }
    expected
}

fn sql_eq(left: &Value, right: &Value) -> Option<bool> {
    if left.is_null() || right.is_null() {
        None
    } else {
        Some(left == right)
    }
}

fn sql_not(value: Option<bool>) -> Option<bool> {
    value.map(|value| !value)
}

fn sql_and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn sql_or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[test]
fn explain_binds_without_running_or_writing() {
    let (_dir, path) = temporary("explain");
    let mut database = Database::open(&path).unwrap();
    seed(&mut database);
    database
        .execute("INSERT INTO users VALUES (8, 'Ivy', TRUE);")
        .unwrap();
    drop(database);
    let before = std::fs::metadata(&path).unwrap().len();
    let wal = {
        let mut name = path.as_os_str().to_owned();
        name.push(".wal");
        std::path::PathBuf::from(name)
    };
    assert_eq!(
        wal.metadata().map(|meta| meta.len()).unwrap_or(0),
        0,
        "a committed insert truncates the redo log"
    );

    let mut database = Database::open(&path).unwrap();
    let before_rows = rows(&mut database, "SELECT name FROM users;");
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users AS u JOIN orders o ON u.id = o.user_id WHERE o.total > 15;",
    );
    assert_eq!(
        explained.rows,
        vec![vec![
            text("NestedLoopJoin"),
            text("u (users)"),
            text("o (orders)"),
            text("u.id = o.user_id"),
            text("o.total > 15"),
        ]]
    );
    assert_eq!(rows(&mut database, "SELECT name FROM users;"), before_rows);
    let ambiguous = failure(
        &mut database,
        "EXPLAIN SELECT id FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(ambiguous, "column 'id' is ambiguous");
    let unsupported = failure(
        &mut database,
        "EXPLAIN SELECT * FROM users LEFT JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(unsupported, "LEFT JOIN is not supported");
    assert_eq!(rows(&mut database, EQUAL).len(), 5);
    drop(database);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(wal.metadata().map(|meta| meta.len()).unwrap_or(0), 0);
}

#[test]
fn repeated_open_keeps_the_lock_and_the_index() {
    let (_dir, path) = temporary("lock");
    {
        let mut database = Database::open(&path).unwrap();
        seed(&mut database);
    }
    Database::migrate(&path).unwrap();
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_users_id ON users(id);")
            .unwrap();
    }
    let expected = equal_pairs();
    for _ in 0..4 {
        let mut database = Database::open(&path).unwrap();
        assert_eq!(rows(&mut database, EQUAL), expected);
        let explained = query(
            &mut database,
            "EXPLAIN SELECT name FROM users u WHERE u.id = 1;",
        );
        assert_eq!(explained.rows[0][0], text("IndexScan"));
        assert_eq!(explained.rows[0][2], text("id = 1"));
        drop(database);
    }
    let held = Database::open(&path).unwrap();
    let blocked = match Database::open(&path) {
        Ok(_) => panic!("a second open must fail while the first database is open"),
        Err(error) => error,
    };
    assert!(blocked.to_string().contains("is locked"), "{blocked}");
    drop(held);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(&mut database, "SELECT name FROM users WHERE id = 1;"),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
}

#[test]
#[ignore = "release profile of nested-loop join"]
fn profile_nested_loop_join() {
    let cpu = command_text("sysctl", &["-n", "machdep.cpu.brand_string"]);
    let memory = command_text("sysctl", &["-n", "hw.memsize"]);
    let rustc = command_text("rustc", &["--version"]);
    println!("join_profile cpu={cpu} mem_bytes={memory} rustc={rustc}");
    println!(
        "join_profile value_bytes={} vec_bytes={}",
        std::mem::size_of::<Value>(),
        std::mem::size_of::<Vec<Value>>()
    );

    for rows in [100_i64, 1_000, 4_000] {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE lhs (id INT, note INT);")
            .unwrap();
        database
            .execute("CREATE TABLE rhs (id INT, note INT);")
            .unwrap();
        load_identity(&mut database, "lhs", rows);
        load_identity(&mut database, "rhs", rows);
        let sample = measure_join(
            &mut database,
            "SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id;",
        );
        let pairs = rows as u128 * rows as u128;
        assert_eq!(sample.matches, rows as usize);
        assert_eq!(sample.sum, (rows - 1) * rows / 2);
        let copied_bytes =
            rows as usize * (std::mem::size_of::<Vec<Value>>() + 2 * std::mem::size_of::<Value>());
        println!(
            "join_profile shape=identity n={rows} m={rows} pairs={pairs} matches={} cold_us={} warm_us={} result_bytes={} copied_rows={rows} copied_bytes={copied_bytes}",
            sample.matches, sample.cold_us, sample.warm_us, sample.result_bytes
        );
    }

    let rows = 1_000_i64;
    let mut database = Database::new();
    database
        .execute("CREATE TABLE lhs (id INT, note INT);")
        .unwrap();
    database
        .execute("CREATE TABLE rhs (id INT, note INT);")
        .unwrap();
    load_identity(&mut database, "lhs", rows);
    load_mod(&mut database, rows, 50);
    let sample = measure_join(
        &mut database,
        "SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id;",
    );
    assert_eq!(sample.matches, 1_000);
    println!(
        "join_profile shape=duplicate n={rows} m={rows} pairs={} matches={} cold_us={} warm_us={} result_bytes={} copied_rows={rows}",
        rows as u128 * rows as u128,
        sample.matches,
        sample.cold_us,
        sample.warm_us,
        sample.result_bytes
    );
}

fn load_identity(database: &mut Database, table: &str, rows: i64) {
    let mut start = 0_i64;
    while start < rows {
        let end = (start + 499).min(rows - 1);
        let mut sql = format!("INSERT INTO {table} VALUES ");
        for id in start..=end {
            if id != start {
                sql.push(',');
            }
            sql.push_str(&format!("({id}, {id})"));
        }
        database.execute(&sql).unwrap();
        start = end + 1;
    }
}

fn load_mod(database: &mut Database, rows: i64, modulus: i64) {
    let mut start = 0_i64;
    while start < rows {
        let end = (start + 499).min(rows - 1);
        let mut sql = String::from("INSERT INTO rhs VALUES ");
        for id in start..=end {
            if id != start {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {id})", id % modulus));
        }
        database.execute(&sql).unwrap();
        start = end + 1;
    }
}

struct JoinSample {
    cold_us: u128,
    warm_us: u128,
    matches: usize,
    sum: i64,
    result_bytes: usize,
}

fn measure_join(database: &mut Database, sql: &str) -> JoinSample {
    let (cold_us, matches, sum, result_bytes) = timed_join(database, sql);
    let mut warm = Vec::with_capacity(5);
    for _ in 0..5 {
        let (elapsed, next_matches, next_sum, _) = timed_join(database, sql);
        assert_eq!(next_matches, matches);
        assert_eq!(next_sum, sum);
        warm.push(elapsed);
    }
    warm.sort_unstable();
    JoinSample {
        cold_us,
        warm_us: warm[warm.len() / 2],
        matches,
        sum,
        result_bytes,
    }
}

fn timed_join(database: &mut Database, sql: &str) -> (u128, usize, i64, usize) {
    let started = std::time::Instant::now();
    let result = query(database, sql);
    let mut sum = 0_i64;
    let mut result_bytes = 0_usize;
    for row in &result.rows {
        let Value::Integer(id) = row[0] else {
            panic!("expected an integer id");
        };
        sum = sum.wrapping_add(id);
        result_bytes += std::mem::size_of::<Vec<Value>>();
        result_bytes += row.len() * std::mem::size_of::<Value>();
    }
    let matches = result.rows.len();
    std::hint::black_box(sum);
    std::hint::black_box(matches);
    (started.elapsed().as_micros(), matches, sum, result_bytes)
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

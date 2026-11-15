//! Index nested-loop `INNER JOIN`.

use rustdb::{Database, StatementResult, Value};

#[path = "support/temp_dir.rs"]
mod temp_dir;

const EQUAL: &str = "SELECT users.name, orders.total, orders.id
    FROM users JOIN orders ON users.id = orders.user_id;";

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

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn seed(database: &mut Database) {
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT, total INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES
                (1, 'Ada'),
                (2, 'Bea'),
                (NULL, 'Cy'),
                (1, 'Dee');",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (10, 1, 10),
                (11, 1, 30),
                (12, 2, NULL),
                (13, NULL, 5),
                (14, 9, 7);",
        )
        .unwrap();
}

fn equal_pairs() -> Vec<Vec<Value>> {
    vec![
        vec![text("Ada"), integer(10), integer(10)],
        vec![text("Ada"), integer(30), integer(11)],
        vec![text("Bea"), Value::Null, integer(12)],
        vec![text("Dee"), integer(10), integer(10)],
        vec![text("Dee"), integer(30), integer(11)],
    ]
}

/// Open a version-2 file that already contains the seed tables.
fn seeded(label: &str) -> (temp_dir::KeepDir, std::path::PathBuf) {
    let (dir, path) = temporary(label);
    let mut database = Database::open(&path).unwrap();
    seed(&mut database);
    drop(database);
    (dir, path)
}

fn migrate_and_index(path: &std::path::Path, statements: &[&str]) -> Database {
    Database::migrate(path).unwrap();
    let mut database = Database::open(path).unwrap();
    for statement in statements {
        database.execute(statement).unwrap();
    }
    database
}

#[test]
fn indexed_equality_matches_the_nested_loop_and_keeps_order() {
    let (_dir, path) = seeded("equality");
    let mut database = Database::open(&path).unwrap();
    let nested = rows(&mut database, EQUAL);
    assert_eq!(nested, equal_pairs());
    drop(database);

    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(rows(&mut database, EQUAL), nested);
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "index".to_string(),
            "predicate".to_string(),
            "residual".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(
        explained.rows,
        vec![vec![
            text("IndexNestedLoopJoin"),
            text("users"),
            text("orders"),
            text("idx_orders_user"),
            text("users.id = orders.user_id"),
            Value::Null,
            Value::Null,
        ]]
    );
    let single = query(
        &mut database,
        "EXPLAIN SELECT name FROM users WHERE id = 1;",
    );
    assert_eq!(single.rows[0][0], text("SeqScan"));
}

#[test]
fn reversed_equality_uses_the_same_index() {
    let (_dir, path) = seeded("reversed");
    let mut database = Database::open(&path).unwrap();
    let sql = "SELECT users.name, orders.id
        FROM users JOIN orders ON orders.user_id = users.id;";
    let nested = rows(&mut database, sql);
    drop(database);
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(rows(&mut database, sql), nested);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
    assert_eq!(explained.rows[0][4], text("orders.user_id = users.id"));
    assert_eq!(explained.rows[0][5], Value::Null);
}

#[test]
fn duplicate_and_null_keys_keep_nested_loop_results() {
    let (_dir, path) = seeded("nulls");
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(rows(&mut database, EQUAL), equal_pairs());
    let null_users = rows(
        &mut database,
        "SELECT users.name, orders.id FROM users JOIN orders ON users.id IS NULL;",
    );
    assert_eq!(
        null_users,
        vec![
            vec![text("Cy"), integer(10)],
            vec![text("Cy"), integer(11)],
            vec![text("Cy"), integer(12)],
            vec![text("Cy"), integer(13)],
            vec![text("Cy"), integer(14)],
        ]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id IS NULL;",
    );
    assert_eq!(explained.rows[0][0], text("NestedLoopJoin"));
    let both_null = rows(
        &mut database,
        "SELECT users.name, orders.id
            FROM users JOIN orders
            ON users.id IS NULL AND orders.user_id IS NULL;",
    );
    assert_eq!(both_null, vec![vec![text("Cy"), integer(13)]]);
}

#[test]
fn and_keeps_a_residual_predicate() {
    let (_dir, path) = seeded("residual");
    let sql = "SELECT users.name, orders.total
        FROM users JOIN orders
        ON users.id = orders.user_id AND orders.total > 15;";
    let mut database = Database::open(&path).unwrap();
    let nested = rows(&mut database, sql);
    drop(database);
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(
        rows(&mut database, sql),
        vec![
            vec![text("Ada"), integer(30)],
            vec![text("Dee"), integer(30)],
        ]
    );
    assert_eq!(rows(&mut database, sql), nested);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][4], text("users.id = orders.user_id"));
    assert_eq!(
        explained.rows[0][5],
        text("users.id = orders.user_id AND orders.total > 15")
    );
    let where_sql = "SELECT u.name FROM users u JOIN orders o
        ON u.id = o.user_id WHERE o.total > 15;";
    let filtered = query(&mut database, &format!("EXPLAIN {where_sql}"));
    assert_eq!(filtered.rows[0][5], Value::Null);
    assert_eq!(filtered.rows[0][6], text("o.total > 15"));
    assert_eq!(
        rows(&mut database, where_sql),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
}

#[test]
fn or_and_a_missing_index_stay_nested_loops() {
    let (_dir, path) = seeded("fallback");
    let or_sql = "SELECT users.name, orders.id
        FROM users JOIN orders
        ON users.id = orders.user_id OR orders.user_id IS NULL;";
    let mut database = Database::open(&path).unwrap();
    let nested = rows(&mut database, or_sql);
    let equal = rows(&mut database, EQUAL);
    drop(database);
    let mut database = migrate_and_index(&path, &["CREATE INDEX idx_orders_id ON orders(id);"]);
    assert_eq!(rows(&mut database, EQUAL), equal);
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("NestedLoopJoin"));
    assert_eq!(rows(&mut database, or_sql), nested);
    let or_plan = query(&mut database, &format!("EXPLAIN {or_sql}"));
    assert_eq!(or_plan.rows[0][0], text("NestedLoopJoin"));
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let or_plan = query(&mut database, &format!("EXPLAIN {or_sql}"));
    assert_eq!(or_plan.rows[0][0], text("NestedLoopJoin"));
    assert_eq!(rows(&mut database, or_sql), nested);
}

#[test]
fn several_indexes_prefer_catalog_order() {
    let (_dir, path) = seeded("several");
    let mut database = migrate_and_index(
        &path,
        &[
            "CREATE INDEX idx_orders_id ON orders(id);",
            "CREATE INDEX idx_orders_user ON orders(user_id);",
        ],
    );
    let user = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(user.rows[0][3], text("idx_orders_user"));
    let both = "SELECT users.name, orders.id
        FROM users JOIN orders
        ON users.id = orders.id AND users.id = orders.user_id;";
    assert!(rows(&mut database, both).is_empty());
    let explained = query(&mut database, &format!("EXPLAIN {both}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_id"));
    assert_eq!(
        explained.rows[0][5],
        text("users.id = orders.id AND users.id = orders.user_id")
    );
}

#[test]
fn aliases_and_a_self_join_use_the_inner_index() {
    let (_dir, path) = seeded("alias");
    let mut database = migrate_and_index(
        &path,
        &[
            "CREATE INDEX idx_orders_user ON orders(user_id);",
            "CREATE INDEX idx_users_id ON users(id);",
        ],
    );
    let aliased = "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id;";
    assert_eq!(
        rows(&mut database, aliased)
            .into_iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        vec![
            text("Ada"),
            text("Ada"),
            text("Bea"),
            text("Dee"),
            text("Dee"),
        ]
    );
    let explained = query(&mut database, &format!("EXPLAIN {aliased}"));
    assert_eq!(explained.rows[0][1], text("u (users)"));
    assert_eq!(explained.rows[0][2], text("o (orders)"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
    assert_eq!(explained.rows[0][4], text("u.id = o.user_id"));
    let hidden = failure(
        &mut database,
        "SELECT users.id FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(hidden, "table 'users' is not in scope");
    let unqualified = rows(
        &mut database,
        "SELECT users.name, orders.id FROM users JOIN orders ON users.id = user_id;",
    );
    assert_eq!(unqualified.len(), 5);

    let self_sql = "SELECT a.name, b.name FROM users a JOIN users b ON a.id = b.id;";
    let pairs = rows(&mut database, self_sql);
    assert_eq!(
        pairs,
        vec![
            vec![text("Ada"), text("Ada")],
            vec![text("Ada"), text("Dee")],
            vec![text("Bea"), text("Bea")],
            vec![text("Dee"), text("Ada")],
            vec![text("Dee"), text("Dee")],
        ]
    );
    let self_plan = query(&mut database, &format!("EXPLAIN {self_sql}"));
    assert_eq!(self_plan.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(self_plan.rows[0][3], text("idx_users_id"));
    let single = query(
        &mut database,
        "EXPLAIN SELECT name FROM users WHERE id = 1;",
    );
    assert_eq!(single.rows[0][0], text("IndexScan"));
    assert_eq!(single.rows[0][2], text("id = 1"));
}

#[test]
fn empty_tables_keep_their_columns() {
    let (_dir, path) = temporary("empty");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, total INT);")
            .unwrap();
    }
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    let result = query(
        &mut database,
        "SELECT * FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(
        result.columns,
        vec![
            "id".to_string(),
            "name".to_string(),
            "user_id".to_string(),
            "total".to_string()
        ]
    );
    assert!(result.rows.is_empty());
    let explained = query(
        &mut database,
        "EXPLAIN SELECT name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    let typed = failure(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.name = orders.total LIMIT 0;",
    );
    assert_eq!(typed, "cannot apply operator '=' to TEXT and INTEGER");
}

#[test]
fn insert_and_delete_update_the_index_probe() {
    let (_dir, path) = seeded("mutate");
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    database
        .execute("INSERT INTO orders VALUES (16, 1, 4);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (3, 'Eve');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (17, 3, 8);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.id FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(11)],
            vec![text("Ada"), integer(16)],
            vec![text("Bea"), integer(12)],
            vec![text("Dee"), integer(10)],
            vec![text("Dee"), integer(11)],
            vec![text("Dee"), integer(16)],
            vec![text("Eve"), integer(17)],
        ]
    );
    database
        .execute("DELETE FROM orders WHERE id = 11;")
        .unwrap();
    database
        .execute("DELETE FROM users WHERE name = 'Bea';")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.id FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(16)],
            vec![text("Dee"), integer(10)],
            vec![text("Dee"), integer(16)],
            vec![text("Eve"), integer(17)],
        ]
    );
}

#[test]
fn restart_keeps_the_indexed_join() {
    let (_dir, path) = seeded("restart");
    {
        let mut database =
            migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
        assert_eq!(rows(&mut database, EQUAL), equal_pairs());
    }
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, EQUAL), equal_pairs());
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
}

#[test]
fn order_by_and_limit_match_the_nested_loop() {
    let (_dir, path) = seeded("sort");
    let ordered = "SELECT users.name, orders.total
        FROM users JOIN orders ON users.id = orders.user_id
        ORDER BY orders.total;";
    let limited = "SELECT users.name, orders.id
        FROM users JOIN orders ON users.id = orders.user_id
        LIMIT 2;";
    let mut database = Database::open(&path).unwrap();
    let ordered_rows = rows(&mut database, ordered);
    let limited_rows = rows(&mut database, limited);
    let none = query(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id LIMIT 0;",
    );
    drop(database);
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(
        ordered_rows,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Dee"), integer(10)],
            vec![text("Ada"), integer(30)],
            vec![text("Dee"), integer(30)],
            vec![text("Bea"), Value::Null],
        ]
    );
    assert_eq!(rows(&mut database, ordered), ordered_rows);
    assert_eq!(
        limited_rows,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(11)],
        ]
    );
    assert_eq!(rows(&mut database, limited), limited_rows);
    let zero = query(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id LIMIT 0;",
    );
    assert_eq!(zero.columns, none.columns);
    assert!(zero.rows.is_empty());
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id LIMIT 100;",
        )
        .len(),
        5
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.total
                FROM users JOIN orders ON users.id = orders.user_id
                ORDER BY orders.total LIMIT 1;",
        ),
        vec![vec![text("Ada"), integer(10)]]
    );
}

#[test]
fn explain_does_not_run_the_join_or_write() {
    let (_dir, path) = seeded("explain");
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    database
        .execute("INSERT INTO users VALUES (8, 'Ivy');")
        .unwrap();
    drop(database);
    let before = std::fs::metadata(&path).unwrap().len();
    let mut wal_name = path.as_os_str().to_owned();
    wal_name.push(".wal");
    let wal = std::path::PathBuf::from(wal_name);
    assert_eq!(wal.metadata().map(|meta| meta.len()).unwrap_or(0), 0);

    let mut database = Database::open(&path).unwrap();
    let before_rows = rows(&mut database, "SELECT name FROM users;");
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users AS u JOIN orders o ON u.id = o.user_id WHERE o.total > 15;",
    );
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][6], text("o.total > 15"));
    assert_eq!(rows(&mut database, "SELECT name FROM users;"), before_rows);
    let ambiguous = failure(
        &mut database,
        "EXPLAIN SELECT id FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(ambiguous, "column 'id' is ambiguous");
    assert_eq!(rows(&mut database, EQUAL).len(), 5);
    drop(database);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(wal.metadata().map(|meta| meta.len()).unwrap_or(0), 0);
}

#[test]
fn indexed_joins_match_a_reference_model() {
    let (_dir, path) = temporary("random");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    let mut state = 0x7eed_u64;
    let mut users = Vec::new();
    let mut orders = Vec::new();
    for n in 0..40_u64 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let id = if n.is_multiple_of(11) {
            None
        } else {
            Some((state % 13) as i64)
        };
        let name = format!("u{n}");
        match id {
            Some(id) => database
                .execute(&format!("INSERT INTO users VALUES ({id}, '{name}');"))
                .unwrap(),
            None => database
                .execute(&format!("INSERT INTO users VALUES (NULL, '{name}');"))
                .unwrap(),
        };
        users.push((id, name));
    }
    for n in 0..50_u64 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let user_id = if n.is_multiple_of(9) {
            None
        } else {
            Some((state % 13) as i64)
        };
        let total = n as i64;
        match user_id {
            Some(user_id) => database
                .execute(&format!("INSERT INTO orders VALUES ({user_id}, {total});"))
                .unwrap(),
            None => database
                .execute(&format!("INSERT INTO orders VALUES (NULL, {total});"))
                .unwrap(),
        };
        orders.push((user_id, total));
    }
    let equality = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true)
    });
    let reversed = reference(&users, &orders, |user, order| {
        sql_eq(order.0, user.0) == Some(true)
    });
    let residual = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true) && order.1 > 20
    });
    let either = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true) || order.0.is_none()
    });
    let equality_sql =
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;";
    let reversed_sql =
        "SELECT users.name, orders.total FROM users JOIN orders ON orders.user_id = users.id;";
    let residual_sql = "SELECT users.name, orders.total
        FROM users JOIN orders ON users.id = orders.user_id AND orders.total > 20;";
    let either_sql = "SELECT users.name, orders.total
        FROM users JOIN orders ON users.id = orders.user_id OR orders.user_id IS NULL;";
    assert_eq!(rows(&mut database, equality_sql), equality);
    assert_eq!(rows(&mut database, reversed_sql), reversed);
    assert_eq!(rows(&mut database, residual_sql), residual);
    assert_eq!(rows(&mut database, either_sql), either);
    drop(database);

    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(rows(&mut database, equality_sql), equality);
    assert_eq!(rows(&mut database, reversed_sql), reversed);
    assert_eq!(rows(&mut database, residual_sql), residual);
    assert_eq!(rows(&mut database, either_sql), either);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {equality_sql}")).rows[0][0],
        text("IndexNestedLoopJoin")
    );
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {either_sql}")).rows[0][0],
        text("NestedLoopJoin")
    );
}

#[test]
fn duplicate_keys_span_leaves_and_match_a_reference() {
    let (_dir, path) = temporary("fanout");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (NULL, 'Cy'), (8, 'Bea');")
        .unwrap();
    let mut orders = Vec::new();
    let mut batch = String::new();
    for total in 1..=300 {
        if batch.is_empty() {
            batch.push_str("INSERT INTO orders VALUES ");
        } else {
            batch.push_str(", ");
        }
        batch.push_str(&format!("(1, {total})"));
        orders.push((Some(1), total));
        if total % 50 == 0 {
            batch.push(';');
            database.execute(&batch).unwrap();
            batch.clear();
        }
    }
    database
        .execute("INSERT INTO orders VALUES (NULL, 0), (8, 900);")
        .unwrap();
    orders.push((None, 0));
    orders.push((Some(8), 900));
    let users = vec![
        (Some(1), "Ada".to_string()),
        (None, "Cy".to_string()),
        (Some(8), "Bea".to_string()),
    ];
    let expected = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true)
    });
    assert_eq!(expected.len(), 301);
    let sql =
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;";
    assert_eq!(rows(&mut database, sql), expected);
    drop(database);
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    assert_eq!(rows(&mut database, sql), expected);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
}

#[test]
fn multi_row_changes_and_reopen_match_a_reference() {
    let (_dir, path) = temporary("rewrite");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 11), (2, 20);")
        .unwrap();
    drop(database);
    let mut database =
        migrate_and_index(&path, &["CREATE INDEX idx_orders_user ON orders(user_id);"]);
    let users = vec![(Some(1), "Ada".to_string()), (Some(2), "Bea".to_string())];
    let mut orders = vec![(Some(1), 10), (Some(1), 11), (Some(2), 20)];
    let sql =
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;";
    database
        .execute("INSERT INTO orders VALUES (1, 12), (1, 13), (NULL, 14), (2, 21);")
        .unwrap();
    orders.extend([(Some(1), 12), (Some(1), 13), (None, 14), (Some(2), 21)]);
    let expected = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true)
    });
    assert_eq!(rows(&mut database, sql), expected);
    database
        .execute("UPDATE orders SET user_id = 2 WHERE user_id = 1;")
        .unwrap();
    for order in &mut orders {
        if order.0 == Some(1) {
            order.0 = Some(2);
        }
    }
    let expected = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true)
    });
    assert_eq!(rows(&mut database, sql), expected);
    database
        .execute("DELETE FROM orders WHERE user_id = 2;")
        .unwrap();
    orders.retain(|order| order.0 != Some(2));
    let expected = reference(&users, &orders, |user, order| {
        sql_eq(user.0, order.0) == Some(true)
    });
    assert_eq!(expected, Vec::<Vec<Value>>::new());
    assert_eq!(rows(&mut database, sql), expected);
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, sql), expected);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {sql}")).rows[0][0],
        text("IndexNestedLoopJoin")
    );
}

fn reference(
    users: &[(Option<i64>, String)],
    orders: &[(Option<i64>, i64)],
    keep: impl Fn(&(Option<i64>, String), &(Option<i64>, i64)) -> bool,
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for user in users {
        for order in orders {
            if keep(user, order) {
                rows.push(vec![text(&user.1), integer(order.1)]);
            }
        }
    }
    rows
}

fn sql_eq(left: Option<i64>, right: Option<i64>) -> Option<bool> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left == right),
        _ => None,
    }
}

#[test]
#[ignore = "release profile of index nested-loop join"]
fn profile_index_nested_loop_join() {
    let cpu = command_text("sysctl", &["-n", "machdep.cpu.brand_string"]);
    let memory = command_text("sysctl", &["-n", "hw.memsize"]);
    let rustc = command_text("rustc", &["--version"]);
    println!("join_index cpu={cpu} mem_bytes={memory} rustc={rustc}");
    println!(
        "join_index value_bytes={} vec_bytes={}",
        std::mem::size_of::<Value>(),
        std::mem::size_of::<Vec<Value>>()
    );
    let sql = "SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id;";
    for rows in [100_i64, 1_000, 4_000, 10_000] {
        measure_shape("identity", rows, rows, sql, |database| {
            load_identity(database, "lhs", rows);
            load_identity(database, "rhs", rows);
        });
        measure_shape("duplicate", rows, 1_000.min(rows) * 20, sql, |database| {
            load_identity(database, "lhs", rows);
            load_mod(database, rows, 50);
        });
    }
}

fn measure_shape(
    shape: &str,
    rows: i64,
    expected_floor: i64,
    sql: &str,
    load: impl FnOnce(&mut Database),
) {
    let (_dir, path) = temporary(shape);
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE lhs (id INT, note INT);")
        .unwrap();
    database
        .execute("CREATE TABLE rhs (id INT, note INT);")
        .unwrap();
    load(&mut database);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    let nested = measure_join(&mut database, sql);
    database
        .execute("CREATE INDEX idx_rhs_id ON rhs(id);")
        .unwrap();
    let indexed = measure_join(&mut database, sql);
    assert_eq!(nested.matches, indexed.matches);
    assert_eq!(nested.sum, indexed.sum);
    assert!(indexed.matches as i64 >= expected_floor.min(rows));
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    let pairs = rows as u128 * rows as u128;
    let speedup = if indexed.warm_us == 0 {
        0.0
    } else {
        nested.warm_us as f64 / indexed.warm_us as f64
    };
    println!(
        "join_index shape={shape} n={rows} pairs={pairs} matches={} nested_cold_us={} nested_warm_us={} index_cold_us={} index_warm_us={} speedup={speedup:.2} result_bytes={}",
        indexed.matches,
        nested.cold_us,
        nested.warm_us,
        indexed.cold_us,
        indexed.warm_us,
        indexed.result_bytes
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

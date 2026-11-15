//! In-memory hash `INNER JOIN` when no usable inner index exists.

use rustdb::{Database, StatementResult, Value};

#[path = "support/temp_dir.rs"]
mod temp_dir;

fn temporary(label: &str) -> (temp_dir::KeepDir, std::path::PathBuf) {
    let dir = temp_dir::KeepDir::new(label);
    let path = dir.join("db");
    (dir, path)
}

fn query(database: &mut Database, sql: &str) -> rustdb::QueryResult {
    let results = database.execute(sql).expect(sql);
    let StatementResult::Query(result) = &results[0] else {
        panic!("expected a query");
    };
    result.clone()
}

fn rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    query(database, sql).rows
}

fn failure(database: &mut Database, sql: &str) -> String {
    database.execute(sql).unwrap_err().to_string()
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

fn reference(
    outer: &[Vec<Value>],
    inner: &[Vec<Value>],
    keep: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for left in outer {
        for right in inner {
            if keep(left, right) {
                let mut row = left.clone();
                row.extend(right.iter().cloned());
                rows.push(row);
            }
        }
    }
    rows
}

fn eq_cell(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        _ => false,
    }
}

#[test]
fn scalar_equalities_match_a_nested_loop_reference() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, label TEXT, paid BOOLEAN, total INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES
                (1, 'Ada', TRUE),
                (NULL, 'Cy', FALSE),
                (1, 'Dee', TRUE),
                (2, 'Bea', FALSE);",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (1, 'Ada', TRUE, 10),
                (1, 'Ada', FALSE, 30),
                (NULL, 'Cy', TRUE, 5),
                (2, 'Bea', FALSE, NULL);",
        )
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada"), boolean(true)],
        vec![Value::Null, text("Cy"), boolean(false)],
        vec![integer(1), text("Dee"), boolean(true)],
        vec![integer(2), text("Bea"), boolean(false)],
    ];
    let orders = vec![
        vec![integer(1), text("Ada"), boolean(true), integer(10)],
        vec![integer(1), text("Ada"), boolean(false), integer(30)],
        vec![Value::Null, text("Cy"), boolean(true), integer(5)],
        vec![integer(2), text("Bea"), boolean(false), Value::Null],
    ];
    let by_id = "SELECT * FROM users JOIN orders ON users.id = orders.user_id;";
    let by_name = "SELECT * FROM users JOIN orders ON users.name = orders.label;";
    let by_flag = "SELECT * FROM users JOIN orders ON users.active = orders.paid;";
    let reversed = "SELECT * FROM users JOIN orders ON orders.user_id = users.id;";
    assert_eq!(
        rows(&mut database, by_id),
        reference(&users, &orders, |user, order| eq_cell(&user[0], &order[0]))
    );
    assert_eq!(
        rows(&mut database, by_name),
        reference(&users, &orders, |user, order| eq_cell(&user[1], &order[1]))
    );
    assert_eq!(
        rows(&mut database, by_flag),
        reference(&users, &orders, |user, order| eq_cell(&user[2], &order[2]))
    );
    assert_eq!(rows(&mut database, reversed), rows(&mut database, by_id));
    let explained = query(&mut database, &format!("EXPLAIN {by_name}"));
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][3], text("orders"));
    assert_eq!(explained.rows[0][4], text("users.name = orders.label"));
    assert_eq!(explained.rows[0][5], Value::Null);
    let nulls = rows(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id IS NULL;",
    );
    assert_eq!(
        nulls,
        vec![
            vec![text("Cy")],
            vec![text("Cy")],
            vec![text("Cy")],
            vec![text("Cy")]
        ]
    );
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id IS NULL;"
        )
        .rows[0][0],
        text("NestedLoopJoin")
    );
}

#[test]
fn duplicates_keep_outer_then_inner_order_when_the_outer_table_is_smaller() {
    let mut database = Database::new();
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
        .execute("INSERT INTO orders VALUES (2, 10), (2, 11), (1, 12);")
        .unwrap();
    let sql =
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;";
    assert_eq!(
        rows(&mut database, sql),
        vec![
            vec![text("Ada"), integer(12)],
            vec![text("Bea"), integer(10)],
            vec![text("Bea"), integer(11)],
        ]
    );
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.columns[3], "build".to_string());
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][3], text("users"));
    database
        .execute("INSERT INTO users VALUES (3, 'Cy'), (4, 'Dee');")
        .unwrap();
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {sql}")).rows[0][3],
        text("orders")
    );
    assert_eq!(
        rows(&mut database, sql),
        vec![
            vec![text("Ada"), integer(12)],
            vec![text("Bea"), integer(10)],
            vec![text("Bea"), integer(11)],
        ]
    );
}

#[test]
fn residual_where_order_and_limit_keep_nested_loop_results() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (1, 'Dee');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 30), (2, 5), (9, 7);")
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Bea")],
        vec![integer(1), text("Dee")],
    ];
    let orders = vec![
        vec![integer(1), integer(10)],
        vec![integer(1), integer(30)],
        vec![integer(2), integer(5)],
        vec![integer(9), integer(7)],
    ];
    let residual = "SELECT users.name, orders.total
        FROM users JOIN orders
        ON users.id = orders.user_id AND orders.total > 15;";
    let filtered = "SELECT u.name, o.total
        FROM users u JOIN orders o
        ON u.id = o.user_id
        WHERE o.total > 15;";
    let ordered = "SELECT users.name, orders.total
        FROM users JOIN orders
        ON users.id = orders.user_id
        ORDER BY orders.total, users.name;";
    let limited = "SELECT users.name, orders.total
        FROM users JOIN orders
        ON users.id = orders.user_id
        LIMIT 2;";
    let expected_residual = reference(&users, &orders, |user, order| {
        eq_cell(&user[0], &order[0]) && matches!(order[1], Value::Integer(total) if total > 15)
    });
    assert_eq!(
        rows(&mut database, residual),
        expected_residual
            .into_iter()
            .map(|row| vec![row[1].clone(), row[3].clone()])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        rows(&mut database, filtered),
        vec![
            vec![text("Ada"), integer(30)],
            vec![text("Dee"), integer(30)],
        ]
    );
    let explained = query(&mut database, &format!("EXPLAIN {residual}"));
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][4], text("users.id = orders.user_id"));
    assert_eq!(
        explained.rows[0][5],
        text("users.id = orders.user_id AND orders.total > 15")
    );
    assert_eq!(
        rows(&mut database, ordered),
        vec![
            vec![text("Bea"), integer(5)],
            vec![text("Ada"), integer(10)],
            vec![text("Dee"), integer(10)],
            vec![text("Ada"), integer(30)],
            vec![text("Dee"), integer(30)],
        ]
    );
    assert_eq!(
        rows(&mut database, limited),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(30)],
        ]
    );
    let none = query(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id LIMIT 0;",
    );
    assert!(none.rows.is_empty());
    assert_eq!(none.columns, vec!["name".to_string()]);
}

#[test]
fn or_and_ranges_stay_nested_loops() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (NULL, 'Cy');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (NULL, 4);")
        .unwrap();
    let either = "SELECT users.name, orders.total
        FROM users JOIN orders
        ON users.id = orders.user_id OR orders.user_id IS NULL;";
    assert_eq!(
        rows(&mut database, either),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(4)],
            vec![text("Cy"), integer(4)],
        ]
    );
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {either}")).rows[0][0],
        text("NestedLoopJoin")
    );
    let range = "SELECT users.name FROM users JOIN orders ON users.id > orders.user_id;";
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {range}")).rows[0][0],
        text("NestedLoopJoin")
    );
    assert!(rows(&mut database, range).is_empty());
}

#[test]
fn aliases_self_joins_and_empty_tables_keep_their_shape() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, total INT);")
        .unwrap();
    let empty = query(
        &mut database,
        "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns, vec!["name".to_string(), "total".to_string()]);
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;"
        )
        .rows[0][3],
        text("orders")
    );
    let typed = failure(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.name = orders.user_id LIMIT 0;",
    );
    assert_eq!(typed, "cannot apply operator '=' to TEXT and INTEGER");
    let filtered = failure(
        &mut database,
        "SELECT users.name FROM users JOIN orders ON users.id = orders.user_id WHERE users.name = orders.total;",
    );
    assert_eq!(filtered, "cannot apply operator '=' to TEXT and INTEGER");
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(10)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT a.name, b.id FROM users a JOIN users b ON a.name = b.name;"
        ),
        vec![
            vec![text("Ada"), integer(1)],
            vec![text("Ada"), integer(1)],
            vec![text("Ada"), integer(1)],
            vec![text("Ada"), integer(1)],
            vec![text("Bea"), integer(2)],
        ]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT a.name FROM users a JOIN users b ON a.name = b.name;",
    );
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][1], text("a (users)"));
    assert_eq!(explained.rows[0][2], text("b (users)"));
    assert_eq!(explained.rows[0][3], text("b (users)"));
}

#[test]
fn an_inner_index_wins_and_a_missing_index_still_hashes() {
    let (_dir, path) = temporary("index");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (10, 1), (11, 1), (12, 2);")
        .unwrap();
    let sql = "SELECT users.name, orders.id FROM users JOIN orders ON users.id = orders.user_id;";
    let before = rows(&mut database, sql);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_id ON orders(id);")
        .unwrap();
    assert_eq!(rows(&mut database, sql), before);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {sql}")).rows[0][0],
        text("HashJoin")
    );
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    assert_eq!(rows(&mut database, sql), before);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][3], text("users"));
    for n in 0..126 {
        database
            .execute(&format!("INSERT INTO orders VALUES ({}, 1);", 100 + n))
            .unwrap();
    }
    let wide = rows(&mut database, sql);
    assert_eq!(wide.len(), before.len() + 126);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][3], text("idx_orders_user"));
}

#[test]
fn restart_keeps_the_hash_join_and_explain_does_not_write() {
    let (_dir, path) = temporary("restart");
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
        .execute("INSERT INTO orders VALUES (1, 10), (2, 4);")
        .unwrap();
    drop(database);
    let before = std::fs::metadata(&path).unwrap().len();
    let mut database = Database::open(&path).unwrap();
    let expected = vec![
        vec![text("Ada"), integer(10)],
        vec![text("Bea"), integer(4)],
    ];
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        expected
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    drop(database);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        expected
    );
    database
        .execute("CREATE INDEX idx_orders_total ON orders (total);")
        .unwrap();
    let indexed = query(
        &mut database,
        "EXPLAIN SELECT total FROM orders WHERE total = 10;",
    );
    assert_eq!(indexed.rows[0][0], text("IndexScan"));
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT users.name FROM users JOIN orders ON users.id = orders.user_id;"
        )
        .rows[0][0],
        text("HashJoin")
    );
    database
        .execute("UPDATE orders SET total = 11 WHERE total = 10;")
        .unwrap();
    database
        .execute("DELETE FROM orders WHERE user_id = 2;")
        .unwrap();
    let updated = vec![vec![text("Ada"), integer(11)]];
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        updated
    );
    assert_eq!(
        rows(&mut database, "SELECT total FROM orders WHERE total = 11;"),
        vec![vec![integer(11)]]
    );
    drop(database);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT users.name, orders.total FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        updated
    );
    assert_eq!(
        rows(&mut database, "SELECT total FROM orders WHERE total = 11;"),
        vec![vec![integer(11)]]
    );
}

#[test]
fn randomized_hash_joins_match_a_reference_model() {
    for (seed, user_count, order_count) in [
        (0x9a51_u64, 40, 50),
        (0x51c0_u64, 60, 20),
        (0x0e11_u64, 30, 30),
        (0xc0ff_u64, 12, 80),
    ] {
        check_random_join(seed, user_count, order_count);
    }
}

fn check_random_join(seed: u64, user_count: u64, order_count: u64) {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, label TEXT, paid BOOLEAN);")
        .unwrap();
    let mut state = seed;
    let mut users = Vec::new();
    let mut orders = Vec::new();
    for n in 0..user_count {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let id = if n.is_multiple_of(7) {
            None
        } else {
            Some((state % 11) as i64)
        };
        let name = if n.is_multiple_of(5) {
            "same".to_string()
        } else {
            format!("u{n}")
        };
        let active = n.is_multiple_of(3);
        let active_sql = if active { "TRUE" } else { "FALSE" };
        if let Some(id) = id {
            database
                .execute(&format!(
                    "INSERT INTO users VALUES ({id}, '{name}', {active_sql});"
                ))
                .unwrap();
        } else {
            database
                .execute(&format!(
                    "INSERT INTO users VALUES (NULL, '{name}', {active_sql});"
                ))
                .unwrap();
        }
        users.push(vec![
            id.map(integer).unwrap_or(Value::Null),
            text(&name),
            boolean(active),
        ]);
    }
    for n in 0..order_count {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let user_id = if n.is_multiple_of(6) {
            None
        } else {
            Some((state % 11) as i64)
        };
        let label = if n.is_multiple_of(4) {
            "same".to_string()
        } else {
            format!("o{n}")
        };
        let paid = n.is_multiple_of(2);
        let paid_sql = if paid { "TRUE" } else { "FALSE" };
        if let Some(user_id) = user_id {
            database
                .execute(&format!(
                    "INSERT INTO orders VALUES ({user_id}, '{label}', {paid_sql});"
                ))
                .unwrap();
        } else {
            database
                .execute(&format!(
                    "INSERT INTO orders VALUES (NULL, '{label}', {paid_sql});"
                ))
                .unwrap();
        }
        orders.push(vec![
            user_id.map(integer).unwrap_or(Value::Null),
            text(&label),
            boolean(paid),
        ]);
    }
    let by_id = reference(&users, &orders, |user, order| eq_cell(&user[0], &order[0]));
    let by_name = reference(&users, &orders, |user, order| eq_cell(&user[1], &order[1]));
    let by_flag = reference(&users, &orders, |user, order| eq_cell(&user[2], &order[2]));
    let residual = reference(&users, &orders, |user, order| {
        eq_cell(&user[0], &order[0]) && matches!(order[2], Value::Boolean(true))
    });
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON users.id = orders.user_id;"
        ),
        by_id
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON orders.label = users.name;"
        ),
        by_name
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON users.active = orders.paid;"
        ),
        by_flag
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders
                ON users.id = orders.user_id AND orders.paid = TRUE;"
        ),
        residual
    );
    let mut limited = by_id.clone();
    limited.truncate(3);
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON users.id = orders.user_id LIMIT 3;"
        ),
        limited
    );
    let active = reference(&users, &orders, |user, order| {
        eq_cell(&user[2], &order[2]) && matches!(user[2], Value::Boolean(true))
    });
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON users.active = orders.paid WHERE users.active = TRUE;"
        ),
        active
    );
    let mut ordered = by_name.clone();
    ordered.sort_by(|left, right| {
        text_of(&left[1])
            .cmp(text_of(&right[1]))
            .then_with(|| text_of(&left[4]).cmp(text_of(&right[4])))
    });
    assert_eq!(
        rows(
            &mut database,
            "SELECT * FROM users JOIN orders ON orders.label = users.name
                ORDER BY users.name, orders.label;"
        ),
        ordered
    );
    let mut self_rows = Vec::new();
    for left in &users {
        for right in &users {
            if eq_cell(&left[1], &right[1]) {
                self_rows.push(vec![left[1].clone(), right[1].clone()]);
            }
        }
    }
    assert_eq!(
        rows(
            &mut database,
            "SELECT a.name, b.name FROM users a JOIN users b ON a.name = b.name;"
        ),
        self_rows
    );
    let build = if order_count <= user_count {
        "orders"
    } else {
        "users"
    };
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT * FROM users JOIN orders ON users.id = orders.user_id;"
        )
        .rows[0][3],
        text(build)
    );
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT * FROM users JOIN orders
                ON users.id = orders.user_id OR orders.paid = TRUE;"
        )
        .rows[0][0],
        text("NestedLoopJoin")
    );
}

fn text_of(value: &Value) -> &str {
    match value {
        Value::Text(text) => text,
        _ => "",
    }
}

//! Index nested-loop `LEFT JOIN`, checked against an independent walker.
//!
//! The walker does not use the B+ tree, the join visitor, or the hash table.

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

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn step(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn eq_true(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => *left == *right,
        _ => false,
    }
}

/// Left rows, then right rows. `ON` true emits the pair. Otherwise one null
/// extension. `WHERE` runs after that choice.
fn logical_rows(
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
    right_width: usize,
    on: impl Fn(&[Value], &[Value]) -> bool,
    keep: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let nulls = vec![Value::Null; right_width];
    for left in left_rows {
        let mut matched = false;
        for right in right_rows {
            if !on(left, right) {
                continue;
            }
            matched = true;
            if keep(left, right) {
                let mut row = left.to_vec();
                row.extend(right.iter().cloned());
                out.push(row);
            }
        }
        if !matched && keep(left, &nulls) {
            let mut row = left.to_vec();
            row.extend(nulls.iter().cloned());
            out.push(row);
        }
    }
    out
}

fn project(rows: &[Vec<Value>], indexes: &[usize]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| indexes.iter().map(|index| row[*index].clone()).collect())
        .collect()
}

fn distinct_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for row in rows {
        if !out.contains(&row) {
            out.push(row);
        }
    }
    out
}

fn insert(database: &mut Database, table: &str, rows: &[Vec<Value>]) {
    if rows.is_empty() {
        return;
    }
    let mut sql = format!("INSERT INTO {table} VALUES ");
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
                Value::Text(value) => sql.push_str(&format!("'{value}'")),
                Value::Boolean(value) => sql.push_str(if *value { "TRUE" } else { "FALSE" }),
                Value::Ratio { .. } => panic!("the walker does not insert ratios"),
            }
        }
        sql.push(')');
    }
    sql.push(';');
    database.execute(&sql).unwrap();
}

fn open_indexed(
    label: &str,
    users: &[Vec<Value>],
    orders: &[Vec<Value>],
) -> (temp_dir::KeepDir, Database) {
    let (dir, path) = temporary(label);
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        insert(&mut database, "users", users);
        insert(&mut database, "orders", orders);
    }
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    (dir, database)
}

fn expected_plan(users: usize, orders: usize) -> &'static str {
    if users == 0 || orders > users.saturating_mul(64) {
        "IndexLeftNestedLoopJoin"
    } else {
        "HashLeftJoin"
    }
}

#[test]
fn the_size_rule_chooses_the_index_without_changing_rows() {
    let users = vec![vec![integer(1), text("Ada")]];
    let small = (0..64)
        .map(|id| vec![integer(1), integer(id)])
        .collect::<Vec<_>>();
    let (_dir, mut database) = open_indexed("m24-hash-64", &users, &small);
    let sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {sql}")).rows[0][0],
        Value::Text("HashLeftJoin".to_string())
    );
    drop(database);

    let large = (0..65)
        .map(|id| vec![integer(1), integer(id)])
        .collect::<Vec<_>>();
    let (dir, mut database) = open_indexed("m24-index-65", &users, &large);
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(
        explained.rows[0][0],
        Value::Text("IndexLeftNestedLoopJoin".to_string())
    );
    assert_eq!(
        explained.rows[0].last(),
        Some(&Value::Text("idx_orders_user".to_string()))
    );
    assert_eq!(explained.rows[0][1], text("u (users)"));
    assert_eq!(explained.rows[0][2], text("o (orders)"));
    assert_eq!(explained.rows[0][3], Value::Null);
    assert_eq!(explained.rows[0][4], text("u.id = o.user_id"));
    let path = dir.join("db");
    let before = std::fs::read(&path).unwrap();
    let wal = path.with_extension("wal");
    let wal_before = std::fs::read(&wal).unwrap_or_default();
    let got = rows(&mut database, sql);
    let expect = logical_rows(
        &users,
        &large,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    assert_eq!(got, project(&expect, &[1, 3]));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read(&wal).unwrap_or_default(), wal_before);
    database
        .execute("UPDATE orders SET amount = 7 WHERE user_id = 1 AND amount = 0;")
        .unwrap();
    database
        .execute("DELETE FROM orders WHERE amount = 64;")
        .unwrap();
    drop(database);
    let mut database = Database::open(&path).unwrap();
    let mut large = large;
    large[0][1] = integer(7);
    large.pop();
    let expect = logical_rows(
        &users,
        &large,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    assert_eq!(
        rows(&mut database, sql),
        project(&expect, &[1, 3]),
        "reopened index"
    );
}

#[test]
fn seeded_left_index_matches_the_walker() {
    let seeds = [
        0x2424_u64, 0x24a1, 0x24b2, 0x24c3, 0x24d4, 0x24e5, 0x24f6, 0x2407,
    ];
    let sizes = [
        (0, 0),
        (0, 4),
        (4, 0),
        (1, 65),
        (8, 8),
        (3, 12),
        (12, 3),
        (2, 130),
        (130, 2),
    ];
    for seed in seeds {
        for (users, orders) in sizes {
            let mut state = seed;
            let outer = (0..users)
                .map(|_| {
                    let key = match step(&mut state) % 5 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap() - 1),
                    };
                    let name = text(&format!(
                        "{}{}",
                        (b'A' + u8::try_from(step(&mut state) % 3).unwrap()) as char,
                        "xyz".repeat(usize::try_from(step(&mut state) % 3).unwrap())
                    ));
                    vec![key, name]
                })
                .collect::<Vec<_>>();
            let inner = (0..orders)
                .map(|_| {
                    let key = match step(&mut state) % 5 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap() - 1),
                    };
                    let amount = match step(&mut state) % 4 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap()),
                    };
                    vec![key, amount]
                })
                .collect::<Vec<_>>();
            let label = format!("seed={seed:#x} users={users} orders={orders}");
            let (_dir, mut database) =
                open_indexed(&format!("m24-{seed:x}-{users}-{orders}"), &outer, &inner);
            let explained = query(
                &mut database,
                "EXPLAIN SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
            );
            assert_eq!(
                explained.rows[0][0],
                Value::Text(expected_plan(users, orders).to_string()),
                "{label}"
            );
            let logical = logical_rows(
                &outer,
                &inner,
                2,
                |user, order| eq_true(&user[0], &order[0]),
                |_, _| true,
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
                ),
                project(&logical, &[1, 3]),
                "{label}"
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT DISTINCT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
                ),
                distinct_rows(project(&logical, &[1, 3])),
                "{label} distinct"
            );
            let filtered = logical_rows(
                &outer,
                &inner,
                2,
                |user, order| eq_true(&user[0], &order[0]),
                |_, order| matches!(order[1], Value::Integer(amount) if amount > 0),
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 0;",
                ),
                project(&filtered, &[1, 3]),
                "{label} where"
            );
            let residual = logical_rows(
                &outer,
                &inner,
                2,
                |user, order| {
                    eq_true(&user[0], &order[0])
                        && matches!(order[1], Value::Integer(amount) if amount > 0)
                },
                |_, _| true,
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
                ),
                project(&residual, &[1, 3]),
                "{label} residual"
            );
            let or_rows = logical_rows(
                &outer,
                &inner,
                2,
                |user, order| eq_true(&user[0], &order[0]) || matches!(order[1], Value::Integer(1)),
                |_, _| true,
            );
            let or_plan = query(
                &mut database,
                "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 1;",
            );
            assert_eq!(
                or_plan.rows[0][0],
                Value::Text("LeftNestedLoopJoin".to_string()),
                "{label} or"
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 1;",
                ),
                project(&or_rows, &[1, 3]),
                "{label} or rows"
            );
        }
    }
}

#[test]
fn a_self_join_keeps_the_walker_order() {
    let rows_in = vec![
        vec![integer(1), text("Ada")],
        vec![integer(1), text("Ann")],
        vec![Value::Null, text("Dee")],
        vec![integer(2), text("Bea")],
    ];
    let logical = logical_rows(
        &rows_in,
        &rows_in,
        2,
        |left, right| eq_true(&left[0], &right[0]),
        |_, _| true,
    );
    let (dir, path) = temporary("m24-self");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        insert(&mut database, "users", &rows_in);
    }
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_users_id ON users(id);")
        .unwrap();
    assert_eq!(
        query(
            &mut database,
            "EXPLAIN SELECT a.name, b.name FROM users a LEFT JOIN users b ON a.id = b.id;",
        )
        .rows[0][0],
        Value::Text("HashLeftJoin".to_string())
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT a.name, b.name FROM users a LEFT JOIN users b ON a.id = b.id;",
        ),
        project(&logical, &[1, 3])
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT a.name, b.name FROM users a LEFT JOIN users b ON a.id = b.id;",
        ),
        distinct_rows(project(&logical, &[1, 3]))
    );
    let _ = dir;
}

fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

fn access(database: &mut Database, sql: &str) -> String {
    match &query(database, &format!("EXPLAIN {sql}")).rows[0][0] {
        Value::Text(text) => text.clone(),
        other => panic!("access cell was {other:?}"),
    }
}

#[test]
fn text_and_boolean_equalities_stay_hashed_with_an_integer_index() {
    let users = vec![
        vec![integer(1), text("Ada"), boolean(true)],
        vec![integer(2), text("Bea"), boolean(false)],
        vec![Value::Null, text("Ada"), boolean(true)],
    ];
    let orders = vec![
        vec![integer(1), text("Ada"), boolean(true), integer(10)],
        vec![integer(1), text("No"), boolean(false), integer(11)],
        vec![integer(2), text("Bea"), boolean(false), integer(20)],
        vec![integer(9), text("Ada"), boolean(true), integer(30)],
        vec![Value::Null, text("Ada"), Value::Null, integer(5)],
    ];
    let (dir, path) = temporary("m24-scalar");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, label TEXT, paid BOOLEAN, amount INT);")
            .unwrap();
        insert(&mut database, "users", &users);
        insert(&mut database, "orders", &orders);
        let text_sql =
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.name = o.label;";
        let flag_sql =
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.active = o.paid;";
        let id_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
        assert_eq!(access(&mut database, text_sql), "HashLeftJoin");
        assert_eq!(access(&mut database, flag_sql), "HashLeftJoin");
        assert_eq!(access(&mut database, id_sql), "HashLeftJoin");
        assert_eq!(
            rows(&mut database, text_sql),
            project(
                &logical_rows(
                    &users,
                    &orders,
                    4,
                    |user, order| eq_true(&user[1], &order[1]),
                    |_, _| true,
                ),
                &[1, 6]
            )
        );
        assert_eq!(
            rows(&mut database, flag_sql),
            project(
                &logical_rows(
                    &users,
                    &orders,
                    4,
                    |user, order| eq_true(&user[2], &order[2]),
                    |_, _| true,
                ),
                &[1, 6]
            )
        );
    }
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_amount ON orders(amount);")
        .unwrap();
    let text_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.name = o.label;";
    let flag_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.active = o.paid;";
    let id_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
    let explained = query(&mut database, &format!("EXPLAIN {text_sql}"));
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_eq!(explained.rows[0][3], text("o (orders)"));
    assert_eq!(explained.rows[0][4], text("u.name = o.label"));
    assert_eq!(explained.columns.len(), 10);
    assert_eq!(access(&mut database, flag_sql), "HashLeftJoin");
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {flag_sql}")).rows[0][4],
        text("u.active = o.paid")
    );
    assert_eq!(access(&mut database, id_sql), "HashLeftJoin");
    assert_eq!(
        rows(&mut database, text_sql),
        project(
            &logical_rows(
                &users,
                &orders,
                4,
                |user, order| eq_true(&user[1], &order[1]),
                |_, _| true,
            ),
            &[1, 6]
        )
    );
    assert_eq!(
        rows(&mut database, flag_sql),
        project(
            &logical_rows(
                &users,
                &orders,
                4,
                |user, order| eq_true(&user[2], &order[2]),
                |_, _| true,
            ),
            &[1, 6]
        )
    );
    let _ = dir;
}

#[test]
fn an_integer_index_is_used_only_for_its_own_equality() {
    let users = vec![vec![integer(1), text("Ada"), boolean(true)]];
    let orders = (0..65)
        .map(|id| {
            vec![
                if id % 4 == 0 { Value::Null } else { integer(1) },
                text(if id % 2 == 0 { "Ada" } else { "Bea" }),
                boolean(id % 3 == 0),
                integer(id),
            ]
        })
        .collect::<Vec<_>>();
    let (dir, path) = temporary("m24-own-equality");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, label TEXT, paid BOOLEAN, amount INT);")
            .unwrap();
        insert(&mut database, "users", &users);
        insert(&mut database, "orders", &orders);
    }
    Database::migrate(&path).unwrap();
    let mut plain = Database::open(&path).unwrap();
    let id_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
    assert_eq!(access(&mut plain, id_sql), "HashLeftJoin");
    let unindexed = rows(&mut plain, id_sql);
    drop(plain);

    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_amount ON orders(amount);")
        .unwrap();
    assert_eq!(access(&mut database, id_sql), "HashLeftJoin");
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let explained = query(&mut database, &format!("EXPLAIN {id_sql}"));
    assert_eq!(explained.rows[0][0], text("IndexLeftNestedLoopJoin"));
    assert_eq!(explained.rows[0][4], text("u.id = o.user_id"));
    assert_eq!(explained.rows[0].last(), Some(&text("idx_orders_user")));
    assert_eq!(rows(&mut database, id_sql), unindexed);
    let reversed = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON o.user_id = u.id;";
    let reversed_plan = query(&mut database, &format!("EXPLAIN {reversed}"));
    assert_eq!(reversed_plan.rows[0][0], text("IndexLeftNestedLoopJoin"));
    assert_eq!(reversed_plan.rows[0][4], text("o.user_id = u.id"));
    assert_eq!(rows(&mut database, reversed), unindexed);
    let text_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.name = o.label;";
    assert_eq!(access(&mut database, text_sql), "HashLeftJoin");
    assert_eq!(
        rows(&mut database, text_sql),
        project(
            &logical_rows(
                &users,
                &orders,
                4,
                |user, order| eq_true(&user[1], &order[1]),
                |_, _| true,
            ),
            &[1, 6]
        )
    );
    let flag_sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.active = o.paid;";
    assert_eq!(access(&mut database, flag_sql), "HashLeftJoin");
    assert_eq!(
        rows(&mut database, flag_sql),
        project(
            &logical_rows(
                &users,
                &orders,
                4,
                |user, order| eq_true(&user[2], &order[2]),
                |_, _| true,
            ),
            &[1, 6]
        )
    );
    let or_sql =
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 1;";
    assert_eq!(access(&mut database, or_sql), "LeftNestedLoopJoin");
    let _ = dir;
}

#[test]
fn the_earliest_catalog_index_supplies_the_probe() {
    let users = vec![vec![integer(1), text("Ada")]];
    let orders = (0..65)
        .map(|id| vec![integer(1), integer(1 + id)])
        .collect::<Vec<_>>();
    let (dir, path) = temporary("m24-catalog-order");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        insert(&mut database, "users", &users);
        insert(&mut database, "orders", &orders);
    }
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_amount ON orders(amount);")
        .unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.id = o.amount;";
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexLeftNestedLoopJoin"));
    assert_eq!(explained.rows[0][4], text("u.id = o.amount"));
    assert_eq!(
        explained.rows[0][5],
        text("u.id = o.user_id AND u.id = o.amount")
    );
    assert_eq!(explained.rows[0].last(), Some(&text("idx_orders_amount")));
    let expect = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]) && eq_true(&user[0], &order[1]),
        |_, _| true,
    );
    assert_eq!(rows(&mut database, sql), project(&expect, &[1, 3]));
    let _ = dir;
}

#[test]
fn the_sql_size_boundary_matches_the_right_table() {
    let users = vec![vec![integer(1), text("Ada")], vec![integer(2), text("Bea")]];
    for (orders, plan) in [(128_i64, "HashLeftJoin"), (129, "IndexLeftNestedLoopJoin")] {
        let right = (0..orders)
            .map(|id| vec![integer(1), integer(id)])
            .collect::<Vec<_>>();
        let (_dir, mut database) = open_indexed(&format!("m24-bound-{orders}"), &users, &right);
        let sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
        assert_eq!(access(&mut database, sql), plan);
        let expect = logical_rows(
            &users,
            &right,
            2,
            |user, order| eq_true(&user[0], &order[0]),
            |_, _| true,
        );
        assert_eq!(rows(&mut database, sql), project(&expect, &[1, 3]));
    }
    let users = (0..4)
        .map(|id| vec![integer(id), text(&format!("n{id}"))])
        .collect::<Vec<_>>();
    let orders = (0..4)
        .map(|id| vec![integer(id), integer(id)])
        .collect::<Vec<_>>();
    let (_dir, mut database) = open_indexed("m24-equal", &users, &orders);
    assert_eq!(
        access(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id;"
        ),
        "HashLeftJoin"
    );
}

#[test]
fn an_inner_join_keeps_its_own_64_row_threshold() {
    let users = vec![vec![integer(1), text("Ada")]];
    let small = (0..64)
        .map(|id| vec![integer(1), integer(id)])
        .collect::<Vec<_>>();
    let (_dir, mut database) = open_indexed("m24-inner-64", &users, &small);
    assert_eq!(
        access(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        "HashJoin"
    );
    let large = (0..65)
        .map(|id| vec![integer(1), integer(id)])
        .collect::<Vec<_>>();
    let (_dir, mut database) = open_indexed("m24-inner-65", &users, &large);
    assert_eq!(
        access(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        "IndexNestedLoopJoin"
    );
}

#[test]
fn duplicate_index_keys_follow_insertion_order_through_reopen() {
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![Value::Null, text("Dee")],
    ];
    let mut orders = vec![
        vec![integer(2), integer(1)],
        vec![integer(1), integer(30)],
        vec![integer(2), integer(2)],
        vec![integer(1), integer(10)],
        vec![integer(1), Value::Null],
        vec![integer(3), integer(3)],
        vec![integer(1), integer(20)],
    ];
    while orders.len() < 129 {
        let amount = i64::try_from(orders.len()).unwrap();
        orders.push(vec![integer(9), integer(amount)]);
    }
    let (dir, mut database) = open_indexed("m24-order", &users, &orders);
    let path = dir.join("db");
    let sql = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
    assert_eq!(access(&mut database, sql), "IndexLeftNestedLoopJoin");
    let mut expect = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    assert_eq!(rows(&mut database, sql), project(&expect, &[1, 3]));
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        distinct_rows(project(&expect, &[1, 3]))
    );
    database
        .execute("UPDATE orders SET amount = 31 WHERE user_id = 1 AND amount = 30;")
        .unwrap();
    orders[1][1] = integer(31);
    database
        .execute("DELETE FROM orders WHERE user_id = 1 AND amount = 10;")
        .unwrap();
    orders.remove(3);
    database
        .execute("INSERT INTO orders VALUES (1, 40);")
        .unwrap();
    orders.push(vec![integer(1), integer(40)]);
    expect = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    let projected = project(&expect, &[1, 3]);
    assert_eq!(rows(&mut database, sql), projected);
    assert_eq!(
        projected.iter().take(4).cloned().collect::<Vec<_>>(),
        vec![
            vec![text("Ada"), integer(31)],
            vec![text("Ada"), Value::Null],
            vec![text("Ada"), integer(20)],
            vec![text("Ada"), integer(40)],
        ]
    );
    let filtered = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, order| matches!(order[1], Value::Integer(amount) if amount > 15),
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 15;",
        ),
        project(&filtered, &[1, 3])
    );
    let residual = logical_rows(
        &users,
        &orders,
        2,
        |user, order| {
            eq_true(&user[0], &order[0])
                && matches!(order[1], Value::Integer(amount) if amount > 15)
        },
        |_, _| true,
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 15;",
        ),
        project(&residual, &[1, 3])
    );
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, sql), projected);
}

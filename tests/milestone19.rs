//! Hash `LEFT JOIN` against an independent walker.
//!
//! Equality queries use `HashLeftJoin`. `OR`, a range, and `IS NULL` stay
//! `LeftNestedLoopJoin`. The walker does not build a hash table. Seeds are
//! `0x1919`, `0x19a1`, `0x19b2`, `0x19c3`, `0x19d4`, `0x19e5`, `0x19f6`, and
//! `0x1907`.

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
        panic!("expected a query: {sql}");
    };
    result.clone()
}

fn rows(database: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    query(database, sql).rows
}

fn failure(database: &mut Database, sql: &str) -> String {
    database.execute(sql).unwrap_err().to_string()
}

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn shop() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy'), (NULL, 'Dee');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 50), (1, 150), (1, NULL), (2, 10), (NULL, 5);")
        .unwrap();
    database
}

#[test]
fn hash_left_join_keeps_on_and_where_apart() {
    let mut database = shop();
    let (users, orders) = shop_rows();
    let on_residual = "SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 100;";
    let expect = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| and3(eq_int(&user[0], &order[0]), gt_int(&order[1], 100)),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(rows(&mut database, on_residual), expect);
    assert_eq!(
        expect,
        vec![
            vec![text("Ada"), integer(150)],
            vec![text("Bea"), Value::Null],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
    let filtered = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| eq_int(&user[0], &order[0]),
        |_, order| gt_int(&order[1], 100),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 100;",
        ),
        filtered
    );
    assert_eq!(filtered, vec![vec![text("Ada"), integer(150)]]);
    let reversed = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| eq_int(&order[0], &user[0]),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON o.user_id = u.id;",
        ),
        reversed
    );
}

#[test]
fn matched_nulls_stay_distinct_and_null_keys_do_not_hash() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.user_id, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.amount IS NULL;",
        ),
        vec![
            vec![text("Ada"), integer(1), Value::Null],
            vec![text("Cy"), Value::Null, Value::Null],
            vec![text("Dee"), Value::Null, Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.user_id IS NULL;",
        ),
        vec![vec![text("Cy")], vec![text("Dee")]]
    );
}

#[test]
fn text_and_boolean_keys_keep_duplicates_and_left_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE people (name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE flags (name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO people VALUES ('Ada', TRUE), ('Bea', FALSE), (NULL, TRUE), ('Cy', NULL);",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO flags VALUES ('Ada', TRUE), ('Ada', FALSE), (NULL, TRUE), ('Bea', FALSE);",
        )
        .unwrap();
    let people = vec![
        vec![text("Ada"), Value::Boolean(true)],
        vec![text("Bea"), Value::Boolean(false)],
        vec![Value::Null, Value::Boolean(true)],
        vec![text("Cy"), Value::Null],
    ];
    let flags = vec![
        vec![text("Ada"), Value::Boolean(true)],
        vec![text("Ada"), Value::Boolean(false)],
        vec![Value::Null, Value::Boolean(true)],
        vec![text("Bea"), Value::Boolean(false)],
    ];
    let by_name = left_join_rows(
        &people,
        &flags,
        2,
        |left, right| eq_text(&left[0], &right[0]),
        |_, _| flag(true),
        |left, right| vec![left[0].clone(), right[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT p.name, f.active FROM people p LEFT JOIN flags f ON p.name = f.name;",
        ),
        by_name
    );
    assert_eq!(
        by_name,
        vec![
            vec![text("Ada"), Value::Boolean(true)],
            vec![text("Ada"), Value::Boolean(false)],
            vec![text("Bea"), Value::Boolean(false)],
            vec![Value::Null, Value::Null],
            vec![text("Cy"), Value::Null],
        ]
    );
    let by_flag = left_join_rows(
        &people,
        &flags,
        2,
        |left, right| eq_bool(&left[1], &right[1]),
        |_, _| flag(true),
        |left, right| vec![left[0].clone(), right[0].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT p.name, f.name FROM people p LEFT JOIN flags f ON p.active = f.active;",
        ),
        by_flag
    );
}

#[test]
fn orientations_empty_inputs_and_self_joins_follow_the_walker() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE left_side (id INT, tag TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE right_side (id INT, tag TEXT);")
        .unwrap();
    assert!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id;",
        )
        .is_empty()
    );
    database
        .execute("INSERT INTO right_side VALUES (1, 'r1'), (1, 'r2'), (2, 'r3');")
        .unwrap();
    assert!(
        rows(
            &mut database,
            "SELECT l.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id;",
        )
        .is_empty()
    );
    database
        .execute("INSERT INTO left_side VALUES (1, 'a'), (3, 'b'), (1, 'c');")
        .unwrap();
    let left = vec![
        vec![integer(1), text("a")],
        vec![integer(3), text("b")],
        vec![integer(1), text("c")],
    ];
    let right = vec![
        vec![integer(1), text("r1")],
        vec![integer(1), text("r2")],
        vec![integer(2), text("r3")],
    ];
    let expect = left_join_rows(
        &left,
        &right,
        2,
        |left, right| eq_int(&left[0], &right[0]),
        |_, _| flag(true),
        |left, right| vec![left[1].clone(), right[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id;",
        ),
        expect
    );
    assert_eq!(
        expect,
        vec![
            vec![text("a"), text("r1")],
            vec![text("a"), text("r2")],
            vec![text("b"), Value::Null],
            vec![text("c"), text("r1")],
            vec![text("c"), text("r2")],
        ]
    );
    assert!(
        rows(
            &mut database,
            "SELECT l.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id LIMIT 0;",
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id ORDER BY l.tag DESC LIMIT 1;",
        ),
        vec![vec![text("c"), text("r1")]]
    );
    database.execute("DELETE FROM right_side;").unwrap();
    database
        .execute("INSERT INTO right_side VALUES (1, 'only');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id;",
        ),
        vec![
            vec![text("a"), text("only")],
            vec![text("b"), Value::Null],
            vec![text("c"), text("only")],
        ]
    );
    database
        .execute("CREATE TABLE people (id INT, n INT);")
        .unwrap();
    database
        .execute("INSERT INTO people VALUES (1, 10), (1, 20), (2, 30);")
        .unwrap();
    let people = vec![
        vec![integer(1), integer(10)],
        vec![integer(1), integer(20)],
        vec![integer(2), integer(30)],
    ];
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.n, v.n FROM people u LEFT JOIN people v ON u.id = v.id AND u.n < v.n;",
        ),
        left_join_rows(
            &people,
            &people,
            2,
            |left, right| and3(
                eq_int(&left[0], &right[0]),
                gt_int(&right[1], int_of(&left[1]))
            ),
            |_, _| flag(true),
            |left, right| vec![left[1].clone(), right[1].clone()],
        )
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT id FROM left_side l LEFT JOIN right_side r ON l.id = r.id;",
        ),
        "column 'id' is ambiguous"
    );
}

#[test]
fn explain_reports_hash_or_the_nested_fallback() {
    let mut database = shop();
    let hashed = query(
        &mut database,
        "EXPLAIN SELECT u.name, o.amount
         FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 100
         WHERE u.name = 'Ada'
         ORDER BY o.amount DESC NULLS LAST
         LIMIT 1;",
    );
    assert_eq!(
        hashed.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "build".to_string(),
            "predicate".to_string(),
            "residual".to_string(),
            "filter".to_string(),
            "projection".to_string(),
            "order_by".to_string(),
            "limit".to_string(),
        ]
    );
    assert_eq!(hashed.rows[0][0], text("HashLeftJoin"));
    assert_eq!(hashed.rows[0][3], text("o (orders)"));
    assert_eq!(hashed.rows[0][4], text("u.id = o.user_id"));
    assert_eq!(
        hashed.rows[0][5],
        text("u.id = o.user_id AND o.amount > 100")
    );
    assert_eq!(hashed.rows[0][6], text("u.name = 'Ada'"));
    assert_eq!(hashed.rows[0][8], text("o.amount DESC NULLS LAST"));
    assert_eq!(hashed.rows[0][9], integer(1));
    let nested = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
    );
    assert_eq!(nested.rows[0][0], text("LeftNestedLoopJoin"));
    assert_eq!(nested.rows[0][3], Value::Null);
    assert_eq!(nested.rows[0][4], Value::Null);
    assert_eq!(nested.rows[0][5], text("u.id = o.user_id OR o.amount = 5"));
    let range = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id > o.user_id;",
    );
    assert_eq!(range.rows[0][0], text("LeftNestedLoopJoin"));
    let is_null = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON o.user_id IS NULL;",
    );
    assert_eq!(is_null.rows[0][0], text("LeftNestedLoopJoin"));
    let inner = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(inner.rows[0][0], text("HashJoin"));
    let error = failure(
        &mut database,
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x' LIMIT 0;",
    );
    assert!(
        error.contains("INTEGER") && error.contains("TEXT"),
        "{error}"
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM users;",),
        vec![vec![integer(4)]]
    );
}

#[test]
fn an_index_does_not_turn_a_left_join_into_an_index_probe() {
    let (dir, path) = temporary("m19-hash-left");
    {
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        database
            .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
            .unwrap();
        database
            .execute("INSERT INTO orders VALUES (1, 10), (1, 20);")
            .unwrap();
    }
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_ne!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let selected = rows(
        &mut database,
        "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        selected
    );
    assert_eq!(
        selected,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(20)],
            vec![text("Bea"), Value::Null],
        ]
    );
    let inner = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(inner.rows[0][0], text("HashJoin"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn randomized_hash_left_joins_match_an_independent_reference() {
    for seed in [
        0x1919_u64, 0x19a1, 0x19b2, 0x19c3, 0x19d4, 0x19e5, 0x19f6, 0x1907,
    ] {
        for (users_n, orders_n) in [(0, 0), (0, 5), (5, 0), (3, 8), (8, 3), (6, 6)] {
            let (users, orders) = sample(seed, users_n, orders_n);
            let mut database = Database::new();
            database
                .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
                .unwrap();
            insert_rows(&mut database, "users", &users);
            insert_rows(&mut database, "orders", &orders);
            let got = rows(
                &mut database,
                "SELECT u.name, o.amount
                 FROM users u
                 LEFT JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
                 WHERE o.amount IS NULL OR o.amount > 3
                 ORDER BY u.name ASC NULLS LAST, o.amount DESC NULLS FIRST
                 LIMIT 4;",
            );
            let mut expect = left_join_rows(
                &users,
                &orders,
                3,
                |user, order| {
                    and3(
                        eq_int(&user[0], &order[0]),
                        eq_bool(&order[2], &Value::Boolean(true)),
                    )
                },
                |_, order| or3(flag(order[1].is_null()), gt_int(&order[1], 3)),
                |user, order| vec![user[1].clone(), order[1].clone()],
            );
            expect.sort_by(|left, right| {
                cmp_text_nulls_last(&left[0], &right[0])
                    .then_with(|| cmp_int_desc_nulls_first(&left[1], &right[1]))
            });
            expect.truncate(4);
            assert_eq!(got, expect, "seed {seed:#x} {users_n}x{orders_n}");
            let nested = rows(
                &mut database,
                "SELECT u.name, o.amount
                 FROM users u
                 LEFT JOIN orders o ON u.id = o.user_id OR o.paid = TRUE;",
            );
            let nested_expect = left_join_rows(
                &users,
                &orders,
                3,
                |user, order| {
                    or3(
                        eq_int(&user[0], &order[0]),
                        eq_bool(&order[2], &Value::Boolean(true)),
                    )
                },
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[1].clone()],
            );
            assert_eq!(nested, nested_expect, "or seed {seed:#x}");
        }
    }
}

#[test]
fn a_failed_residual_does_not_count_as_a_match() {
    let mut database = shop();
    let (users, orders) = shop_rows();
    let all_false = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| and3(eq_int(&user[0], &order[0]), gt_int(&order[1], 1_000)),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 1000;",
        ),
        all_false
    );
    assert_eq!(
        all_false,
        vec![
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), Value::Null],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
    let unknown = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| and3(eq_int(&user[0], &order[0]), eq_int(&order[1], &Value::Null)),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount = NULL;",
        ),
        unknown
    );
    assert_eq!(unknown, all_false);
    assert!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 1000;",
        )
        .is_empty()
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 1000;",
    );
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_eq!(explained.rows[0][3], text("o (orders)"));
    let not_eq = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id <> o.user_id;",
    );
    assert_eq!(not_eq.rows[0][0], text("LeftNestedLoopJoin"));
    let is_not_null = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON o.user_id IS NOT NULL;",
    );
    assert_eq!(is_not_null.rows[0][0], text("LeftNestedLoopJoin"));
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM orders;"),
        vec![vec![integer(5)]]
    );
}

#[test]
fn two_equalities_keep_only_rows_that_satisfy_both() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, tag TEXT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (1, 'Bea'), (2, 'Cy'), (3, 'Dee');")
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES (1, 'Ada', 10), (1, 'Bea', 20), (1, 'Zed', 30), (2, 'Cy', 40), (2, 'Cy', 50);",
        )
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(1), text("Bea")],
        vec![integer(2), text("Cy")],
        vec![integer(3), text("Dee")],
    ];
    let orders = vec![
        vec![integer(1), text("Ada"), integer(10)],
        vec![integer(1), text("Bea"), integer(20)],
        vec![integer(1), text("Zed"), integer(30)],
        vec![integer(2), text("Cy"), integer(40)],
        vec![integer(2), text("Cy"), integer(50)],
    ];
    let expect = left_join_rows(
        &users,
        &orders,
        3,
        |user, order| and3(eq_int(&user[0], &order[0]), eq_text(&user[1], &order[1])),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[2].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.tag;",
        ),
        expect
    );
    assert_eq!(
        expect,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Bea"), integer(20)],
            vec![text("Cy"), integer(40)],
            vec![text("Cy"), integer(50)],
            vec![text("Dee"), Value::Null],
        ]
    );
    let reversed = left_join_rows(
        &users,
        &orders,
        3,
        |user, order| and3(eq_text(&order[1], &user[1]), eq_int(&order[0], &user[0])),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[2].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON o.tag = u.name AND o.user_id = u.id;",
        ),
        reversed
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.tag;",
    );
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_eq!(explained.rows[0][4], text("u.id = o.user_id"));
    assert_eq!(
        explained.rows[0][5],
        text("u.id = o.user_id AND u.name = o.tag")
    );
}

#[test]
fn skewed_inputs_keep_left_major_order_against_the_walker() {
    for (left_n, right_n) in [(100_usize, 4_000), (4_000, 100)] {
        let left = (0..left_n)
            .map(|id| {
                let key = if id.is_multiple_of(11) {
                    Value::Null
                } else {
                    integer(i64::try_from(id).unwrap())
                };
                vec![key, text(&format!("L{id:04}"))]
            })
            .collect::<Vec<_>>();
        let right = (0..right_n)
            .map(|seq: usize| {
                let key = if seq.is_multiple_of(17) {
                    Value::Null
                } else {
                    integer(i64::try_from(seq % 13).unwrap())
                };
                vec![
                    key,
                    text(&format!("R{seq:04}")),
                    Value::Boolean(seq.is_multiple_of(2)),
                ]
            })
            .collect::<Vec<_>>();
        let mut database = Database::new();
        database
            .execute("CREATE TABLE left_side (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE right_side (user_id INT, tag TEXT, flag BOOLEAN);")
            .unwrap();
        insert_rows(&mut database, "left_side", &left);
        insert_rows(&mut database, "right_side", &right);
        let expect = left_join_rows(
            &left,
            &right,
            3,
            |left, right| {
                and3(
                    eq_int(&left[0], &right[0]),
                    eq_bool(&right[2], &Value::Boolean(true)),
                )
            },
            |_, _| flag(true),
            |left, right| vec![left[1].clone(), right[1].clone()],
        );
        assert_eq!(
            rows(
                &mut database,
                "SELECT l.name, r.tag
                 FROM left_side l
                 LEFT JOIN right_side r ON l.id = r.user_id AND r.flag = TRUE;",
            ),
            expect,
            "{left_n}x{right_n}"
        );
        let mut ordered = expect.clone();
        ordered.sort_by(|left, right| cmp_text_nulls_last(&left[0], &right[0]));
        ordered.truncate(3);
        assert_eq!(
            rows(
                &mut database,
                "SELECT l.name, r.tag
                 FROM left_side l
                 LEFT JOIN right_side r ON l.id = r.user_id AND r.flag = TRUE
                 ORDER BY l.name ASC NULLS LAST
                 LIMIT 3;",
            ),
            ordered,
            "order {left_n}x{right_n}"
        );
    }
}

#[test]
fn more_seeds_match_the_walker_for_text_boolean_and_self_joins() {
    for seed in [0x19c3_u64, 0x19d4, 0x19e5, 0x19f6, 0x1907] {
        let (people, flags) = sample_flags(seed, 12, 40);
        let mut database = Database::new();
        database
            .execute("CREATE TABLE people (name TEXT, active BOOLEAN, n INT);")
            .unwrap();
        database
            .execute("CREATE TABLE flags (name TEXT, active BOOLEAN, n INT);")
            .unwrap();
        insert_rows(&mut database, "people", &people);
        insert_rows(&mut database, "flags", &flags);
        let by_text = left_join_rows(
            &people,
            &flags,
            3,
            |left, right| and3(eq_text(&left[0], &right[0]), eq_bool(&left[1], &right[1])),
            |_, right| or3(flag(right[2].is_null()), gt_int(&right[2], 2)),
            |left, right| vec![left[0].clone(), right[2].clone()],
        );
        assert_eq!(
            rows(
                &mut database,
                "SELECT p.name, f.n
                 FROM people p
                 LEFT JOIN flags f ON p.name = f.name AND p.active = f.active
                 WHERE f.n IS NULL OR f.n > 2;",
            ),
            by_text,
            "text/bool seed {seed:#x}"
        );
        let self_rows = sample_self(seed, 9);
        database
            .execute("CREATE TABLE nodes (id INT, n INT, label TEXT);")
            .unwrap();
        insert_rows(&mut database, "nodes", &self_rows);
        let expect = left_join_rows(
            &self_rows,
            &self_rows,
            3,
            |left, right| and3(eq_int(&left[0], &right[0]), ne_int(&left[1], &right[1])),
            |_, _| flag(true),
            |left, right| vec![left[2].clone(), right[2].clone()],
        );
        assert_eq!(
            rows(
                &mut database,
                "SELECT a.label, b.label
                 FROM nodes a LEFT JOIN nodes b ON a.id = b.id AND a.n <> b.n;",
            ),
            expect,
            "self seed {seed:#x}"
        );
    }
}

fn shop_rows() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    (
        vec![
            vec![integer(1), text("Ada")],
            vec![integer(2), text("Bea")],
            vec![integer(3), text("Cy")],
            vec![Value::Null, text("Dee")],
        ],
        vec![
            vec![integer(1), integer(50)],
            vec![integer(1), integer(150)],
            vec![integer(1), Value::Null],
            vec![integer(2), integer(10)],
            vec![Value::Null, integer(5)],
        ],
    )
}

fn left_join_rows(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    right_width: usize,
    mut on: impl FnMut(&[Value], &[Value]) -> Option<bool>,
    mut filter: impl FnMut(&[Value], &[Value]) -> Option<bool>,
    mut project: impl FnMut(&[Value], &[Value]) -> Vec<Value>,
) -> Vec<Vec<Value>> {
    let nulls = vec![Value::Null; right_width];
    let mut rows = Vec::new();
    for left_row in left {
        let mut matched = false;
        for right_row in right {
            if !is_true(on(left_row, right_row)) {
                continue;
            }
            matched = true;
            if is_true(filter(left_row, right_row)) {
                rows.push(project(left_row, right_row));
            }
        }
        if !matched && is_true(filter(left_row, &nulls)) {
            rows.push(project(left_row, &nulls));
        }
    }
    rows
}

fn flag(value: bool) -> Option<bool> {
    Some(value)
}

fn is_true(value: Option<bool>) -> bool {
    matches!(value, Some(true))
}

fn eq_int(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left == right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected integers"),
    }
}

fn eq_text(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Text(left), Value::Text(right)) => Some(left == right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected text"),
    }
}

fn eq_bool(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Boolean(left), Value::Boolean(right)) => Some(left == right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected booleans"),
    }
}

fn gt_int(value: &Value, bound: i64) -> Option<bool> {
    match value {
        Value::Integer(value) => Some(*value > bound),
        Value::Null => None,
        _ => panic!("expected an integer"),
    }
}

fn int_of(value: &Value) -> i64 {
    match value {
        Value::Integer(value) => *value,
        _ => panic!("expected an integer"),
    }
}

fn and3(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or3(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

fn cmp_text_nulls_last(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        _ => panic!("expected text"),
    }
}

fn cmp_int_desc_nulls_first(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (Value::Integer(left), Value::Integer(right)) => right.cmp(left),
        _ => panic!("expected integers"),
    }
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        self.0
    }
}

fn sample(seed: u64, users: usize, orders: usize) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let mut rng = Lcg(seed);
    let mut people = Vec::with_capacity(users);
    for id in 0..users {
        let name = if rng.next().is_multiple_of(5) {
            Value::Null
        } else {
            text(&format!("n{}", rng.next() % 4))
        };
        let active = Value::Boolean(rng.next().is_multiple_of(2));
        let key = if rng.next().is_multiple_of(7) {
            Value::Null
        } else {
            integer(i64::try_from(id).unwrap())
        };
        people.push(vec![key, name, active]);
    }
    let mut sales = Vec::with_capacity(orders);
    for _ in 0..orders {
        let user_id = if users == 0 || rng.next().is_multiple_of(6) {
            Value::Null
        } else {
            integer(i64::try_from(rng.next() as usize % users).unwrap())
        };
        let amount = if rng.next().is_multiple_of(4) {
            Value::Null
        } else {
            integer(i64::try_from(rng.next() % 9).unwrap())
        };
        sales.push(vec![
            user_id,
            amount,
            Value::Boolean(rng.next().is_multiple_of(2)),
        ]);
    }
    (people, sales)
}

fn ne_int(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left != right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected integers"),
    }
}

fn sample_flags(seed: u64, people_n: usize, flags_n: usize) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let mut rng = Lcg(seed);
    let mut people = Vec::with_capacity(people_n);
    for id in 0..people_n {
        let name = if rng.next().is_multiple_of(4) {
            Value::Null
        } else {
            text(&format!("t{}", rng.next() % 3))
        };
        people.push(vec![
            name,
            Value::Boolean(!rng.next().is_multiple_of(3)),
            if rng.next().is_multiple_of(5) {
                Value::Null
            } else {
                integer(i64::try_from(id % 7).unwrap())
            },
        ]);
    }
    let mut flags = Vec::with_capacity(flags_n);
    for id in 0..flags_n {
        let name = if rng.next().is_multiple_of(3) {
            Value::Null
        } else {
            text(&format!("t{}", id % 3))
        };
        flags.push(vec![
            name,
            Value::Boolean(rng.next().is_multiple_of(2)),
            if rng.next().is_multiple_of(4) {
                Value::Null
            } else {
                integer(i64::try_from(rng.next() % 6).unwrap())
            },
        ]);
    }
    (people, flags)
}

fn sample_self(seed: u64, rows: usize) -> Vec<Vec<Value>> {
    let mut rng = Lcg(seed);
    (0..rows)
        .map(|id| {
            vec![
                if rng.next().is_multiple_of(5) {
                    Value::Null
                } else {
                    integer(i64::try_from(id % 4).unwrap())
                },
                if rng.next().is_multiple_of(4) {
                    Value::Null
                } else {
                    integer(i64::try_from(rng.next() % 5).unwrap())
                },
                text(&format!("s{id}")),
            ]
        })
        .collect()
}

fn insert_rows(database: &mut Database, table: &str, rows: &[Vec<Value>]) {
    for row in rows {
        let cells = row
            .iter()
            .map(|cell| match cell {
                Value::Null => "NULL".to_string(),
                Value::Integer(value) => value.to_string(),
                Value::Text(value) => format!("'{value}'"),
                Value::Boolean(true) => "TRUE".to_string(),
                Value::Boolean(false) => "FALSE".to_string(),
                Value::Ratio { .. } => panic!("ratio"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        database
            .execute(&format!("INSERT INTO {table} VALUES ({cells});"))
            .unwrap();
    }
}

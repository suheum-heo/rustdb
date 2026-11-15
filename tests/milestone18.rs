//! Two-table `LEFT JOIN` without aggregates or `DISTINCT`.
//!
//! The reference walks both inputs in insertion order, remembers whether `ON`
//! was TRUE, and emits one null-extended right side only when it was not.
//! `WHERE`, projection, sort, and `LIMIT` run after that. It does not call
//! the engine's join executor. Recorded seeds are `0x1818`, `0x18a1`,
//! `0x18b2`, `0x18c3`, `0x18d4`, `0x18e5`, `0x18f6`, and `0x1807`.

use std::cmp::Ordering;

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

const BASIC: &str = "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;";

#[test]
fn basic_left_join_keeps_unmatched_rows_and_duplicate_matches() {
    let mut database = shop();
    assert_eq!(
        rows(&mut database, BASIC),
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(10)],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
    let outer = rows(
        &mut database,
        "SELECT u.name, o.amount FROM users u LEFT OUTER JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(outer, rows(&mut database, BASIC));
    let inner = rows(
        &mut database,
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        inner,
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(10)],
        ]
    );
}

#[test]
fn on_and_where_do_not_preserve_the_same_rows() {
    let mut database = shop();
    let on_residual = rows(
        &mut database,
        "SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 100;",
    );
    assert_eq!(
        on_residual,
        vec![
            vec![text("Ada"), integer(150)],
            vec![text("Bea"), Value::Null],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
    let filtered = rows(
        &mut database,
        "SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 100;",
    );
    assert_eq!(filtered, vec![vec![text("Ada"), integer(150)]]);
}

#[test]
fn unmatched_rows_are_not_the_same_as_a_matched_null() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.user_id, o.amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.user_id IS NULL;",
        ),
        vec![
            vec![text("Cy"), Value::Null, Value::Null],
            vec![text("Dee"), Value::Null, Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.user_id, o.amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
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
            "SELECT u.name FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.user_id IS NOT NULL;",
        ),
        vec![
            text_row("Ada"),
            text_row("Ada"),
            text_row("Ada"),
            text_row("Bea")
        ]
    );
}

fn text_row(value: &str) -> Vec<Value> {
    vec![text(value)]
}

#[test]
fn false_unknown_and_or_conditions_follow_three_valued_logic() {
    let mut database = shop();
    let names = rows(
        &mut database,
        "SELECT u.name FROM users u LEFT JOIN orders o ON FALSE;",
    );
    assert_eq!(
        names,
        vec![
            text_row("Ada"),
            text_row("Bea"),
            text_row("Cy"),
            text_row("Dee")
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON NULL;",
        ),
        names
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
        ),
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Ada"), integer(5)],
            vec![text("Bea"), integer(10)],
            vec![text("Bea"), integer(5)],
            vec![text("Cy"), integer(5)],
            vec![text("Dee"), integer(5)],
        ]
    );
}

#[test]
fn empty_inputs_and_self_joins_keep_left_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    assert!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        )
        .is_empty()
    );
    database
        .execute("INSERT INTO orders VALUES (1, 10);")
        .unwrap();
    assert!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        )
        .is_empty()
    );
    database.execute("DELETE FROM orders;").unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, v.name
             FROM users u
             LEFT JOIN users v ON u.name = v.name AND u.id < v.id;",
        ),
        vec![
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), Value::Null],
        ]
    );
    database
        .execute("INSERT INTO users VALUES (3, 'Ada');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.id, v.id FROM users u LEFT JOIN users v ON u.name = v.name AND u.id < v.id;",
        ),
        vec![
            vec![integer(1), integer(3)],
            vec![integer(2), Value::Null],
            vec![integer(3), Value::Null],
        ]
    );
    let error = failure(
        &mut database,
        "SELECT name FROM users u LEFT JOIN users v ON u.id = v.id;",
    );
    assert_eq!(error, "column 'name' is ambiguous");
}

#[test]
fn projection_order_sort_and_limit_use_null_extensions() {
    let mut database = shop();
    let star = rows(
        &mut database,
        "SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Cy';",
    );
    assert_eq!(
        star,
        vec![vec![integer(3), text("Cy"), Value::Null, Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.*, o.* FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Cy';",
        ),
        star
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT o.*, u.* FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Cy';",
        ),
        vec![vec![Value::Null, Value::Null, integer(3), text("Cy")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, u.name, o.amount AS spent
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id
             WHERE u.name = 'Cy';",
        ),
        vec![vec![text("Cy"), text("Cy"), Value::Null]]
    );
    let ordered = rows(
        &mut database,
        "SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id
         ORDER BY o.amount ASC NULLS FIRST, u.name ASC;",
    );
    assert_eq!(ordered[0], vec![text("Ada"), Value::Null]);
    assert_eq!(*ordered.last().unwrap(), vec![text("Ada"), integer(150)]);
    let last_nulls = rows(
        &mut database,
        "SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id
         ORDER BY o.amount ASC NULLS LAST, u.name ASC
         LIMIT 1;",
    );
    assert_eq!(last_nulls, vec![vec![text("Bea"), integer(10)]]);
    assert!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id LIMIT 0;",
        )
        .is_empty()
    );
    let hidden = rows(
        &mut database,
        "SELECT u.name
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id
         ORDER BY o.amount DESC NULLS LAST, u.name ASC
         LIMIT 1;",
    );
    assert_eq!(hidden, vec![text_row("Ada")]);
    let error = failure(
        &mut database,
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x' LIMIT 0;",
    );
    assert!(
        error.contains("INTEGER") && error.contains("TEXT"),
        "{error}"
    );
}

#[test]
fn an_index_does_not_change_rows_and_explain_does_not_write() {
    let (dir, path) = temporary("m18-index");
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
    let before = std::fs::metadata(&path).unwrap().len();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let after_index = std::fs::metadata(&path).unwrap().len();
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, o.amount
         FROM users u
         LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 15
         ORDER BY u.name ASC
         LIMIT 1;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "condition".to_string(),
            "filter".to_string(),
            "projection".to_string(),
            "order_by".to_string(),
            "limit".to_string(),
        ]
    );
    assert_eq!(explained.rows[0][0], text("LeftNestedLoopJoin"));
    assert_eq!(explained.rows[0][1], text("u (users)"));
    assert_eq!(explained.rows[0][2], text("o (orders)"));
    assert_eq!(explained.rows[0][3], text("u.id = o.user_id"));
    assert_eq!(explained.rows[0][4], text("o.amount > 15"));
    assert_eq!(explained.rows[0][5], text("name, amount"));
    assert_eq!(explained.rows[0][6], text("u.name ASC"));
    assert_eq!(explained.rows[0][7], integer(1));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), after_index);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let failed = failure(
        &mut database,
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x';",
    );
    assert!(
        failed.contains("INTEGER") && failed.contains("TEXT"),
        "{failed}"
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), after_index);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let selected = rows(
        &mut database,
        "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    drop(database);
    let mut database = Database::open(&path).unwrap();
    let before_select = std::fs::metadata(&path).unwrap().len();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        selected
    );
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before_select);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let inner = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(inner.rows[0][0], text("HashJoin"));
    assert_eq!(inner.rows[0][3], text("o (orders)"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT name FROM users ORDER BY name;"
        ),
        vec![text_row("Ada"), text_row("Bea")]
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM orders;"),
        vec![vec![integer(2)]]
    );
    assert_eq!(
        selected,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(20)],
            vec![text("Bea"), Value::Null],
        ]
    );
    assert!(before > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unsupported_outer_forms_stay_rejected() {
    let mut database = shop();
    let cases = [
        (
            "SELECT * FROM users RIGHT JOIN orders ON users.id = orders.user_id;",
            "RIGHT JOIN is not supported",
        ),
        (
            "SELECT * FROM users FULL JOIN orders ON users.id = orders.user_id;",
            "FULL JOIN is not supported",
        ),
        (
            "SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id LEFT JOIN orders p ON u.id = p.user_id;",
            "multiple JOINs are not supported",
        ),
        (
            "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
            "LEFT JOIN is not supported with DISTINCT",
        ),
        (
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
            "LEFT JOIN is not supported with GROUP BY",
        ),
        (
            "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
            "LEFT JOIN is not supported with aggregate functions",
        ),
        (
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id HAVING u.name = 'Ada';",
            "LEFT JOIN is not supported with HAVING",
        ),
    ];
    for (sql, message) in cases {
        assert_eq!(failure(&mut database, sql), message, "{sql}");
    }
    let error = failure(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(error, "LEFT JOIN is not supported with aggregate functions");
}

#[test]
fn predicate_shapes_match_a_hand_checked_reference() {
    let mut database = shop();
    let (users, orders) = shop_rows();
    let cases = [
        (
            "SELECT u.name, o.user_id, o.amount
             FROM users u LEFT JOIN orders o ON o.amount IS NULL;",
            left_join_rows(
                &users,
                &orders,
                2,
                |_, order| flag(order[1].is_null()),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[0].clone(), order[1].clone()],
            ),
            vec![
                vec![text("Ada"), integer(1), Value::Null],
                vec![text("Bea"), integer(1), Value::Null],
                vec![text("Cy"), integer(1), Value::Null],
                vec![text("Dee"), integer(1), Value::Null],
            ],
        ),
        (
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON o.user_id IS NULL;",
            left_join_rows(
                &users,
                &orders,
                2,
                |_, order| flag(order[0].is_null()),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[1].clone()],
            ),
            vec![
                vec![text("Ada"), integer(5)],
                vec![text("Bea"), integer(5)],
                vec![text("Cy"), integer(5)],
                vec![text("Dee"), integer(5)],
            ],
        ),
        (
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id IS NULL;",
            left_join_rows(
                &users,
                &orders,
                2,
                |user, _| flag(user[0].is_null()),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[1].clone()],
            ),
            vec![
                vec![text("Ada"), Value::Null],
                vec![text("Bea"), Value::Null],
                vec![text("Cy"), Value::Null],
                row2("Dee", Some(50)),
                row2("Dee", Some(150)),
                row2("Dee", None),
                row2("Dee", Some(10)),
                row2("Dee", Some(5)),
            ],
        ),
        (
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON o.user_id = u.id;",
            left_join_rows(
                &users,
                &orders,
                2,
                |user, order| eq_int(&order[0], &user[0]),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[1].clone()],
            ),
            rows(&mut database, BASIC),
        ),
        (
            "SELECT u.name, o.user_id, o.amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id AND o.user_id IS NOT NULL;",
            left_join_rows(
                &users,
                &orders,
                2,
                |user, order| and3(eq_int(&user[0], &order[0]), flag(!order[0].is_null())),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[0].clone(), order[1].clone()],
            ),
            vec![
                vec![text("Ada"), integer(1), integer(50)],
                vec![text("Ada"), integer(1), integer(150)],
                vec![text("Ada"), integer(1), Value::Null],
                vec![text("Bea"), integer(2), integer(10)],
                vec![text("Cy"), Value::Null, Value::Null],
                vec![text("Dee"), Value::Null, Value::Null],
            ],
        ),
        (
            "SELECT u.name, o.amount
             FROM users u
             LEFT JOIN orders o ON u.id = o.user_id OR o.amount IS NULL;",
            left_join_rows(
                &users,
                &orders,
                2,
                |user, order| or3(eq_int(&user[0], &order[0]), flag(order[1].is_null())),
                |_, _| flag(true),
                |user, order| vec![user[1].clone(), order[1].clone()],
            ),
            vec![
                row2("Ada", Some(50)),
                row2("Ada", Some(150)),
                row2("Ada", None),
                row2("Bea", None),
                row2("Bea", Some(10)),
                row2("Cy", None),
                row2("Dee", None),
            ],
        ),
    ];
    for (sql, expect, pinned) in cases {
        let got = rows(&mut database, sql);
        assert_eq!(got, expect, "{sql}");
        assert_eq!(expect, pinned, "{sql}");
    }

    let filtered_all = "SELECT u.name, o.amount
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 1000;";
    assert!(rows(&mut database, filtered_all).is_empty());
    let preserved = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| and3(eq_int(&user[0], &order[0]), gt_int(&order[1], 1000)),
        |_, _| flag(true),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 1000;",
        ),
        preserved
    );
    assert_eq!(preserved.len(), 4);

    let some = "SELECT u.name, o.amount
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 40 OR o.amount IS NULL;";
    let some_expect = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| eq_int(&user[0], &order[0]),
        |_, order| or3(gt_int(&order[1], 40), flag(order[1].is_null())),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(rows(&mut database, some), some_expect);
    assert_eq!(
        some_expect,
        vec![
            row2("Ada", Some(50)),
            row2("Ada", Some(150)),
            row2("Ada", None),
            row2("Cy", None),
            row2("Dee", None),
        ]
    );

    let mixed = "SELECT u.name, o.amount
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 100 AND u.name = 'Ada' OR u.name = 'Cy';";
    let mixed_expect = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| eq_int(&user[0], &order[0]),
        |user, order| {
            or3(
                and3(gt_int(&order[1], 100), flag(user[1] == text("Ada"))),
                flag(user[1] == text("Cy")),
            )
        },
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(rows(&mut database, mixed), mixed_expect);
    assert_eq!(mixed_expect, vec![row2("Ada", Some(150)), row2("Cy", None)]);

    let negated = left_join_rows(
        &users,
        &orders,
        2,
        |user, order| eq_int(&user[0], &order[0]),
        |_, order| not3(eq_int(&order[1], &integer(10))),
        |user, order| vec![user[1].clone(), order[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE NOT (o.amount = 10);",
        ),
        negated
    );
    assert_eq!(negated, vec![row2("Ada", Some(50)), row2("Ada", Some(150))]);
    assert!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount = NULL;",
        )
        .is_empty()
    );
}

#[test]
fn both_size_orientations_keep_left_then_right_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE left_side (id INT, tag TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE right_side (id INT, tag TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO left_side VALUES (1, 'a'), (2, 'b');")
        .unwrap();
    database
        .execute(
            "INSERT INTO right_side VALUES (1, 'r1'), (1, 'r2'), (9, 'r3'), (2, 'r4'), (1, 'r5');",
        )
        .unwrap();
    let left = vec![vec![integer(1), text("a")], vec![integer(2), text("b")]];
    let right = vec![
        vec![integer(1), text("r1")],
        vec![integer(1), text("r2")],
        vec![integer(9), text("r3")],
        vec![integer(2), text("r4")],
        vec![integer(1), text("r5")],
    ];
    let sql = "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id";
    let expect = left_join_rows(
        &left,
        &right,
        2,
        |left, right| eq_int(&left[0], &right[0]),
        |_, _| flag(true),
        |left, right| vec![left[1].clone(), right[1].clone()],
    );
    assert_eq!(rows(&mut database, &format!("{sql};")), expect);
    assert_eq!(
        expect,
        vec![
            vec![text("a"), text("r1")],
            vec![text("a"), text("r2")],
            vec![text("a"), text("r5")],
            vec![text("b"), text("r4")],
        ]
    );
    assert_eq!(
        rows(&mut database, &format!("{sql} LIMIT 1;")),
        vec![vec![text("a"), text("r1")]]
    );

    database.execute("DELETE FROM left_side;").unwrap();
    database.execute("DELETE FROM right_side;").unwrap();
    database
        .execute("INSERT INTO left_side VALUES (1, 'a'), (2, 'b'), (3, 'c'), (1, 'd'), (4, 'e');")
        .unwrap();
    database
        .execute("INSERT INTO right_side VALUES (1, 'r1'), (3, 'r2');")
        .unwrap();
    let left = vec![
        vec![integer(1), text("a")],
        vec![integer(2), text("b")],
        vec![integer(3), text("c")],
        vec![integer(1), text("d")],
        vec![integer(4), text("e")],
    ];
    let right = vec![vec![integer(1), text("r1")], vec![integer(3), text("r2")]];
    let expect = left_join_rows(
        &left,
        &right,
        2,
        |left, right| eq_int(&left[0], &right[0]),
        |_, _| flag(true),
        |left, right| vec![left[1].clone(), right[1].clone()],
    );
    assert_eq!(rows(&mut database, &format!("{sql};")), expect);
    assert_eq!(
        expect,
        vec![
            vec![text("a"), text("r1")],
            vec![text("b"), Value::Null],
            vec![text("c"), text("r2")],
            vec![text("d"), text("r1")],
            vec![text("e"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id ORDER BY l.tag DESC LIMIT 1;",
        ),
        vec![vec![text("e"), Value::Null]]
    );

    database.execute("DELETE FROM left_side;").unwrap();
    database.execute("DELETE FROM right_side;").unwrap();
    database
        .execute("INSERT INTO left_side VALUES (1, 'a'), (2, 'b');")
        .unwrap();
    database
        .execute("INSERT INTO right_side VALUES (2, 'r1'), (1, 'r2');")
        .unwrap();
    assert_eq!(
        rows(&mut database, sql),
        vec![vec![text("a"), text("r2")], vec![text("b"), text("r1")],]
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
    let expect = left_join_rows(
        &people,
        &people,
        2,
        |left, right| {
            and3(
                eq_int(&left[0], &right[0]),
                gt_int(&right[1], left_int(&left[1])),
            )
        },
        |_, _| flag(true),
        |left, right| vec![left[1].clone(), right[1].clone()],
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.n, v.n FROM people u LEFT JOIN people v ON u.id = v.id AND u.n < v.n;",
        ),
        expect
    );
    assert_eq!(
        expect,
        vec![
            vec![integer(10), integer(20)],
            vec![integer(20), Value::Null],
            vec![integer(30), Value::Null],
        ]
    );

    database.execute("DELETE FROM right_side;").unwrap();
    database
        .execute("INSERT INTO right_side VALUES (1, 'r'), (1, 'r');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.tag, r.tag FROM left_side l LEFT JOIN right_side r ON l.id = r.id WHERE l.tag = 'a';",
        ),
        vec![vec![text("a"), text("r")], vec![text("a"), text("r")],]
    );
}

fn left_int(value: &Value) -> i64 {
    match value {
        Value::Integer(value) => *value,
        _ => panic!("expected an integer"),
    }
}

#[test]
fn binding_stars_aliases_and_schema_stay_unchanged() {
    let mut database = shop();
    let star = query(
        &mut database,
        "SELECT u.* FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Cy';",
    );
    assert_eq!(star.columns, vec!["id".to_string(), "name".to_string()]);
    assert_eq!(star.rows, vec![vec![integer(3), text("Cy")]]);
    let right = query(
        &mut database,
        "SELECT o.* FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Cy';",
    );
    assert_eq!(
        right.columns,
        vec!["user_id".to_string(), "amount".to_string()]
    );
    assert_eq!(right.rows, vec![vec![Value::Null, Value::Null]]);
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, u.name, o.amount, o.amount
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE u.name = 'Bea';",
        ),
        vec![vec![text("Bea"), text("Bea"), integer(10), integer(10)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name AS label, o.amount AS name
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             ORDER BY name ASC NULLS LAST, label ASC;",
        ),
        vec![
            vec![text("Bea"), integer(10)],
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
    database.execute("DELETE FROM users;").unwrap();
    database.execute("DELETE FROM orders;").unwrap();
    let error = failure(
        &mut database,
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 'x' LIMIT 0;",
    );
    assert!(
        error.contains("INTEGER") && error.contains("TEXT"),
        "{error}"
    );
    let mismatch = failure(&mut database, "INSERT INTO orders VALUES (1, 'x');");
    assert!(
        mismatch.contains("INTEGER") && mismatch.contains("TEXT"),
        "{mismatch}"
    );
    database
        .execute("INSERT INTO orders VALUES (NULL, NULL);")
        .unwrap();

    database
        .execute("CREATE TABLE \"Users\" (id INT, \"Name\" TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE \"Orders\" (\"UserId\" INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO \"Users\" VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO \"Orders\" VALUES (1, 5);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.\"Name\", o.amount
             FROM \"Users\" u LEFT JOIN \"Orders\" o ON u.id = o.\"UserId\";",
        ),
        vec![
            vec![text("Ada"), integer(5)],
            vec![text("Bea"), Value::Null],
        ]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT \"Name\" FROM \"Users\" u LEFT JOIN \"Users\" v ON u.id = v.id;",
        ),
        "column 'Name' is ambiguous"
    );
    assert_eq!(
        failure(
            &mut database,
            "EXPLAIN SELECT \"Name\" FROM \"Users\" u LEFT JOIN \"Users\" v ON u.id = v.id;",
        ),
        "column 'Name' is ambiguous"
    );
}

#[test]
fn a_failed_left_join_leaves_the_next_query_intact() {
    let mut database = shop();
    let error = database.execute(
        "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x' LIMIT 0;
         SELECT u.name FROM users u;",
    );
    assert!(error.unwrap_err().to_string().contains("INTEGER"));
    assert_eq!(
        rows(&mut database, BASIC),
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(10)],
            vec![text("Cy"), Value::Null],
            vec![text("Dee"), Value::Null],
        ]
    );
}

#[test]
fn randomized_left_joins_match_an_independent_reference() {
    for seed in [0x1818_u64, 0x18a1, 0x18b2] {
        let (users, orders) = sample(seed, 12, 20);
        let mut database = Database::new();
        database
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
            .unwrap();
        insert_users(&mut database, &users);
        insert_orders(&mut database, &orders);
        let sql = "SELECT u.name, o.amount
            FROM users u
            LEFT JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
            WHERE o.amount IS NULL OR o.amount > 3
            ORDER BY u.name ASC NULLS LAST, o.amount DESC NULLS FIRST
            LIMIT 5;";
        let got = rows(&mut database, sql);
        let expect = reference(&users, &orders);
        assert_eq!(got, expect, "seed {seed:#x}");
    }
}

#[test]
fn randomized_orientations_match_an_independent_reference() {
    let seeds = [
        0x1818_u64, 0x18a1, 0x18b2, 0x18c3, 0x18d4, 0x18e5, 0x18f6, 0x1807,
    ];
    let sizes = [(0, 0), (0, 4), (4, 0), (2, 7), (7, 2), (5, 5)];
    for seed in seeds {
        for (users_n, orders_n) in sizes {
            let (users, orders) = sample(seed, users_n, orders_n);
            let mut database = Database::new();
            database
                .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
                .unwrap();
            insert_users(&mut database, &users);
            insert_orders(&mut database, &orders);
            let got = rows(
                &mut database,
                "SELECT u.name, o.amount
                 FROM users u
                 LEFT JOIN orders o ON o.user_id = u.id AND o.paid = TRUE
                 WHERE o.amount IS NULL OR o.amount > 3;",
            );
            let expect = left_join_rows(
                &users,
                &orders,
                3,
                |user, order| and3(eq_int(&order[0], &user[0]), eq_bool(&order[2], true)),
                |_, order| or3(flag(order[1].is_null()), gt_int(&order[1], 3)),
                |user, order| vec![user[1].clone(), order[1].clone()],
            );
            assert_eq!(got, expect, "seed {seed:#x} {users_n}x{orders_n}");

            let got = rows(
                &mut database,
                "SELECT u.id, o.user_id
                 FROM users u
                 LEFT JOIN orders o ON u.id = o.user_id
                 ORDER BY u.id ASC NULLS LAST
                 LIMIT 5;",
            );
            let mut expect = left_join_rows(
                &users,
                &orders,
                3,
                |user, order| eq_int(&user[0], &order[0]),
                |_, _| flag(true),
                |user, order| vec![user[0].clone(), order[0].clone()],
            );
            expect.sort_by(|left, right| cmp_int_nulls_last(&left[0], &right[0]));
            expect.truncate(5);
            assert_eq!(got, expect, "ordered seed {seed:#x} {users_n}x{orders_n}");
        }
        let (users, _) = sample(seed, 6, 0);
        let mut database = Database::new();
        database
            .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        insert_users(&mut database, &users);
        let got = rows(
            &mut database,
            "SELECT u.id, v.id
             FROM users u
             LEFT JOIN users v ON u.id = v.id AND u.active = v.active
             WHERE v.name IS NULL;",
        );
        let expect = left_join_rows(
            &users,
            &users,
            3,
            |left, right| and3(eq_int(&left[0], &right[0]), eq_bools(&left[2], &right[2])),
            |_, right| flag(right[1].is_null()),
            |left, right| vec![left[0].clone(), right[0].clone()],
        );
        assert_eq!(got, expect, "self seed {seed:#x}");
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

fn row2(name: &str, amount: Option<i64>) -> Vec<Value> {
    vec![
        text(name),
        match amount {
            Some(amount) => integer(amount),
            None => Value::Null,
        },
    ]
}

fn flag(value: bool) -> Option<bool> {
    Some(value)
}

fn not3(value: Option<bool>) -> Option<bool> {
    value.map(|value| !value)
}

fn eq_bools(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Boolean(left), Value::Boolean(right)) => Some(left == right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected booleans"),
    }
}

fn cmp_int_nulls_last(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        _ => panic!("expected integers"),
    }
}

fn reference(users: &[Vec<Value>], orders: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let nulls = vec![Value::Null, Value::Null, Value::Null];
    let mut joined = Vec::new();
    for user in users {
        let mut matched = false;
        for order in orders {
            if !on_true(user, order) {
                continue;
            }
            matched = true;
            if where_true(order) {
                joined.push((user.clone(), order.clone()));
            }
        }
        if !matched && where_true(&nulls) {
            joined.push((user.clone(), nulls.clone()));
        }
    }
    joined.sort_by(|left, right| {
        compare_nulls_last(&left.0[1], &right.0[1])
            .then_with(|| compare_amount_desc(&left.1[1], &right.1[1]))
    });
    joined.truncate(5);
    joined
        .into_iter()
        .map(|(user, order)| vec![user[1].clone(), order[1].clone()])
        .collect()
}

fn on_true(user: &[Value], order: &[Value]) -> bool {
    is_true(and3(eq_int(&user[0], &order[0]), eq_bool(&order[2], true)))
}

fn where_true(order: &[Value]) -> bool {
    is_true(or3(
        Some(matches!(order[1], Value::Null)),
        gt_int(&order[1], 3),
    ))
}

fn eq_int(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => Some(left == right),
        (Value::Null, _) | (_, Value::Null) => None,
        _ => panic!("expected integers"),
    }
}

fn eq_bool(value: &Value, expect: bool) -> Option<bool> {
    match value {
        Value::Boolean(value) => Some(*value == expect),
        Value::Null => None,
        _ => panic!("expected a boolean"),
    }
}

fn gt_int(value: &Value, bound: i64) -> Option<bool> {
    match value {
        Value::Integer(value) => Some(*value > bound),
        Value::Null => None,
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

fn is_true(value: Option<bool>) -> bool {
    matches!(value, Some(true))
}

fn compare_nulls_last(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        _ => panic!("expected text"),
    }
}

fn compare_amount_desc(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
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

fn insert_users(database: &mut Database, rows: &[Vec<Value>]) {
    for row in rows {
        let id = sql_cell(&row[0]);
        let name = sql_cell(&row[1]);
        let active = match &row[2] {
            Value::Boolean(true) => "TRUE",
            Value::Boolean(false) => "FALSE",
            _ => panic!("active"),
        };
        database
            .execute(&format!(
                "INSERT INTO users VALUES ({id}, {name}, {active});"
            ))
            .unwrap();
    }
}

fn insert_orders(database: &mut Database, rows: &[Vec<Value>]) {
    for row in rows {
        let user_id = sql_cell(&row[0]);
        let amount = sql_cell(&row[1]);
        let paid = match &row[2] {
            Value::Boolean(true) => "TRUE",
            Value::Boolean(false) => "FALSE",
            _ => panic!("paid"),
        };
        database
            .execute(&format!(
                "INSERT INTO orders VALUES ({user_id}, {amount}, {paid});"
            ))
            .unwrap();
    }
}

fn sql_cell(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Text(value) => format!("'{value}'"),
        Value::Boolean(value) => {
            if *value {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        Value::Ratio { .. } => panic!("ratio is not a stored cell"),
    }
}

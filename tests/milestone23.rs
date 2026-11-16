//! `SELECT DISTINCT` over one `LEFT JOIN`, checked against a linear walker.
//!
//! The walker emits `ON`-true pairs or one null-extended row, applies `WHERE`,
//! projects, and keeps the first equal tuple with direct comparison. It does
//! not hash or call the join visitor. Seeds are `0x2323`, `0x23a1`, `0x23b2`,
//! `0x23c3`, `0x23d4`, `0x23e5`, `0x23f6`, and `0x2307`.

use rustdb::{Database, StatementResult, Value};

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

fn eq_true(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        _ => false,
    }
}

fn logical_rows(
    outer: &[Vec<Value>],
    inner: &[Vec<Value>],
    inner_width: usize,
    on: impl Fn(&[Value], &[Value]) -> bool,
    keep: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let nulls = vec![Value::Null; inner_width];
    let mut rows = Vec::new();
    for outer_row in outer {
        let mut matched = false;
        for inner_row in inner {
            if on(outer_row, inner_row) {
                matched = true;
                if keep(outer_row, inner_row) {
                    let mut row = outer_row.clone();
                    row.extend(inner_row.iter().cloned());
                    rows.push(row);
                }
            }
        }
        if !matched && keep(outer_row, &nulls) {
            let mut row = outer_row.clone();
            row.extend(nulls.iter().cloned());
            rows.push(row);
        }
    }
    rows
}

fn project(rows: &[Vec<Value>], indexes: &[usize]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| indexes.iter().map(|index| row[*index].clone()).collect())
        .collect()
}

fn distinct_rows(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for row in rows {
        if !kept.iter().any(|seen| seen == &row) {
            kept.push(row);
        }
    }
    kept
}

fn sort_name_amount(rows: &mut [Vec<Value>]) {
    rows.sort_by(|left, right| {
        let name = cmp_cell(&left[0], &right[0], true, false);
        name.then_with(|| cmp_cell(&left[1], &right[1], true, false))
    });
}

fn cmp_cell(left: &Value, right: &Value, ascending: bool, nulls_first: bool) -> std::cmp::Ordering {
    let order = match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => {
            if nulls_first {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }
        (_, Value::Null) => {
            if nulls_first {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Less
            }
        }
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    };
    if ascending { order } else { order.reverse() }
}

#[test]
fn matched_and_unmatched_projections_collapse() {
    let users = vec![
        vec![integer(1), text("Ada"), Value::Boolean(true)],
        vec![integer(2), text("Ada"), Value::Boolean(true)],
        vec![integer(4), text("Ada"), Value::Boolean(false)],
        vec![integer(3), text("Bea"), Value::Boolean(true)],
        vec![Value::Null, text("Dee"), Value::Boolean(true)],
    ];
    let orders = vec![
        vec![integer(1), integer(10)],
        vec![integer(1), integer(10)],
        vec![integer(1), Value::Null],
        vec![integer(3), integer(20)],
        vec![Value::Null, integer(5)],
    ];
    let logical = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    let expected = distinct_rows(project(&logical, &[1, 4]));
    assert_eq!(
        expected,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(20)],
            vec![text("Dee"), Value::Null],
        ]
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada', TRUE), (2, 'Ada', TRUE), (4, 'Ada', FALSE), (3, 'Bea', TRUE), (NULL, 'Dee', TRUE);")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 10), (1, NULL), (3, 20), (NULL, 5);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        expected
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        distinct_rows(project(&logical, &[4]))
    );
    let names = distinct_rows(project(
        &logical_rows(
            &users,
            &orders,
            2,
            |user, order| eq_true(&user[0], &order[0]),
            |_, order| matches!(order[1], Value::Integer(amount) if amount > 15),
        ),
        &[1],
    ));
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 15;",
        ),
        names
    );
    assert_eq!(names, vec![vec![text("Bea")]]);
    let false_on = distinct_rows(project(
        &logical_rows(&users, &orders, 2, |_, _| false, |_, _| true),
        &[1],
    ));
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id AND FALSE;",
        ),
        false_on
    );
    let hidden = failure(
        &mut database,
        "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY o.amount;",
    );
    assert!(
        hidden.contains("must appear in the SELECT DISTINCT list"),
        "{hidden}"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY o.amount LIMIT 1;",
        ),
        vec![vec![text("Ada")]]
    );
}

#[test]
fn stars_aliases_and_limit_zero_bind() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, name TEXT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 'Ada', 10), (1, 'Ada', 10), (9, 'Cy', NULL);")
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Bea")],
    ];
    let orders = vec![
        vec![integer(1), text("Ada"), integer(10)],
        vec![integer(1), text("Ada"), integer(10)],
        vec![integer(9), text("Cy"), Value::Null],
    ];
    let logical = logical_rows(
        &users,
        &orders,
        3,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.*, o.* FROM users u LEFT JOIN orders o ON u.id = o.id;",
        ),
        distinct_rows(project(&logical, &[0, 1, 2, 3, 4]))
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.*, u.* FROM users u LEFT JOIN orders o ON u.id = o.id;",
        ),
        distinct_rows(project(&logical, &[2, 3, 4, 0, 1]))
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.* FROM users u LEFT JOIN orders o ON u.id = o.id;",
        ),
        distinct_rows(project(&logical, &[0, 1]))
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.* FROM users u LEFT JOIN orders o ON u.id = o.id;",
        ),
        distinct_rows(project(&logical, &[2, 3, 4]))
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT name FROM users u LEFT JOIN orders o ON u.id = o.id;",
        ),
        "column 'name' is ambiguous"
    );
    assert!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name AS n, o.amount AS n FROM users u LEFT JOIN orders o ON u.id = o.id;",
        )
        .contains("duplicate column alias")
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT DISTINCT u.name AS n, o.amount AS a FROM users u LEFT JOIN orders o ON u.id = o.id WHERE u.name = 'Ada' ORDER BY n DESC NULLS FIRST, a ASC NULLS LAST LIMIT 0;",
    );
    assert_eq!(
        explained.rows[0][0],
        Value::Text("HashLeftJoin".to_string())
    );
    assert_eq!(
        explained.rows[0][7],
        Value::Text("HashDistinct".to_string())
    );
    assert_eq!(
        explained.rows[0][8],
        Value::Text("u.name AS n, o.amount AS a".to_string())
    );
    assert_eq!(explained.rows[0][10], Value::Integer(0));
    assert!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name AS n FROM users u LEFT JOIN orders o ON u.id = o.id LIMIT 0;",
        )
        .is_empty()
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.id WHERE o.amount > 'x' LIMIT 0;",
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.id GROUP BY u.name;",
        ),
        "DISTINCT is not supported with GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.id HAVING u.name = 'Ada';",
        ),
        "DISTINCT is not supported with HAVING"
    );
    assert!(!rows(
        &mut database,
        "SELECT DISTINCT u.name AS n, o.amount AS a FROM users u LEFT JOIN orders o ON u.id = o.id ORDER BY n ASC, o.amount DESC NULLS FIRST;",
    )
    .is_empty());
}

#[test]
fn seeded_left_distinct_matches_the_walker() {
    let seeds = [
        0x2323_u64, 0x23a1, 0x23b2, 0x23c3, 0x23d4, 0x23e5, 0x23f6, 0x2307,
    ];
    let sizes = [(0, 0), (0, 4), (4, 0), (3, 12), (12, 3), (8, 8)];
    for seed in seeds {
        for (users, orders) in sizes {
            let mut state = seed;
            let mut next = || {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                state
            };
            let outer = (0..users)
                .map(|_| {
                    let key = match next() % 5 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap() - 1),
                    };
                    let name = text(&format!(
                        "{}{}",
                        (b'A' + u8::try_from(next() % 3).unwrap()) as char,
                        "xyz".repeat(usize::try_from(next() % 3).unwrap())
                    ));
                    vec![key, name]
                })
                .collect::<Vec<_>>();
            let inner = (0..orders)
                .map(|_| {
                    let key = match next() % 5 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap() - 1),
                    };
                    let amount = match next() % 4 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap()),
                    };
                    vec![key, amount]
                })
                .collect::<Vec<_>>();
            let logical = logical_rows(
                &outer,
                &inner,
                2,
                |user, order| eq_true(&user[0], &order[0]),
                |_, _| true,
            );
            let expected = distinct_rows(project(&logical, &[1, 3]));
            let mut database = Database::new();
            database
                .execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT);")
                .unwrap();
            insert(&mut database, "users", &outer);
            insert(&mut database, "orders", &inner);
            let label = format!("seed={seed:#x} users={users} orders={orders}");
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT DISTINCT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
                ),
                expected,
                "{label}"
            );
            let mut ordered = expected;
            sort_name_amount(&mut ordered);
            let limited = ordered.into_iter().take(2).collect::<Vec<_>>();
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT DISTINCT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY u.name ASC, o.amount ASC LIMIT 2;",
                ),
                limited,
                "{label} order"
            );
        }
    }
}

#[test]
fn boolean_case_and_repeated_columns_stay_distinct() {
    let flags = vec![
        vec![Value::Boolean(true), text("Ada")],
        vec![Value::Boolean(true), text("ada")],
        vec![Value::Boolean(false), text("Bea")],
        vec![Value::Null, text("Ada")],
    ];
    let checks = vec![
        vec![Value::Boolean(true), integer(1)],
        vec![Value::Boolean(true), integer(1)],
        vec![Value::Null, integer(2)],
    ];
    let logical = logical_rows(
        &flags,
        &checks,
        2,
        |flag, check| eq_true(&flag[0], &check[0]),
        |_, _| true,
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE flags (ok BOOLEAN, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE checks (ok BOOLEAN, n INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO flags VALUES (TRUE, 'Ada'), (TRUE, 'ada'), (FALSE, 'Bea'), (NULL, 'Ada');",
        )
        .unwrap();
    database
        .execute("INSERT INTO checks VALUES (TRUE, 1), (TRUE, 1), (NULL, 2);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT f.name, f.ok, c.n, c.n FROM flags f LEFT JOIN checks c ON f.ok = c.ok;",
        ),
        distinct_rows(project(&logical, &[1, 0, 3, 3]))
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT f.name FROM flags f LEFT JOIN checks c ON f.ok = c.ok;",
        ),
        vec![vec![text("Ada")], vec![text("ada")], vec![text("Bea")]]
    );
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
            sql.push_str(&match cell {
                Value::Null => "NULL".to_string(),
                Value::Integer(value) => value.to_string(),
                Value::Text(value) => format!("'{value}'"),
                Value::Boolean(true) => "TRUE".to_string(),
                Value::Boolean(false) => "FALSE".to_string(),
                Value::Ratio { .. } => panic!("a ratio is not stored"),
            });
        }
        sql.push(')');
    }
    sql.push(';');
    database.execute(&sql).unwrap();
}

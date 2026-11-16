//! `INNER JOIN` results checked against an independent walker.
//!
//! The walker visits the outer table, then the inner table, in insertion
//! order. It does not hash, probe an index, or call the engine join. Seeds
//! are `0x2121`, `0x21a1`, `0x21b2`, `0x21c3`, `0x21d4`, `0x21e5`, `0x21f6`,
//! and `0x2107`.

use std::cmp::Ordering;

use rustdb::{Database, StatementResult, Value};

#[path = "support/temp_dir.rs"]
mod temp_dir;

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

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn cell_sql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Integer(value) => value.to_string(),
        Value::Boolean(true) => "TRUE".to_string(),
        Value::Boolean(false) => "FALSE".to_string(),
        Value::Text(value) => format!("'{value}'"),
        Value::Ratio { .. } => panic!("a ratio is not a stored cell"),
    }
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
            sql.push_str(&cell_sql(cell));
        }
        sql.push(')');
    }
    sql.push(';');
    database.execute(&sql).unwrap();
}

/// True only for a same-type non-NULL equality. NULL and a type mismatch are not TRUE.
fn eq_true(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        _ => false,
    }
}

fn gt_true(left: &Value, right: i64) -> bool {
    matches!(left, Value::Integer(value) if *value > right)
}

fn pairs(
    outer: &[Vec<Value>],
    inner: &[Vec<Value>],
    keep: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<(usize, usize)> {
    let mut matched = Vec::new();
    for (outer_index, outer_row) in outer.iter().enumerate() {
        for (inner_index, inner_row) in inner.iter().enumerate() {
            if keep(outer_row, inner_row) {
                matched.push((outer_index, inner_index));
            }
        }
    }
    matched
}

fn project(
    outer: &[Vec<Value>],
    inner: &[Vec<Value>],
    matched: &[(usize, usize)],
    indexes: &[(bool, usize)],
) -> Vec<Vec<Value>> {
    matched
        .iter()
        .map(|(outer_index, inner_index)| {
            indexes
                .iter()
                .map(|(from_outer, index)| {
                    if *from_outer {
                        outer[*outer_index][*index].clone()
                    } else {
                        inner[*inner_index][*index].clone()
                    }
                })
                .collect()
        })
        .collect()
}

fn distinct(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for row in rows {
        if !kept.contains(row) {
            kept.push(row.clone());
        }
    }
    kept
}

fn cmp_asc(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => Ordering::Equal,
    }
}

fn sort_limit(rows: Vec<Vec<Value>>, limit: usize) -> Vec<Vec<Value>> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|left, right| {
        for (left_cell, right_cell) in rows[*left].iter().zip(rows[*right].iter()) {
            let compared = cmp_asc(left_cell, right_cell);
            if compared != Ordering::Equal {
                return compared;
            }
        }
        left.cmp(right)
    });
    order
        .into_iter()
        .take(limit)
        .map(|index| rows[index].clone())
        .collect()
}

fn ratio(numerator: i128, denominator: i64) -> Value {
    assert!(denominator > 0);
    if numerator == 0 {
        return Value::Ratio {
            numerator: 0,
            denominator: 1,
        };
    }
    let negative = numerator < 0;
    let mut num = numerator.unsigned_abs();
    let mut den = u128::from(denominator as u64);
    let mut divisor = gcd(num, den);
    num /= divisor;
    den /= divisor;
    while {
        divisor = gcd(num, den);
        divisor > 1
    } {
        num /= divisor;
        den /= divisor;
    }
    Value::Ratio {
        numerator: if negative {
            -i128::try_from(num).unwrap()
        } else {
            i128::try_from(num).unwrap()
        },
        denominator: i64::try_from(den).unwrap(),
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

struct Gen {
    state: u64,
}

impl Gen {
    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        self.state
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    fn int_cell(&mut self) -> Value {
        match self.below(6) {
            0 => Value::Null,
            other => Value::Integer(i64::try_from(other).unwrap() - 1),
        }
    }

    fn text_cell(&mut self) -> Value {
        match self.below(6) {
            0 => Value::Null,
            other => {
                let letter = (b'a' + u8::try_from(other).unwrap()) as char;
                text(&letter.to_string())
            }
        }
    }

    fn bool_cell(&mut self) -> Value {
        match self.below(3) {
            0 => Value::Null,
            1 => Value::Boolean(false),
            _ => Value::Boolean(true),
        }
    }
}

fn load_pair(kind: &str, outer: &[Vec<Value>], inner: &[Vec<Value>]) -> Database {
    let mut database = Database::new();
    let (outer_sql, inner_sql) = match kind {
        "int" => (
            "CREATE TABLE users (id INT, name TEXT);",
            "CREATE TABLE orders (user_id INT, amount INT);",
        ),
        "text" => (
            "CREATE TABLE users (id TEXT, name TEXT);",
            "CREATE TABLE orders (user_id TEXT, amount TEXT);",
        ),
        "bool" => (
            "CREATE TABLE users (id BOOLEAN, name TEXT);",
            "CREATE TABLE orders (user_id BOOLEAN, amount INT);",
        ),
        _ => panic!("unknown kind"),
    };
    database.execute(outer_sql).unwrap();
    database.execute(inner_sql).unwrap();
    insert(&mut database, "users", outer);
    insert(&mut database, "orders", inner);
    database
}

fn assert_engine(kind: &str, outer: &[Vec<Value>], inner: &[Vec<Value>], label: &str) {
    let matched = pairs(outer, inner, |outer_row, inner_row| {
        eq_true(&outer_row[0], &inner_row[0])
    });
    let projected = project(outer, inner, &matched, &[(true, 1), (false, 1)]);
    let mut database = load_pair(kind, outer, inner);
    let sql = "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;";
    assert_eq!(rows(&mut database, sql), projected, "{label}");
    let reversed = "SELECT u.name, o.amount FROM users u JOIN orders o ON o.user_id = u.id;";
    assert_eq!(rows(&mut database, reversed), projected, "{label} reversed");
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        distinct(&project(outer, inner, &matched, &[(true, 1)])),
        "{label} distinct"
    );
    let count = i64::try_from(matched.len()).unwrap();
    let amounts: Vec<i64> = matched
        .iter()
        .filter_map(|(_, index)| match &inner[*index][1] {
            Value::Integer(amount) => Some(*amount),
            _ => None,
        })
        .collect();
    let sum = amounts
        .iter()
        .fold(0_i128, |sum, amount| sum + i128::from(*amount));
    let (sum_cell, avg_cell, min_cell, max_cell) = if amounts.is_empty() {
        (Value::Null, Value::Null, Value::Null, Value::Null)
    } else {
        (
            Value::Integer(i64::try_from(sum).unwrap()),
            ratio(sum, i64::try_from(amounts.len()).unwrap()),
            Value::Integer(*amounts.iter().min().unwrap()),
            Value::Integer(*amounts.iter().max().unwrap()),
        )
    };
    if kind == "bool" || kind == "int" {
        assert_eq!(
            rows(
                &mut database,
                "SELECT COUNT(*), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount) FROM users u JOIN orders o ON u.id = o.user_id;"
            ),
            vec![vec![integer(count), sum_cell, avg_cell, min_cell, max_cell]],
            "{label} aggregate"
        );
    }
    let mut groups: Vec<(Value, i64)> = Vec::new();
    for (outer_index, _) in &matched {
        let name = outer[*outer_index][1].clone();
        if let Some(group) = groups.iter_mut().find(|(key, _)| key == &name) {
            group.1 += 1;
        } else {
            groups.push((name, 1));
        }
    }
    let grouped: Vec<Vec<Value>> = groups
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, count)| vec![name.clone(), integer(*count)])
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING COUNT(*) > 0;"
        ),
        grouped,
        "{label} group"
    );
    let limited = sort_limit(projected.clone(), 2);
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name ASC, o.amount ASC LIMIT 2;"
        ),
        limited,
        "{label} order"
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        explained.rows[0][0],
        Value::Text("HashJoin".to_string()),
        "{label} explain"
    );
}

fn shop() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
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

#[test]
fn the_walker_matches_the_hand_counted_shop() {
    let (users, orders) = shop();
    let matched = pairs(&users, &orders, |user, order| eq_true(&user[0], &order[0]));
    assert_eq!(
        project(&users, &orders, &matched, &[(true, 1), (false, 1)]),
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(10)],
        ]
    );
    let residual = pairs(&users, &orders, |user, order| {
        eq_true(&user[0], &order[0]) && gt_true(&order[1], 100)
    });
    assert_eq!(
        project(&users, &orders, &residual, &[(true, 1), (false, 1)]),
        vec![vec![text("Ada"), integer(150)]]
    );
    let either = pairs(&users, &orders, |user, order| {
        eq_true(&user[0], &order[0]) || gt_true(&order[1], 100)
    });
    assert_eq!(
        project(&users, &orders, &either, &[(true, 1), (false, 1)]),
        vec![
            vec![text("Ada"), integer(50)],
            vec![text("Ada"), integer(150)],
            vec![text("Ada"), Value::Null],
            vec![text("Bea"), integer(150)],
            vec![text("Bea"), integer(10)],
            vec![text("Cy"), integer(150)],
            vec![text("Dee"), integer(150)],
        ]
    );
}

#[test]
fn fixed_inner_joins_match_the_walker() {
    let (users, orders) = shop();
    assert_engine("int", &users, &orders, "shop");
    let mut database = load_pair("int", &users, &orders);
    let residual = pairs(&users, &orders, |user, order| {
        eq_true(&user[0], &order[0]) && gt_true(&order[1], 100)
    });
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 100;"
        ),
        project(&users, &orders, &residual, &[(true, 1), (false, 1)])
    );
    let either = pairs(&users, &orders, |user, order| {
        eq_true(&user[0], &order[0]) || matches!(order[1], Value::Integer(5))
    });
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id OR o.amount = 5;"
        ),
        project(&users, &orders, &either, &[(true, 1), (false, 1)])
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
    );
    assert_eq!(
        explained.rows[0][0],
        Value::Text("NestedLoopJoin".to_string())
    );
    let self_rows = pairs(&users, &users, |left, right| eq_true(&left[0], &right[0]));
    assert_eq!(
        rows(
            &mut database,
            "SELECT a.name, b.name FROM users a JOIN users b ON a.id = b.id;"
        ),
        project(&users, &users, &self_rows, &[(true, 1), (false, 1)])
    );

    let named_users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Ada")],
        vec![integer(3), text("Bea")],
    ];
    let named_orders = vec![
        vec![integer(1), text("Ada"), integer(10)],
        vec![integer(1), text("no"), integer(99)],
        vec![integer(2), text("Ada"), integer(5)],
    ];
    let mut named = Database::new();
    named
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    named
        .execute("CREATE TABLE orders (user_id INT, name TEXT, amount INT);")
        .unwrap();
    insert(&mut named, "users", &named_users);
    insert(&mut named, "orders", &named_orders);
    let both = pairs(&named_users, &named_orders, |user, order| {
        eq_true(&user[0], &order[0]) && eq_true(&user[1], &order[1])
    });
    assert_eq!(
        rows(
            &mut named,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id AND u.name = o.name;"
        ),
        project(&named_users, &named_orders, &both, &[(true, 1), (false, 2)])
    );
    assert_eq!(
        rows(
            &mut named,
            "SELECT SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id AND u.name = o.name;"
        ),
        vec![vec![integer(15)]]
    );

    let flags = vec![
        vec![Value::Boolean(true), text("a")],
        vec![Value::Boolean(false), text("b")],
        vec![Value::Null, text("c")],
    ];
    let checks = vec![
        vec![Value::Boolean(true), integer(1)],
        vec![Value::Boolean(true), integer(2)],
        vec![Value::Boolean(false), integer(3)],
        vec![Value::Null, integer(4)],
    ];
    assert_engine("bool", &flags, &checks, "boolean");

    let labels = vec![
        vec![text("a"), text("Ada")],
        vec![text("a"), text("Ann")],
        vec![text("b"), text("Bea")],
        vec![Value::Null, text("Dee")],
    ];
    let tags = vec![
        vec![text("a"), text("one")],
        vec![text("a"), text("two")],
        vec![text("c"), text("skip")],
        vec![Value::Null, text("none")],
    ];
    assert_engine("text", &labels, &tags, "text");

    let empty: Vec<Vec<Value>> = Vec::new();
    assert_engine("int", &empty, &orders, "empty outer");
    assert_engine("int", &users, &empty, "empty inner");
    assert_engine("int", &empty, &empty, "empty both");
}

#[test]
fn seeded_inner_joins_match_the_walker() {
    let seeds = [
        0x2121_u64, 0x21a1, 0x21b2, 0x21c3, 0x21d4, 0x21e5, 0x21f6, 0x2107,
    ];
    let sizes = [(0, 0), (0, 6), (6, 0), (8, 3), (3, 14), (24, 24)];
    for seed in seeds {
        for (users, orders) in sizes {
            for kind in ["int", "text", "bool"] {
                let mut generator = Gen { state: seed };
                let outer = (0..users)
                    .map(|_| {
                        vec![
                            match kind {
                                "int" => generator.int_cell(),
                                "text" => generator.text_cell(),
                                _ => generator.bool_cell(),
                            },
                            generator.text_cell(),
                        ]
                    })
                    .collect::<Vec<_>>();
                let inner = (0..orders)
                    .map(|_| {
                        vec![
                            match kind {
                                "int" => generator.int_cell(),
                                "text" => generator.text_cell(),
                                _ => generator.bool_cell(),
                            },
                            if kind == "text" {
                                generator.text_cell()
                            } else {
                                generator.int_cell()
                            },
                        ]
                    })
                    .collect::<Vec<_>>();
                assert_engine(
                    kind,
                    &outer,
                    &inner,
                    &format!("seed={seed:#x} users={users} orders={orders} kind={kind}"),
                );
            }
        }
    }
}

#[test]
fn explain_stays_read_only_and_a_reopen_keeps_the_join() {
    let (dir, path) = {
        let dir = temp_dir::KeepDir::new("m21-reopen");
        let path = dir.join("db");
        (dir, path)
    };
    let (users, orders) = shop();
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
        let before = std::fs::read(&path).unwrap();
        let explained = query(
            &mut database,
            "EXPLAIN SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let matched = pairs(&users, &orders, |user, order| eq_true(&user[0], &order[0]));
        assert_eq!(
            rows(
                &mut database,
                "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;"
            ),
            project(&users, &orders, &matched, &[(true, 1), (false, 1)])
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let mut reopened = Database::open(&path).unwrap();
    let matched = pairs(&users, &orders, |user, order| eq_true(&user[0], &order[0]));
    assert_eq!(
        rows(
            &mut reopened,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        project(&users, &orders, &matched, &[(true, 1), (false, 1)])
    );
    drop(reopened);
    drop(dir);
}

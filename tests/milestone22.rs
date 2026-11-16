//! `DISTINCT` aggregates over one `LEFT JOIN`, checked against an independent walker.
//!
//! The walker emits `ON`-true pairs or one null-extended row, then applies
//! `WHERE`, groups by typed equality, and keeps distinct values in a vector.
//! It does not hash, call the join visitor, or use `GroupSink`. Seeds are
//! `0x2222`, `0x22a1`, `0x22b2`, `0x22c3`, `0x22d4`, `0x22e5`, `0x22f6`, and
//! `0x2207`.

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

fn eq_true(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        _ => false,
    }
}

/// Logical rows in left-then-right insertion order.
///
/// `on` reports whether `ON` is TRUE. A left row with no TRUE `ON` result is
/// emitted once with `inner_width` NULLs when `keep_null` accepts that row.
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

#[derive(Clone)]
struct Totals {
    count_star: i64,
    count_amount: i64,
    count_distinct: Vec<Value>,
    sum: Option<i128>,
    sum_distinct: Vec<i64>,
}

impl Totals {
    fn new() -> Self {
        Self {
            count_star: 0,
            count_amount: 0,
            count_distinct: Vec::new(),
            sum: None,
            sum_distinct: Vec::new(),
        }
    }

    fn push_amount(&mut self, amount: &Value) {
        self.count_star += 1;
        let Value::Integer(amount) = amount else {
            return;
        };
        self.count_amount += 1;
        if !self.count_distinct.contains(&integer(*amount)) {
            self.count_distinct.push(integer(*amount));
        }
        self.sum = Some(self.sum.unwrap_or(0) + i128::from(*amount));
        if !self.sum_distinct.contains(amount) {
            self.sum_distinct.push(*amount);
        }
    }

    fn row(&self) -> Vec<Value> {
        let distinct = i64::try_from(self.count_distinct.len()).unwrap();
        let sum = self
            .sum
            .map(|sum| integer(i64::try_from(sum).unwrap()))
            .unwrap_or(Value::Null);
        let sum_distinct: i128 = self
            .sum_distinct
            .iter()
            .fold(0, |sum, amount| sum + i128::from(*amount));
        let sum_distinct_cell = if self.sum_distinct.is_empty() {
            Value::Null
        } else {
            integer(i64::try_from(sum_distinct).unwrap())
        };
        let avg = match (self.sum, self.count_amount) {
            (Some(sum), count) if count > 0 => ratio(sum, count),
            _ => Value::Null,
        };
        let avg_distinct = if self.sum_distinct.is_empty() {
            Value::Null
        } else {
            ratio(
                sum_distinct,
                i64::try_from(self.sum_distinct.len()).unwrap(),
            )
        };
        vec![
            integer(self.count_star),
            integer(self.count_amount),
            integer(distinct),
            sum,
            sum_distinct_cell,
            avg,
            avg_distinct,
        ]
    }
}

fn grouped(rows: &[Vec<Value>], ordinals: &[usize], amount_at: usize) -> Vec<Vec<Value>> {
    let mut keys = Vec::new();
    let mut totals: Vec<Totals> = Vec::new();
    for row in rows {
        let key = ordinals
            .iter()
            .map(|ordinal| row[*ordinal].clone())
            .collect::<Vec<_>>();
        if let Some(slot) = keys.iter().position(|stored| stored == &key) {
            totals[slot].push_amount(&row[amount_at]);
        } else {
            keys.push(key);
            let mut total = Totals::new();
            total.push_amount(&row[amount_at]);
            totals.push(total);
        }
    }
    keys.into_iter()
        .zip(totals)
        .map(|(key, total)| {
            let mut row = key;
            row.extend(total.row());
            row
        })
        .collect()
}

fn scalar(rows: &[Vec<Value>], amount_at: usize) -> Vec<Value> {
    let mut total = Totals::new();
    for row in rows {
        total.push_amount(&row[amount_at]);
    }
    total.row()
}

fn shop() -> (Database, Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Ada")],
        vec![integer(3), text("Bea")],
        vec![Value::Null, text("Dee")],
    ];
    let orders = vec![
        vec![integer(1), integer(10)],
        vec![integer(1), integer(10)],
        vec![integer(1), integer(20)],
        vec![integer(3), integer(10)],
        vec![Value::Null, integer(5)],
    ];
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada'), (3, 'Bea'), (NULL, 'Dee');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 10), (1, 20), (3, 10), (NULL, 5);")
        .unwrap();
    (database, users, orders)
}

#[test]
fn the_ada_group_matches_the_hand_count() {
    let (mut database, users, orders) = shop();
    let logical = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    let ada = logical
        .iter()
        .filter(|row| row[1] == text("Ada"))
        .cloned()
        .collect::<Vec<_>>();
    let mut total = Totals::new();
    for row in &ada {
        total.push_amount(&row[3]);
    }
    assert_eq!(total.count_star, 4);
    assert_eq!(total.count_amount, 3);
    assert_eq!(total.count_distinct.len(), 2);
    assert_eq!(total.sum, Some(40));
    assert_eq!(total.sum_distinct.iter().sum::<i64>(), 30);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount), AVG(o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.name = 'Ada';",
        ),
        vec![total.row()]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), COUNT(o.amount), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount), AVG(o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
        ),
        grouped(&logical, &[1], 3)
    );
}

#[test]
fn null_extensions_and_matched_nulls_are_not_distinct_members() {
    let mut database = Database::new();
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
        .execute("INSERT INTO orders VALUES (1, NULL);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), COUNT(DISTINCT o.amount), SUM(DISTINCT o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![
            integer(2),
            integer(0),
            integer(0),
            Value::Null,
            Value::Null,
        ]]
    );
    database.execute("DELETE FROM users;").unwrap();
    database.execute("DELETE FROM orders;").unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT o.amount), SUM(DISTINCT o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(0), Value::Null, Value::Null]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
        )
        .is_empty()
    );
}

#[test]
fn distinct_sets_stay_inside_their_groups() {
    let users = vec![vec![integer(1), text("Ada")], vec![integer(2), text("Bea")]];
    let orders = vec![
        vec![integer(1), integer(10)],
        vec![integer(2), integer(10)],
        vec![integer(2), integer(10)],
    ];
    let logical = logical_rows(
        &users,
        &orders,
        2,
        |user, order| eq_true(&user[0], &order[0]),
        |_, _| true,
    );
    let mut database = Database::new();
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
        .execute("INSERT INTO orders VALUES (1, 10), (2, 10), (2, 10);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), SUM(o.amount), SUM(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
        ),
        vec![
            vec![text("Ada"), integer(1), integer(10), integer(10)],
            vec![text("Bea"), integer(2), integer(20), integer(10)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT SUM(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(10)]]
    );
    let _ = logical;
}

#[test]
fn text_boolean_residual_and_aliases_follow_the_walker() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE flags (id BOOLEAN, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE checks (ok BOOLEAN, n INT);")
        .unwrap();
    database
        .execute("INSERT INTO flags VALUES (TRUE, 'A'), (TRUE, 'a'), (FALSE, 'B'), (NULL, 'C');")
        .unwrap();
    database
        .execute("INSERT INTO checks VALUES (TRUE, 1), (TRUE, 1), (FALSE, 2), (NULL, 3);")
        .unwrap();
    let flags = vec![
        vec![Value::Boolean(true), text("A")],
        vec![Value::Boolean(true), text("a")],
        vec![Value::Boolean(false), text("B")],
        vec![Value::Null, text("C")],
    ];
    let checks = vec![
        vec![Value::Boolean(true), integer(1)],
        vec![Value::Boolean(true), integer(1)],
        vec![Value::Boolean(false), integer(2)],
        vec![Value::Null, integer(3)],
    ];
    let logical = logical_rows(
        &flags,
        &checks,
        2,
        |flag, check| eq_true(&flag[0], &check[0]),
        |_, _| true,
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT f.name, COUNT(*), COUNT(DISTINCT c.n) FROM flags f LEFT JOIN checks c ON f.id = c.ok GROUP BY f.name;",
        ),
        grouped(&logical, &[1], 3)
            .into_iter()
            .map(|row| vec![row[0].clone(), row[1].clone(), row[3].clone()])
            .collect::<Vec<_>>()
    );
    assert!(
        failure(
            &mut database,
            "SELECT SUM(DISTINCT f.name) FROM flags f LEFT JOIN checks c ON f.id = c.ok;",
        )
        .contains("INTEGER")
    );
    assert!(
        failure(
            &mut database,
            "SELECT AVG(DISTINCT f.id) FROM flags f LEFT JOIN checks c ON f.id = c.ok;",
        )
        .contains("INTEGER")
    );
    assert!(
        failure(
            &mut database,
            "SELECT MIN(DISTINCT c.n) FROM flags f LEFT JOIN checks c ON f.id = c.ok;",
        )
        .contains("DISTINCT is not supported")
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT *) FROM flags f LEFT JOIN checks c ON f.id = c.ok;",
        ),
        "DISTINCT requires a column"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT f.name FROM flags f LEFT JOIN checks c ON f.id = c.ok;",
        ),
        "LEFT JOIN is not supported with DISTINCT"
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT f.name, COUNT(DISTINCT c.n) AS n FROM flags f LEFT JOIN checks c ON f.id = c.ok AND c.n > 0 GROUP BY f.name HAVING n > 0 ORDER BY n DESC LIMIT 1;",
    );
    assert_eq!(
        explained.rows[0][0],
        Value::Text("HashLeftJoin".to_string())
    );
    assert_eq!(
        explained.rows[0][8],
        Value::Text("COUNT(DISTINCT c.n)".to_string())
    );
    assert_eq!(explained.rows[0][9], Value::Text("n > 0".to_string()));
}

#[test]
fn seeded_left_distinct_matches_the_walker() {
    let seeds = [
        0x2222_u64, 0x22a1, 0x22b2, 0x22c3, 0x22d4, 0x22e5, 0x22f6, 0x2207,
    ];
    let sizes = [(0, 0), (0, 5), (5, 0), (8, 3), (3, 12), (16, 16)];
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
                    let name =
                        text(&((b'a' + u8::try_from(next() % 4).unwrap()) as char).to_string());
                    vec![key, name]
                })
                .collect::<Vec<_>>();
            let inner = (0..orders)
                .map(|_| {
                    let key = match next() % 5 {
                        0 => Value::Null,
                        other => integer(i64::try_from(other).unwrap() - 1),
                    };
                    let amount = match next() % 6 {
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
                    "SELECT COUNT(*), COUNT(o.amount), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount), AVG(o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
                ),
                vec![scalar(&logical, 3)],
                "{label}"
            );
            assert_eq!(
                rows(
                    &mut database,
                    "SELECT u.name, COUNT(*), COUNT(o.amount), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount), AVG(o.amount), AVG(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING COUNT(DISTINCT o.amount) >= 0 ORDER BY u.name ASC;",
                ),
                sort_name(grouped(&logical, &[1], 3)),
                "{label} group"
            );
        }
    }
}

fn sort_name(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by(|left, right| match (&left[0], &right[0]) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    });
    rows
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

#[test]
fn a_residual_and_a_where_do_not_share_null_extension() {
    let (mut database, users, orders) = shop();
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
            "SELECT COUNT(*), COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 15;",
        ),
        vec![vec![
            integer(i64::try_from(residual.len()).unwrap()),
            integer(1)
        ]]
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
            "SELECT COUNT(*), COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 15;",
        ),
        vec![vec![
            integer(i64::try_from(filtered.len()).unwrap()),
            integer(1)
        ]]
    );
    assert_eq!(filtered.len(), 1);
    assert!(residual.len() > filtered.len());
}

#[test]
fn binder_aliases_and_right_groups_follow_sql() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT, amount INT);")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT id) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        "column 'id' is ambiguous"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT MAX(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        "DISTINCT is not supported for MAX"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT u.name, COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        "column 'name' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x' LIMIT 0;",
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id LIMIT 0;",
        )
        .is_empty()
    );
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES (10, 1, NULL), (11, 1, 10), (12, 1, NULL), (13, 2, 10);",
        )
        .unwrap();
    let users = vec![vec![integer(1), text("Ada")], vec![integer(2), text("Bea")]];
    let orders = vec![
        vec![integer(10), integer(1), Value::Null],
        vec![integer(11), integer(1), integer(10)],
        vec![integer(12), integer(1), Value::Null],
        vec![integer(13), integer(2), integer(10)],
    ];
    let logical = logical_rows(
        &users,
        &orders,
        3,
        |user, order| eq_true(&user[0], &order[1]),
        |_, _| true,
    );
    let mut keys = Vec::new();
    let mut counts = Vec::new();
    let mut distinct_users: Vec<Vec<Value>> = Vec::new();
    for row in &logical {
        let key = row[4].clone();
        let user_id = row[3].clone();
        if let Some(slot) = keys.iter().position(|stored| stored == &key) {
            counts[slot] += 1;
            if !user_id.is_null() && !distinct_users[slot].contains(&user_id) {
                distinct_users[slot].push(user_id);
            }
        } else {
            keys.push(key);
            counts.push(1_i64);
            distinct_users.push(if user_id.is_null() {
                Vec::new()
            } else {
                vec![user_id]
            });
        }
    }
    let expected = keys
        .iter()
        .zip(counts)
        .zip(distinct_users)
        .map(|((key, count), users)| {
            vec![
                key.clone(),
                integer(count),
                integer(i64::try_from(users.len()).unwrap()),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows(
            &mut database,
            "SELECT o.amount, COUNT(*), COUNT(DISTINCT o.user_id) AS n FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY o.amount HAVING n >= 0 ORDER BY n DESC, o.amount ASC;",
        ),
        {
            let mut sorted = expected;
            sorted.sort_by(|left, right| {
                let count = match (&left[2], &right[2]) {
                    (Value::Integer(left), Value::Integer(right)) => right.cmp(left),
                    _ => std::cmp::Ordering::Equal,
                };
                count.then_with(|| match (&left[0], &right[0]) {
                    (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
                    (Value::Null, _) => std::cmp::Ordering::Greater,
                    (_, Value::Null) => std::cmp::Ordering::Less,
                    (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
                    _ => std::cmp::Ordering::Equal,
                })
            });
            sorted
        }
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(DISTINCT o.amount), COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        explained.rows[0][8],
        Value::Text("COUNT(DISTINCT o.amount)".to_string())
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT o.amount), COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(1), integer(1)]]
    );
}

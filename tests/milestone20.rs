//! Aggregates over one `LEFT JOIN`, checked against an independent walker.
//!
//! The walker visits left rows, then right rows, in insertion order. It does
//! not use the engine hash table, the join visitor, or the group accumulator.
//! `AVG` is compared as a reduced ratio. Seeds are `0x2020`, `0x20a1`,
//! `0x20b2`, `0x20c3`, `0x20d4`, `0x20e5`, `0x20f6`, `0x2007`, `0x2011`,
//! `0x20bb`, and `0x20cc`.

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

/// Independent reduction. The denominator stays positive.
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

fn shop_tables() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
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

fn is_true_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        _ => false,
    }
}

fn is_true_gt(left: &Value, right: i64) -> bool {
    matches!(left, Value::Integer(value) if *value > right)
}

struct Acc {
    star: i64,
    counted: i64,
    sum: Option<i64>,
    avg_sum: i128,
    avg_count: i64,
    min: Option<i64>,
    max: Option<i64>,
}

impl Acc {
    fn new() -> Self {
        Self {
            star: 0,
            counted: 0,
            sum: None,
            avg_sum: 0,
            avg_count: 0,
            min: None,
            max: None,
        }
    }

    fn push_amount(&mut self, amount: &Value) {
        self.star += 1;
        let Value::Integer(value) = amount else {
            return;
        };
        self.counted += 1;
        self.sum = Some(self.sum.unwrap_or(0) + value);
        self.avg_sum += i128::from(*value);
        self.avg_count += 1;
        self.min = Some(self.min.map_or(*value, |current| current.min(*value)));
        self.max = Some(self.max.map_or(*value, |current| current.max(*value)));
    }

    fn cells(&self) -> [Value; 6] {
        let sum = self.sum.map(Value::Integer).unwrap_or(Value::Null);
        let avg = if self.avg_count == 0 {
            Value::Null
        } else {
            ratio(self.avg_sum, self.avg_count)
        };
        [
            integer(self.star),
            integer(self.counted),
            sum,
            avg,
            self.min.map(Value::Integer).unwrap_or(Value::Null),
            self.max.map(Value::Integer).unwrap_or(Value::Null),
        ]
    }
}

fn logical_pairs(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    right_width: usize,
    on_true: impl Fn(&[Value], &[Value]) -> bool,
    where_true: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<(Vec<Value>, Vec<Value>)> {
    let nulls = vec![Value::Null; right_width];
    let mut out = Vec::new();
    for outer in left {
        let mut matched = false;
        for inner in right {
            if !on_true(outer, inner) {
                continue;
            }
            matched = true;
            if where_true(outer, inner) {
                out.push((outer.clone(), inner.clone()));
            }
        }
        if !matched && where_true(outer, &nulls) {
            out.push((outer.clone(), nulls.clone()));
        }
    }
    out
}

fn grouped(
    pairs: &[(Vec<Value>, Vec<Value>)],
    key_of: impl Fn(&[Value], &[Value]) -> Vec<Value>,
    amount_of: impl Fn(&[Value], &[Value]) -> Value,
) -> Vec<(Vec<Value>, Acc)> {
    let mut groups: Vec<(Vec<Value>, Acc)> = Vec::new();
    for (outer, inner) in pairs {
        let key = key_of(outer, inner);
        let amount = amount_of(outer, inner);
        if let Some((_, acc)) = groups.iter_mut().find(|(found, _)| found == &key) {
            acc.push_amount(&amount);
        } else {
            let mut acc = Acc::new();
            acc.push_amount(&amount);
            groups.push((key, acc));
        }
    }
    groups
}

fn eq_users(outer: &[Value], inner: &[Value]) -> bool {
    is_true_eq(&outer[0], &inner[0])
}

/// Direct comparison. NULL matches NULL. This is not the engine group hash.
fn same_key(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| match (left, right) {
                (Value::Null, Value::Null) => true,
                (Value::Integer(left), Value::Integer(right)) => left == right,
                (Value::Text(left), Value::Text(right)) => left == right,
                (Value::Boolean(left), Value::Boolean(right)) => left == right,
                _ => false,
            })
}

fn order_rows(rows: &mut [Vec<Value>]) {
    rows.sort_by(|left, right| {
        let count = match (&left[2], &right[2]) {
            (Value::Integer(left), Value::Integer(right)) => right.cmp(left),
            _ => std::cmp::Ordering::Equal,
        };
        if count != std::cmp::Ordering::Equal {
            return count;
        }
        match (&left[0], &right[0]) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Null, _) => std::cmp::Ordering::Greater,
            (_, Value::Null) => std::cmp::Ordering::Less,
            (Value::Text(left), Value::Text(right)) => left.cmp(right),
            _ => std::cmp::Ordering::Equal,
        }
    });
}

#[test]
fn mixed_groups_count_matches_and_null_extensions_together() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada'), (3, 'Bea'), (4, NULL), (5, NULL);",
        )
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 20), (1, NULL), (5, NULL);")
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Ada")],
        vec![integer(3), text("Bea")],
        vec![integer(4), Value::Null],
        vec![integer(5), Value::Null],
    ];
    let orders = vec![
        vec![integer(1), integer(10)],
        vec![integer(1), integer(20)],
        vec![integer(1), Value::Null],
        vec![integer(5), Value::Null],
    ];
    let pairs = logical_pairs(&users, &orders, 2, eq_users, |_, _| true);
    struct Tally {
        key: Vec<Value>,
        star: i64,
        ids: i64,
        names: i64,
        right_ids: i64,
        sum: Option<i64>,
        avg_sum: i128,
        avg_count: i64,
        min: Option<i64>,
        max: Option<i64>,
    }
    let mut groups: Vec<Tally> = Vec::new();
    for (outer, inner) in &pairs {
        let key = vec![outer[1].clone()];
        let amount = &inner[1];
        let slot = if let Some(found) = groups.iter_mut().find(|found| same_key(&found.key, &key)) {
            found
        } else {
            groups.push(Tally {
                key,
                star: 0,
                ids: 0,
                names: 0,
                right_ids: 0,
                sum: None,
                avg_sum: 0,
                avg_count: 0,
                min: None,
                max: None,
            });
            groups.last_mut().unwrap()
        };
        slot.star += 1;
        if !outer[0].is_null() {
            slot.ids += 1;
        }
        if !outer[1].is_null() {
            slot.names += 1;
        }
        if !inner[0].is_null() {
            slot.right_ids += 1;
        }
        if let Value::Integer(value) = amount {
            slot.sum = Some(slot.sum.unwrap_or(0) + value);
            slot.avg_sum += i128::from(*value);
            slot.avg_count += 1;
            slot.min = Some(slot.min.map_or(*value, |current| current.min(*value)));
            slot.max = Some(slot.max.map_or(*value, |current| current.max(*value)));
        }
    }
    let expect: Vec<_> = groups
        .iter()
        .map(|group| {
            let avg = if group.avg_count == 0 {
                Value::Null
            } else {
                ratio(group.avg_sum, group.avg_count)
            };
            vec![
                group.key[0].clone(),
                integer(group.star),
                integer(group.ids),
                integer(group.names),
                integer(group.right_ids),
                group.sum.map(Value::Integer).unwrap_or(Value::Null),
                avg,
                group.min.map(Value::Integer).unwrap_or(Value::Null),
                group.max.map(Value::Integer).unwrap_or(Value::Null),
            ]
        })
        .collect();
    let got = rows(
        &mut database,
        "SELECT u.name, COUNT(*), COUNT(u.id), COUNT(u.name), COUNT(o.user_id),
                SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount)
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         GROUP BY u.name;",
    );
    assert_eq!(got, expect);
    assert_eq!(
        expect,
        vec![
            vec![
                text("Ada"),
                integer(4),
                integer(4),
                integer(4),
                integer(3),
                integer(30),
                ratio(30, 2),
                integer(10),
                integer(20),
            ],
            vec![
                text("Bea"),
                integer(1),
                integer(1),
                integer(1),
                integer(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Null,
                integer(2),
                integer(2),
                integer(0),
                integer(1),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ]
    );
    let filtered = logical_pairs(&users, &orders, 2, eq_users, |_, inner| {
        is_true_gt(&inner[1], 15)
    });
    assert_eq!(filtered.len(), 1);
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), SUM(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 15
             GROUP BY u.name;",
        ),
        vec![vec![text("Ada"), integer(1), integer(20)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT o.amount, COUNT(*), COUNT(o.amount), COUNT(o.user_id), SUM(o.user_id)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY o.amount;",
        ),
        vec![
            vec![integer(10), integer(1), integer(1), integer(1), integer(1)],
            vec![integer(20), integer(1), integer(1), integer(1), integer(1)],
            vec![Value::Null, integer(5), integer(0), integer(2), integer(6)],
        ]
    );
}

#[test]
fn text_boolean_and_two_equalities_follow_the_walker() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE flags (id BOOLEAN, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE checks (id BOOLEAN, ok BOOLEAN);")
        .unwrap();
    database
        .execute("INSERT INTO flags VALUES (TRUE, 'a'), (FALSE, 'a'), (NULL, 'b'), (TRUE, 'b');")
        .unwrap();
    database
        .execute("INSERT INTO checks VALUES (TRUE, TRUE), (TRUE, FALSE), (FALSE, NULL);")
        .unwrap();
    let flags = vec![
        vec![Value::Boolean(true), text("a")],
        vec![Value::Boolean(false), text("a")],
        vec![Value::Null, text("b")],
        vec![Value::Boolean(true), text("b")],
    ];
    let checks = vec![
        vec![Value::Boolean(true), Value::Boolean(true)],
        vec![Value::Boolean(true), Value::Boolean(false)],
        vec![Value::Boolean(false), Value::Null],
    ];
    let pairs = logical_pairs(
        &flags,
        &checks,
        2,
        |outer, inner| is_true_eq(&outer[0], &inner[0]),
        |_, _| true,
    );
    struct FlagTally {
        key: Value,
        star: i64,
        ids: i64,
        oks: i64,
        min_ok: Option<bool>,
        max_ok: Option<bool>,
    }
    let mut groups: Vec<FlagTally> = Vec::new();
    for (outer, inner) in &pairs {
        let key = outer[1].clone();
        let slot = if let Some(found) = groups.iter_mut().find(|found| found.key == key) {
            found
        } else {
            groups.push(FlagTally {
                key,
                star: 0,
                ids: 0,
                oks: 0,
                min_ok: None,
                max_ok: None,
            });
            groups.last_mut().unwrap()
        };
        slot.star += 1;
        if !outer[0].is_null() {
            slot.ids += 1;
        }
        if let Value::Boolean(value) = inner[1] {
            slot.oks += 1;
            slot.min_ok = Some(slot.min_ok.map_or(value, |current| current && value));
            slot.max_ok = Some(slot.max_ok.map_or(value, |current| current || value));
        }
    }
    let expect: Vec<_> = groups
        .iter()
        .map(|group| {
            vec![
                group.key.clone(),
                integer(group.star),
                integer(group.ids),
                integer(group.oks),
                group.min_ok.map(Value::Boolean).unwrap_or(Value::Null),
                group.max_ok.map(Value::Boolean).unwrap_or(Value::Null),
            ]
        })
        .collect();
    let got = rows(
        &mut database,
        "SELECT f.name, COUNT(*), COUNT(f.id), COUNT(c.ok), MIN(c.ok), MAX(c.ok)
         FROM flags f LEFT JOIN checks c ON f.id = c.id
         GROUP BY f.name;",
    );
    assert_eq!(got, expect);
    assert_eq!(
        expect,
        vec![
            vec![
                text("a"),
                integer(3),
                integer(3),
                integer(2),
                Value::Boolean(false),
                Value::Boolean(true),
            ],
            vec![
                text("b"),
                integer(3),
                integer(2),
                integer(2),
                Value::Boolean(false),
                Value::Boolean(true),
            ],
        ]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM flags f LEFT JOIN checks c ON f.id = c.id OR c.ok = TRUE;",
    );
    assert_eq!(explained.rows[0][0], text("LeftNestedLoopJoin"));
    let or_pairs = logical_pairs(
        &flags,
        &checks,
        2,
        |outer, inner| is_true_eq(&outer[0], &inner[0]) || matches!(inner[1], Value::Boolean(true)),
        |_, _| true,
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(c.ok) FROM flags f LEFT JOIN checks c ON f.id = c.id OR c.ok = TRUE;",
        ),
        vec![vec![integer(or_pairs.len() as i64), {
            let counted = or_pairs
                .iter()
                .filter(|(_, inner)| !inner[1].is_null())
                .count() as i64;
            integer(counted)
        }]]
    );

    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, name TEXT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada'), (3, 'Bea');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 'Ada', 10), (1, 'no', 99), (2, 'Ada', 5);")
        .unwrap();
    let users = vec![
        vec![integer(1), text("Ada")],
        vec![integer(2), text("Ada")],
        vec![integer(3), text("Bea")],
    ];
    let orders = vec![
        vec![integer(1), text("Ada"), integer(10)],
        vec![integer(1), text("no"), integer(99)],
        vec![integer(2), text("Ada"), integer(5)],
    ];
    let both = logical_pairs(
        &users,
        &orders,
        3,
        |outer, inner| is_true_eq(&outer[0], &inner[0]) && is_true_eq(&outer[1], &inner[1]),
        |_, _| true,
    );
    let mut sum = 0_i64;
    let mut counted = 0_i64;
    for (_, inner) in &both {
        if let Value::Integer(amount) = inner[2] {
            sum += amount;
            counted += 1;
        }
    }
    assert_eq!(both.len(), 3);
    assert_eq!(sum, 15);
    assert_eq!(counted, 2);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.name;",
        ),
        vec![vec![integer(3), integer(2), integer(15)]]
    );
    let plan = query(
        &mut database,
        "EXPLAIN SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.name GROUP BY u.name;",
    );
    assert_eq!(plan.rows[0][0], text("HashLeftJoin"));
    assert_eq!(plan.rows[0][4], text("u.id = o.user_id"));
    let Value::Text(residual) = &plan.rows[0][5] else {
        panic!("residual {}", plan.rows[0][5]);
    };
    assert!(residual.contains("u.name = o.name"), "{residual}");
    assert_eq!(plan.rows[0][7], text("u.name"));
    assert_eq!(plan.rows[0][8], text("COUNT(*)"));
}

#[test]
fn exact_averages_ignore_null_and_keep_a_repeating_fraction() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 1), (1, 2), (1, -2), (1, NULL);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![
            integer(5),
            integer(3),
            integer(1),
            ratio(1, 3),
            integer(-2),
            integer(2),
        ]]
    );
    database.execute("DELETE FROM orders;").unwrap();
    database.execute("DELETE FROM users;").unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount), AVG(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(3), integer(0), Value::Null, Value::Null]]
    );
    database
        .execute("INSERT INTO orders VALUES (1, NULL);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), AVG(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 1;",
        ),
        vec![vec![integer(1), integer(0), Value::Null]]
    );
}

#[test]
fn having_aliases_and_bad_bindings_fail_on_an_empty_join() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name AS \"Who\", COUNT(*) AS \"N\", SUM(o.amount) AS \"Total\"
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING \"N\" > 1 AND \"Total\" > 10
             ORDER BY \"Total\" ASC;",
        ),
        vec![vec![text("Ada"), integer(3), integer(200)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) AS name
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING name > 1
             ORDER BY name DESC
             LIMIT 1;",
        ),
        vec![vec![text("Ada"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING SUM(o.amount) > 100 ORDER BY SUM(o.amount) DESC;",
        ),
        vec![vec![text("Ada")]]
    );
    let error = failure(
        &mut database,
        "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id HAVING missing > 1 LIMIT 0;",
    );
    assert!(error.contains("missing"), "{error}");
    let error = failure(
        &mut database,
        "SELECT COUNT(*) AS n FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY other LIMIT 0;",
    );
    assert!(error.contains("other"), "{error}");
    let error = failure(
        &mut database,
        "SELECT SUM(u.name) FROM users u LEFT JOIN orders o ON u.id = o.user_id LIMIT 0;",
    );
    assert!(error.contains("INTEGER"), "{error}");
    let error = failure(
        &mut database,
        "SELECT * FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
    );
    assert_eq!(
        error,
        "SELECT * is not supported with GROUP BY or aggregates"
    );
    database.execute("DELETE FROM users;").unwrap();
    database.execute("DELETE FROM orders;").unwrap();
    let error = failure(
        &mut database,
        "SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id HAVING u.name = 'Ada' LIMIT 0;",
    );
    assert!(error.contains("must appear in GROUP BY"), "{error}");
    let error = failure(
        &mut database,
        "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x';",
    );
    assert!(
        error.contains("INTEGER") && error.contains("TEXT"),
        "{error}"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(0), Value::Null]]
    );
}

#[test]
fn count_star_counts_null_extensions_and_count_column_skips_null() {
    let mut database = shop();
    let (users, orders) = shop_tables();
    let pairs = logical_pairs(&users, &orders, 2, eq_users, |_, _| true);
    let mut total = Acc::new();
    for (_, inner) in &pairs {
        total.push_amount(&inner[1]);
    }
    assert_eq!(total.star, 6);
    assert_eq!(total.counted, 3);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.user_id), COUNT(o.amount), COUNT(u.name),
                    SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![
            integer(6),
            integer(4),
            integer(3),
            integer(6),
            integer(210),
            ratio(210, 3),
            integer(10),
            integer(150),
        ]]
    );
    assert_eq!(total.cells()[0], integer(6));
    assert_eq!(total.cells()[2], integer(210));
    assert_eq!(total.cells()[3], ratio(70, 1));
}

#[test]
fn group_by_keeps_unmatched_rows_and_having_can_drop_them() {
    let mut database = shop();
    let (users, orders) = shop_tables();
    let pairs = logical_pairs(&users, &orders, 2, eq_users, |_, _| true);
    let groups = grouped(
        &pairs,
        |outer, _| vec![outer[1].clone()],
        |_, inner| inner[1].clone(),
    );
    let mut expect = Vec::new();
    for (key, acc) in &groups {
        let cells = acc.cells();
        expect.push(vec![
            key[0].clone(),
            cells[0].clone(),
            cells[1].clone(),
            cells[2].clone(),
        ]);
    }
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.name;",
        ),
        expect
    );
    assert_eq!(
        expect,
        vec![
            vec![text("Ada"), integer(3), integer(2), integer(200)],
            vec![text("Bea"), integer(1), integer(1), integer(10)],
            vec![text("Cy"), integer(1), integer(0), Value::Null],
            vec![text("Dee"), integer(1), integer(0), Value::Null],
        ]
    );
    let having: Vec<_> = expect
        .into_iter()
        .filter(|row| matches!(row[2], Value::Integer(count) if count > 0))
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) AS n, COUNT(o.amount) AS c, SUM(o.amount) AS total
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING c > 0
             ORDER BY total DESC;",
        ),
        {
            let mut ordered = having;
            ordered.sort_by(|left, right| match (&left[3], &right[3]) {
                (Value::Integer(left), Value::Integer(right)) => right.cmp(left),
                _ => panic!("having rows have sums"),
            });
            ordered
        }
    );
}

#[test]
fn a_right_null_key_is_one_group_and_where_does_not_invent_a_row() {
    let mut database = shop();
    let (users, orders) = shop_tables();
    let pairs = logical_pairs(&users, &orders, 2, eq_users, |_, _| true);
    let groups = grouped(
        &pairs,
        |_, inner| vec![inner[0].clone()],
        |_, inner| inner[1].clone(),
    );
    let expect: Vec<_> = groups
        .iter()
        .map(|(key, acc)| vec![key[0].clone(), integer(acc.star), integer(acc.counted)])
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT o.user_id, COUNT(*), COUNT(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             GROUP BY o.user_id;",
        ),
        expect
    );
    assert_eq!(expect.last().unwrap()[0], Value::Null);
    assert_eq!(expect.last().unwrap()[1], integer(2));
    let filtered = logical_pairs(&users, &orders, 2, eq_users, |_, inner| {
        is_true_gt(&inner[1], 100)
    });
    assert_eq!(filtered.len(), 1);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 100;",
        ),
        vec![vec![integer(1), integer(150)]]
    );
    let residual = logical_pairs(
        &users,
        &orders,
        2,
        |outer, inner| eq_users(outer, inner) && is_true_gt(&inner[1], 100),
        |_, _| true,
    );
    assert_eq!(residual.len(), 4);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 100;",
        ),
        vec![vec![integer(4), integer(1), integer(150)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*)
             FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount = NULL;",
        ),
        vec![vec![integer(4)]]
    );
}

#[test]
fn empty_inputs_and_all_null_arguments_follow_scalar_rules() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    let scalar = "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount)
                  FROM users u LEFT JOIN orders o ON u.id = o.user_id;";
    assert_eq!(
        rows(&mut database, scalar),
        vec![vec![
            integer(0),
            integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
        )
        .is_empty()
    );
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    assert_eq!(
        rows(&mut database, scalar),
        vec![vec![
            integer(2),
            integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
    database.execute("DELETE FROM users;").unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, NULL);")
        .unwrap();
    assert_eq!(
        rows(&mut database, scalar)[0][0],
        integer(0),
        "an empty left table ignores the right table"
    );
    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    assert_eq!(
        rows(&mut database, scalar),
        vec![vec![
            integer(1),
            integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]]
    );
    database.execute("DELETE FROM orders;").unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 1000;",
        ),
        vec![vec![integer(0), Value::Null]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 1000 GROUP BY u.name;",
        )
        .is_empty()
    );
}

#[test]
fn aliases_self_joins_and_illegal_projections_bind_before_the_scan() {
    let mut database = shop();
    let error = failure(
        &mut database,
        "SELECT id, COUNT(*) FROM users a LEFT JOIN users b ON a.id = b.id GROUP BY id;",
    );
    assert!(error.contains("ambiguous"), "{error}");
    let error = failure(
        &mut database,
        "SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert!(error.contains("must appear in GROUP BY"), "{error}");
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        vec![vec![integer(3)]]
    );
    let error = failure(
        &mut database,
        "SELECT DISTINCT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(error, "LEFT JOIN is not supported with DISTINCT");
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(b.id) FROM users a LEFT JOIN users b ON a.id = b.id;",
        ),
        vec![vec![integer(4), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id LIMIT 0;",
        ),
        Vec::<Vec<Value>>::new()
    );
    let error = failure(
        &mut database,
        "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 'x' LIMIT 0;",
    );
    assert!(
        error.contains("INTEGER") && error.contains("TEXT"),
        "{error}"
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, COUNT(*) AS n, SUM(o.amount)
         FROM users u LEFT JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 15
         GROUP BY u.name
         HAVING n > 0
         ORDER BY n DESC
         LIMIT 1;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access",
            "outer",
            "inner",
            "build",
            "predicate",
            "residual",
            "filter",
            "groups",
            "aggregates",
            "having",
            "projection",
            "order_by",
            "limit",
        ]
    );
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_eq!(explained.rows[0][1], text("u (users)"));
    assert_eq!(explained.rows[0][3], text("o (orders)"));
    assert_eq!(explained.rows[0][4], text("u.id = o.user_id"));
    assert_eq!(explained.rows[0][5], Value::Null);
    assert_eq!(explained.rows[0][6], text("o.amount > 15"));
    assert_eq!(explained.rows[0][9], text("n > 0"));
    assert_eq!(explained.rows[0][11], text("n DESC"));
    assert_eq!(explained.rows[0][12], integer(1));
    let nested = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
    );
    assert_eq!(nested.rows[0][0], text("LeftNestedLoopJoin"));
    assert_eq!(nested.rows[0][3], Value::Null);
    assert_eq!(nested.rows[0][4], Value::Null);
    let inner = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(inner.columns[0], "access");
    assert_eq!(inner.columns[4], "join");
    assert_eq!(inner.rows[0][0], text("Aggregate"));
    assert_eq!(inner.rows[0][4], text("HashJoin"));
    let grouped = query(
        &mut database,
        "EXPLAIN SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
    );
    assert_eq!(grouped.rows[0][0], text("HashAggregate"));
    assert_eq!(grouped.rows[0][4], text("HashJoin"));
}

#[test]
fn overflow_is_an_error_after_limit_and_the_next_query_still_runs() {
    let (dir, path) = temporary("m20-overflow");
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
        .execute(&format!(
            "INSERT INTO orders VALUES (1, 1), (2, {}), (2, 1);",
            i64::MAX
        ))
        .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let wal_before = std::fs::metadata(&wal).unwrap().len();
    let error = failure(
        &mut database,
        "SELECT u.name, SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name LIMIT 1;",
    );
    assert!(error.contains("SUM overflows INTEGER"), "{error}");
    let error = failure(
        &mut database,
        "SELECT SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id LIMIT 0;",
    );
    assert!(error.contains("SUM overflows INTEGER"), "{error}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_before);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 1;",
        ),
        vec![vec![integer(1), integer(1)]]
    );
    database
        .execute("DELETE FROM orders WHERE user_id = 2;")
        .unwrap();
    database
        .execute(&format!("INSERT INTO orders VALUES (1, {});", i64::MAX - 1))
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 1;",
        ),
        vec![vec![integer(i64::MAX)]]
    );
    drop(dir);
}

#[test]
fn explain_does_not_write_and_groups_match_the_recorded_seeds() {
    let (dir, path) = temporary("m20-explain");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let wal_before = std::fs::metadata(&wal).unwrap().len();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(explained.rows[0][0], text("HashLeftJoin"));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_before);
    let counted = rows(
        &mut database,
        "SELECT COUNT(*), COUNT(o.user_id), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(counted, vec![vec![integer(1), integer(0), Value::Null]]);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), wal_before);
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(o.user_id), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        ),
        counted
    );
    let inner = rows(
        &mut database,
        "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(inner, vec![vec![integer(0)]]);
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT name FROM users;"),
        vec![vec![text("Ada")]]
    );
    drop(database);
    drop(dir);

    for seed in [
        0x2020, 0x20a1, 0x20b2, 0x20c3, 0x20d4, 0x20e5, 0x20f6, 0x2007, 0x2011, 0x20bb, 0x20cc,
    ] {
        for (left_n, right_n) in [(0, 0), (0, 6), (6, 0), (8, 3), (3, 14), (24, 24), (40, 12)] {
            check_seed(seed, left_n, right_n);
        }
    }
}

fn check_seed(seed: u64, left_n: i64, right_n: i64) {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        state
    };
    let mut left = Vec::new();
    let mut left_sql = String::from("INSERT INTO lefts VALUES ");
    for id in 0..left_n {
        let name = format!("n{}", next() % 5);
        let flag = next() % 3 != 0;
        let key = if next() % 11 == 0 {
            Value::Null
        } else {
            integer(id)
        };
        if id > 0 {
            left_sql.push(',');
        }
        let key_sql = match &key {
            Value::Null => "NULL".to_string(),
            Value::Integer(value) => value.to_string(),
            _ => unreachable!(),
        };
        left_sql.push_str(&format!(
            "({key_sql}, '{name}', {})",
            if flag { "TRUE" } else { "FALSE" }
        ));
        left.push(vec![key, text(&name), Value::Boolean(flag)]);
    }
    let mut right = Vec::new();
    let mut right_sql = String::from("INSERT INTO rights VALUES ");
    for id in 0..right_n {
        let user_id = match next() % 4 {
            0 => Value::Null,
            1 => integer(-1),
            2 => integer(id % 5),
            _ => integer(id % left_n.max(1)),
        };
        let amount = if next() % 4 == 0 {
            Value::Null
        } else {
            integer((next() % 30) as i64 - 5)
        };
        if id > 0 {
            right_sql.push(',');
        }
        let user_sql = match &user_id {
            Value::Null => "NULL".to_string(),
            Value::Integer(value) => value.to_string(),
            _ => unreachable!(),
        };
        let amount_sql = match &amount {
            Value::Null => "NULL".to_string(),
            Value::Integer(value) => value.to_string(),
            _ => unreachable!(),
        };
        right_sql.push_str(&format!("({user_sql}, {amount_sql})"));
        right.push(vec![user_id, amount]);
    }
    let mut database = Database::new();
    database
        .execute("CREATE TABLE lefts (id INT, name TEXT, flag BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE rights (user_id INT, amount INT);")
        .unwrap();
    if left_n > 0 {
        database.execute(&format!("{left_sql};")).unwrap();
    }
    if right_n > 0 {
        database.execute(&format!("{right_sql};")).unwrap();
    }
    let threshold = (next() % 12) as i64;
    let on = |outer: &[Value], inner: &[Value]| {
        is_true_eq(&outer[0], &inner[0]) && is_true_gt(&inner[1], threshold)
    };
    let keep = |outer: &[Value], inner: &[Value]| {
        matches!(outer[2], Value::Boolean(true)) && !matches!(inner[1], Value::Null)
    };
    let pairs = logical_pairs(&left, &right, 2, on, keep);
    let groups = grouped(
        &pairs,
        |outer, inner| vec![outer[1].clone(), inner[0].clone()],
        |_, inner| inner[1].clone(),
    );
    let expect: Vec<_> = groups
        .iter()
        .filter(|(_, acc)| acc.counted > 0)
        .map(|(key, acc)| {
            let cells = acc.cells();
            vec![
                key[0].clone(),
                key[1].clone(),
                cells[0].clone(),
                cells[1].clone(),
                cells[2].clone(),
                cells[3].clone(),
                cells[4].clone(),
                cells[5].clone(),
            ]
        })
        .collect();
    let sql = format!(
        "SELECT l.name, r.user_id, COUNT(*) AS n, COUNT(r.amount) AS c,
                SUM(r.amount) AS s, AVG(r.amount) AS a, MIN(r.amount) AS lo, MAX(r.amount) AS hi
         FROM lefts l LEFT JOIN rights r ON l.id = r.user_id AND r.amount > {threshold}
         WHERE l.flag = TRUE AND r.amount IS NOT NULL
         GROUP BY l.name, r.user_id
         HAVING c > 0"
    );
    assert_eq!(
        rows(&mut database, &format!("{sql};")),
        expect,
        "seed {seed:#x} {left_n}x{right_n}"
    );
    let mut ordered = expect.clone();
    order_rows(&mut ordered);
    ordered.truncate(2);
    assert_eq!(
        rows(
            &mut database,
            &format!("{sql} ORDER BY n DESC, l.name ASC LIMIT 2;"),
        ),
        ordered,
        "order seed {seed:#x} {left_n}x{right_n}"
    );
    let pairs_all = logical_pairs(&left, &right, 2, eq_id, |_, _| true);
    let by_name = grouped(
        &pairs_all,
        |outer, _| vec![outer[1].clone()],
        |_, inner| inner[1].clone(),
    );
    let name_rows: Vec<_> = by_name
        .iter()
        .map(|(key, acc)| {
            let cells = acc.cells();
            vec![
                key[0].clone(),
                cells[0].clone(),
                cells[1].clone(),
                cells[2].clone(),
            ]
        })
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT l.name, COUNT(*), COUNT(r.amount), SUM(r.amount)
             FROM lefts l LEFT JOIN rights r ON l.id = r.user_id
             GROUP BY l.name;",
        ),
        name_rows,
        "name groups seed {seed:#x} {left_n}x{right_n}"
    );
    let by_right = grouped(
        &pairs_all,
        |_, inner| vec![inner[0].clone()],
        |_, inner| inner[1].clone(),
    );
    let right_rows: Vec<_> = by_right
        .iter()
        .map(|(key, acc)| vec![key[0].clone(), integer(acc.star), integer(acc.counted)])
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT r.user_id, COUNT(*), COUNT(r.amount)
             FROM lefts l LEFT JOIN rights r ON l.id = r.user_id
             GROUP BY r.user_id;",
        ),
        right_rows,
        "right groups seed {seed:#x} {left_n}x{right_n}"
    );
    let marker = threshold;
    let or_pairs = logical_pairs(
        &left,
        &right,
        2,
        |outer, inner| eq_id(outer, inner) || is_true_eq(&inner[1], &integer(marker)),
        |_, _| true,
    );
    let mut or_total = Acc::new();
    for (_, inner) in &or_pairs {
        or_total.push_amount(&inner[1]);
    }
    let or_cells = or_total.cells();
    assert_eq!(
        rows(
            &mut database,
            &format!(
                "SELECT COUNT(*), COUNT(r.amount), SUM(r.amount), AVG(r.amount)
                 FROM lefts l LEFT JOIN rights r ON l.id = r.user_id OR r.amount = {marker};"
            ),
        ),
        vec![vec![
            or_cells[0].clone(),
            or_cells[1].clone(),
            or_cells[2].clone(),
            or_cells[3].clone(),
        ]],
        "or seed {seed:#x} {left_n}x{right_n}"
    );
    let mut scalar = Acc::new();
    for (_, inner) in &pairs_all {
        scalar.push_amount(&inner[1]);
    }
    let cells = scalar.cells();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(r.amount), SUM(r.amount), AVG(r.amount), MIN(r.amount), MAX(r.amount)
             FROM lefts l LEFT JOIN rights r ON l.id = r.user_id;",
        ),
        vec![vec![
            cells[0].clone(),
            cells[1].clone(),
            cells[2].clone(),
            cells[3].clone(),
            cells[4].clone(),
            cells[5].clone(),
        ]],
        "scalar seed {seed:#x} {left_n}x{right_n}"
    );
}

fn eq_id(outer: &[Value], inner: &[Value]) -> bool {
    is_true_eq(&outer[0], &inner[0])
}

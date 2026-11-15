//! `AVG`, `MIN`, `MAX`, and select-list aliases.

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

fn integer(value: i64) -> Value {
    Value::Integer(value)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

/// Independent reduction. The denominator is positive.
fn ratio(mut numerator: i128, mut denominator: i64) -> Value {
    assert!(denominator > 0);
    if numerator == 0 {
        return Value::Ratio {
            numerator: 0,
            denominator: 1,
        };
    }
    let mut divisor = gcd(numerator.unsigned_abs(), u128::from(denominator as u64));
    while divisor > 1 {
        numerator /= i128::try_from(divisor).unwrap();
        denominator /= i64::try_from(divisor).unwrap();
        divisor = gcd(numerator.unsigned_abs(), u128::from(denominator as u64));
        if divisor == 1 {
            break;
        }
    }
    Value::Ratio {
        numerator,
        denominator,
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

fn staff() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE employees (
                department TEXT,
                name TEXT,
                salary INT,
                active BOOLEAN
            );",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO employees VALUES
                ('eng', 'Zoe', 10, TRUE),
                ('sales', '', 30, FALSE),
                ('eng', 'Bea', NULL, TRUE),
                ('eng', 'Cy', 20, FALSE),
                (NULL, 'Dee', 0, FALSE),
                ('sales', 'Amy', NULL, NULL);",
        )
        .unwrap();
    database
}

fn numbers(values: &[&str]) -> Database {
    let mut database = Database::new();
    database.execute("CREATE TABLE samples (n INT);").unwrap();
    if !values.is_empty() {
        database
            .execute(&format!(
                "INSERT INTO samples (n) VALUES {};",
                values
                    .iter()
                    .map(|value| format!("({value})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .unwrap();
    }
    database
}

#[test]
fn avg_of_one_integer_keeps_a_fractional_shape() {
    let mut database = numbers(&["7"]);
    let result = query(&mut database, "SELECT AVG(n) FROM samples;");
    assert_eq!(result.columns, vec!["AVG(n)".to_string()]);
    assert_eq!(result.rows, vec![vec![ratio(7, 1)]]);
    assert_eq!(result.rows[0][0].to_string(), "7.0");
}

#[test]
fn avg_keeps_fractions_repeating_values_and_signs() {
    let mut database = numbers(&["1", "0"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(1, 2)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "0.5"
    );
    let mut database = numbers(&["1", "2"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(3, 2)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "1.5"
    );

    let mut database = numbers(&["1", "0", "0"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "1/3"
    );
    let mut database = numbers(&["-1", "0", "0"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "-1/3"
    );
    let mut database = numbers(&["-3", "-2"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(-5, 2)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "-2.5"
    );
    let mut database = numbers(&["2", "2", "2", "4"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(5, 2)]]
    );
}

#[test]
fn avg_ignores_null_and_is_null_when_nothing_remains() {
    let mut database = numbers(&[]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n), MIN(n), MAX(n) FROM samples;"),
        vec![vec![Value::Null, Value::Null, Value::Null]]
    );
    let mut database = numbers(&["NULL", "NULL"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![Value::Null]]
    );
    let mut database = numbers(&["NULL", "2", "4"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(3, 1)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "3.0"
    );
}

#[test]
fn avg_handles_extreme_integers_and_a_wide_sum() {
    let mut database = numbers(&["-9223372036854775808"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "-9223372036854775808.0"
    );
    let mut database = numbers(&["9223372036854775807"]);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0].to_string(),
        "9223372036854775807.0"
    );
    let mut database = numbers(&["9223372036854775807", "9223372036854775806"]);
    let cell = &rows(&mut database, "SELECT AVG(n) FROM samples;")[0][0];
    assert_eq!(
        cell,
        &ratio(i128::from(i64::MAX) + i128::from(i64::MAX - 1), 2)
    );
    assert_eq!(cell.to_string(), "9223372036854775806.5");

    let values = ["9223372036854775807"; 64];
    let mut database = numbers(&values);
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(i128::from(i64::MAX), 1)]]
    );
}

#[test]
fn avg_rejects_text_and_boolean_before_reading_rows() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (name TEXT, flag BOOLEAN);")
        .unwrap();
    for sql in [
        "SELECT AVG(name) FROM samples;",
        "SELECT AVG(name) FROM samples LIMIT 0;",
        "SELECT AVG(flag) FROM samples;",
    ] {
        let error = failure(&mut database, sql);
        assert!(
            error == "AVG requires INTEGER, not TEXT"
                || error == "AVG requires INTEGER, not BOOLEAN",
            "{sql} -> {error}"
        );
    }
    assert_eq!(
        failure(&mut database, "SELECT AVG(*) FROM samples;"),
        "AVG requires an INTEGER column"
    );
}

#[test]
fn having_and_order_by_compare_averages_exactly() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (bucket INT, n INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO samples VALUES
                (1, 1),
                (1, 2),
                (2, 10),
                (2, 10),
                (3, NULL),
                (4, 0),
                (4, 0);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT bucket, AVG(n) FROM samples GROUP BY bucket HAVING AVG(n) > 1;"
        ),
        vec![
            vec![integer(1), ratio(3, 2)],
            vec![integer(2), ratio(10, 1)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT bucket FROM samples GROUP BY bucket HAVING AVG(n) = 0;"
        ),
        vec![vec![integer(4)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT bucket FROM samples GROUP BY bucket HAVING AVG(n) IS NULL;"
        ),
        vec![vec![integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT bucket, AVG(n) FROM samples GROUP BY bucket ORDER BY AVG(n) ASC;"
        ),
        vec![
            vec![integer(4), ratio(0, 1)],
            vec![integer(1), ratio(3, 2)],
            vec![integer(2), ratio(10, 1)],
            vec![integer(3), Value::Null],
        ]
    );
}

#[test]
fn min_and_max_cover_integer_text_and_boolean() {
    let mut database = staff();
    assert_eq!(
        rows(
            &mut database,
            "SELECT MIN(salary), MAX(salary), MIN(name), MAX(name), MIN(active), MAX(active)
             FROM employees;"
        ),
        vec![vec![
            integer(0),
            integer(30),
            text(""),
            text("Zoe"),
            boolean(false),
            boolean(true),
        ]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, MIN(salary), MAX(salary), MIN(name), MAX(active)
             FROM employees GROUP BY department;"
        ),
        vec![
            vec![
                text("eng"),
                integer(10),
                integer(20),
                text("Bea"),
                boolean(true)
            ],
            vec![
                text("sales"),
                integer(30),
                integer(30),
                text(""),
                boolean(false)
            ],
            vec![
                Value::Null,
                integer(0),
                integer(0),
                text("Dee"),
                boolean(false)
            ],
        ]
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE flags (flag BOOLEAN);")
        .unwrap();
    database
        .execute("INSERT INTO flags VALUES (NULL), (NULL);")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT MIN(flag), MAX(flag) FROM flags;"),
        vec![vec![Value::Null, Value::Null]]
    );
    database
        .execute("INSERT INTO flags VALUES (TRUE);")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT MIN(flag), MAX(flag) FROM flags;"),
        vec![vec![boolean(true), boolean(true)]]
    );
    assert_eq!(
        failure(&mut database, "SELECT * FROM flags WHERE flag > FALSE;"),
        "operator '>' is not supported for BOOLEAN"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT MIN(flag) FROM flags HAVING MIN(flag) > FALSE;"
        ),
        "operator '>' is not supported for BOOLEAN"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT MIN(flag) FROM flags HAVING MIN(flag) = TRUE;"
        ),
        vec![vec![boolean(true)]]
    );
}

#[test]
fn repeated_aggregates_share_slots_and_a_late_overflow_hides_nothing() {
    let mut database = staff();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT AVG(salary), AVG(employees.salary), MIN(name), MIN(employees.name)
         FROM employees;",
    );
    assert_eq!(
        explained.rows[0][2],
        text("AVG(salary), AVG(employees.salary), MIN(name), MIN(employees.name)")
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT AVG(salary), AVG(employees.salary), MIN(name), MIN(employees.name) FROM employees;"
        ),
        vec![vec![ratio(15, 1), ratio(15, 1), text(""), text("")]]
    );

    let mut database = Database::new();
    database.execute("CREATE TABLE samples (n INT);").unwrap();
    database
        .execute("INSERT INTO samples VALUES (9223372036854775807), (1);")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT SUM(n), AVG(n) FROM samples HAVING SUM(n) > 0 LIMIT 0;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(&mut database, "SELECT AVG(n) FROM samples;"),
        vec![vec![ratio(i128::from(i64::MAX) + 1, 2)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM samples;"),
        vec![vec![integer(2)]]
    );
}

#[test]
fn select_aliases_bind_in_order_by_and_nowhere_else() {
    let mut database = staff();
    let result = query(
        &mut database,
        "SELECT department AS dept, COUNT(*) AS total
         FROM employees
         GROUP BY department
         ORDER BY total DESC, dept ASC;",
    );
    assert_eq!(
        result.columns,
        vec!["dept".to_string(), "total".to_string()]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![text("eng"), integer(3)],
            vec![text("sales"), integer(2)],
            vec![Value::Null, integer(1)],
        ]
    );
    let implicit = query(
        &mut database,
        "SELECT department dept, COUNT(*) total
         FROM employees
         GROUP BY department
         ORDER BY total DESC;",
    );
    assert_eq!(implicit.columns, result.columns);
    assert_eq!(implicit.rows, result.rows);

    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS name, name FROM employees ORDER BY name, employees.name;"
        ),
        vec![
            vec![text("eng"), text("Bea")],
            vec![text("eng"), text("Cy")],
            vec![text("eng"), text("Zoe")],
            vec![text("sales"), text("")],
            vec![text("sales"), text("Amy")],
            vec![Value::Null, text("Dee")],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS name, name FROM employees e ORDER BY e.name;"
        ),
        vec![
            vec![text("sales"), text("")],
            vec![text("sales"), text("Amy")],
            vec![text("eng"), text("Bea")],
            vec![text("eng"), text("Cy")],
            vec![Value::Null, text("Dee")],
            vec![text("eng"), text("Zoe")],
        ]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department AS dept, name AS dept FROM employees;"
        ),
        "duplicate column alias 'dept'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT name, name FROM employees ORDER BY name;"
        ),
        "column 'name' is ambiguous"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT name, name FROM employees ORDER BY employees.name;"
        )
        .len(),
        6
    );
    for sql in [
        "SELECT department AS dept FROM employees WHERE dept = 'eng';",
        "SELECT department AS dept FROM employees GROUP BY dept;",
    ] {
        let error = failure(&mut database, sql);
        assert!(
            error.contains("does not exist") || error.contains("must appear in GROUP BY"),
            "{sql} -> {error}"
        );
    }
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS dept, COUNT(*) AS total
             FROM employees GROUP BY department HAVING total > 1;"
        ),
        vec![
            vec![text("eng"), integer(3)],
            vec![text("sales"), integer(2)]
        ]
    );
    let mut database = staff();
    database
        .execute("CREATE TABLE rooms (department TEXT, name TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO rooms VALUES ('eng', 'lab');")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT employees.department AS total
             FROM employees JOIN rooms ON total = rooms.department;"
        ),
        "column 'total' does not exist"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT e.department AS dept
             FROM employees e JOIN rooms r ON e.department = r.department
             WHERE e.name = 'Zoe';"
        ),
        vec![vec![text("eng")]]
    );
}

#[test]
fn aggregates_over_an_inner_join_match_a_hand_check() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy');")
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (1, 10, TRUE),
                (1, NULL, FALSE),
                (1, 30, TRUE),
                (2, 5, FALSE),
                (NULL, 7, TRUE);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name AS who, AVG(o.amount) AS avg_amount, MIN(o.amount), MAX(o.paid)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING AVG(o.amount) > 6
             ORDER BY avg_amount DESC;"
        ),
        vec![vec![text("Ada"), ratio(20, 1), integer(10), boolean(true)]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, AVG(o.amount)
         FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name
         HAVING AVG(o.amount) IS NOT NULL;",
    );
    assert_eq!(explained.rows[0][2], text("AVG(o.amount)"));
    assert_eq!(explained.rows[0][3], text("AVG(o.amount) IS NOT NULL"));
    assert!(
        matches!(&explained.rows[0][4], Value::Text(join) if join == "HashJoin" || join == "NestedLoopJoin" || join == "IndexNestedLoopJoin")
    );
}

#[test]
fn explain_does_not_execute_and_a_restart_keeps_the_file_format() {
    let (_dir, path) = temporary("avg-restart");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES ('eng', 1), ('eng', 2), ('sales', NULL);")
        .unwrap();
    drop(database);
    let before = std::fs::read(&path).unwrap();
    let version = u32::from_le_bytes(before[32..36].try_into().unwrap());
    assert_eq!(version, 2);
    let mut database = Database::open(&path).unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT department, AVG(salary), MIN(salary), MAX(salary)
         FROM employees GROUP BY department HAVING AVG(salary) > 0
         ORDER BY AVG(salary) DESC;",
    );
    assert_eq!(
        explained.rows[0][2],
        text("AVG(salary), MIN(salary), MAX(salary)")
    );
    assert_eq!(explained.rows[0][7], text("AVG(salary) > 0"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) FROM employees GROUP BY department;"
        ),
        vec![
            vec![text("eng"), ratio(3, 2)],
            vec![text("sales"), Value::Null],
        ]
    );
    drop(database);
    let after = std::fs::read(&path).unwrap();
    assert_eq!(after, before);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(&mut database, "SELECT AVG(salary) FROM employees;"),
        vec![vec![ratio(3, 2)]]
    );
}

#[test]
fn aliases_types_and_overflow_stay_exact_on_the_edges() {
    let mut database = staff();
    let empty = query(
        &mut database,
        "SELECT department AS dept FROM employees WHERE name = 'nobody' ORDER BY dept;",
    );
    assert_eq!(empty.columns, vec!["dept".to_string()]);
    assert!(empty.rows.is_empty());
    let limited = query(
        &mut database,
        "SELECT name AS who, salary FROM employees ORDER BY who LIMIT 0;",
    );
    assert_eq!(
        limited.columns,
        vec!["who".to_string(), "salary".to_string()]
    );
    assert!(limited.rows.is_empty());
    assert_eq!(
        rows(
            &mut database,
            "SELECT name AS who FROM employees ORDER BY who;"
        ),
        ["", "Amy", "Bea", "Cy", "Dee", "Zoe"]
            .into_iter()
            .map(|name| vec![text(name)])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT AVG(salary) FROM employees HAVING AVG(salary) > '1';"
        ),
        "cannot apply operator '>' to RATIO and TEXT"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT AVG(salary) FROM employees HAVING AVG(salary) > TRUE;"
        ),
        "cannot apply operator '>' to RATIO and BOOLEAN"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING MIN(salary) = 0 ORDER BY MAX(name);"
        ),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING MAX(name) = 'Zoe' ORDER BY MIN(salary) DESC;"
        ),
        vec![vec![text("eng")]]
    );

    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (bucket INT, name TEXT, n INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO samples VALUES
                (1, 'early', 1),
                (2, 'late', 9223372036854775807),
                (2, 'overflow', 1);",
        )
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT bucket, MIN(name), MAX(name), COUNT(*), SUM(n), AVG(n)
             FROM samples GROUP BY bucket
             HAVING SUM(n) < 0
             LIMIT 0;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM samples;"),
        vec![vec![integer(3)]]
    );
    database
        .execute("UPDATE samples SET name = 'kept' WHERE bucket = 1;")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT name, n FROM samples WHERE bucket = 1;"
        ),
        vec![vec![text("kept"), integer(1)]]
    );

    let long = "a".repeat(4_000);
    database
        .execute(&format!(
            "INSERT INTO samples VALUES (3, '{long}', NULL), (3, 'b', NULL);"
        ))
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT MIN(name), MAX(name) FROM samples WHERE bucket = 3;"
        ),
        vec![vec![text(&long), text("b")]]
    );

    let mut database = numbers(&["-3", "-2", "0", "NULL"]);
    assert_eq!(
        rows(
            &mut database,
            "SELECT AVG(n) FROM samples HAVING AVG(n) > -2;"
        ),
        vec![vec![ratio(-5, 3)]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT AVG(n) FROM samples HAVING AVG(n) > NULL OR AVG(n) IS NULL;"
        )
        .is_empty()
    );
}

#[test]
fn joined_groups_match_an_independent_reference() {
    let seed = 0x0b12_u64;
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    };
    let mut users = Vec::new();
    for id in 0..6 {
        let name = if next() % 5 == 0 {
            None
        } else {
            Some(format!("u{}", next() % 3))
        };
        users.push((id, name));
    }
    let mut orders = Vec::new();
    for id in 0..14 {
        let user = if next() % 6 == 0 {
            None
        } else {
            Some((next() % 6) as i64)
        };
        let amount = if next() % 4 == 0 {
            None
        } else {
            Some(match next() % 4 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => -2,
                _ => (next() % 9) as i64,
            })
        };
        let _ = id;
        orders.push((user, amount));
    }

    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    let mut sql = String::from("INSERT INTO users VALUES ");
    for (index, (id, name)) in users.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({id}, {})", sql_opt_text(name)));
    }
    database.execute(&sql).unwrap();
    let mut sql = String::from("INSERT INTO orders VALUES ");
    for (index, (user, amount)) in orders.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str(&format!(
            "({}, {})",
            sql_opt_int(*user),
            sql_opt_int(*amount)
        ));
    }
    database.execute(&sql).unwrap();
    let actual = rows(
        &mut database,
        "SELECT u.name AS who, AVG(o.amount) AS average, MIN(o.amount), MAX(o.amount), COUNT(*)
         FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name
         HAVING AVG(o.amount) > 0 OR AVG(o.amount) IS NULL
         ORDER BY average ASC, who ASC;",
    );
    assert_eq!(actual, join_reference(&users, &orders), "seed {seed:#x}");
}

fn join_reference(
    users: &[(i64, Option<String>)],
    orders: &[(Option<i64>, Option<i64>)],
) -> Vec<Vec<Value>> {
    struct Acc {
        name: Option<String>,
        sum: i128,
        count: i64,
        min: Option<i64>,
        max: Option<i64>,
        rows: i64,
    }
    let mut groups: Vec<Acc> = Vec::new();
    for (user_id, name) in users {
        for (order_user, amount) in orders {
            if *order_user != Some(*user_id) {
                continue;
            }
            let slot = groups.iter().position(|group| group.name == *name);
            let group = if let Some(slot) = slot {
                &mut groups[slot]
            } else {
                groups.push(Acc {
                    name: name.clone(),
                    sum: 0,
                    count: 0,
                    min: None,
                    max: None,
                    rows: 0,
                });
                groups.last_mut().unwrap()
            };
            group.rows += 1;
            if let Some(amount) = amount {
                group.sum += i128::from(*amount);
                group.count += 1;
                group.min = Some(group.min.map_or(*amount, |min| min.min(*amount)));
                group.max = Some(group.max.map_or(*amount, |max| max.max(*amount)));
            }
        }
    }
    let mut kept = Vec::new();
    for group in groups {
        let average = if group.count == 0 {
            None
        } else {
            Some(ratio(group.sum, group.count))
        };
        let keep = match &average {
            None => true,
            Some(Value::Ratio { numerator, .. }) => *numerator > 0,
            Some(_) => false,
        };
        if !keep {
            continue;
        }
        kept.push((
            average.clone(),
            group.name.clone(),
            vec![
                group.name.map(|name| text(&name)).unwrap_or(Value::Null),
                average.unwrap_or(Value::Null),
                group.min.map(integer).unwrap_or(Value::Null),
                group.max.map(integer).unwrap_or(Value::Null),
                integer(group.rows),
            ],
        ));
    }
    kept.sort_by(|left, right| {
        let averages = match (&left.0, &right.0) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(left), Some(right)) => ratio_cmp(left, right),
        };
        averages.then_with(|| match (&left.1, &right.1) {
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(left), Some(right)) => left.cmp(right),
        })
    });
    kept.into_iter().map(|(_, _, row)| row).collect()
}

#[test]
fn grouped_queries_match_an_independent_reference() {
    let seeds = [0x0a99_u64, 0x51c0, 0xc0ff, 0x5eed, 0xa11c, 0x11ab];
    for seed in seeds {
        let generated = generate(seed);
        let mut database = Database::new();
        database
            .execute(
                "CREATE TABLE employees (department TEXT, name TEXT, salary INT, active BOOLEAN);",
            )
            .unwrap();
        if !generated.is_empty() {
            let mut sql = String::from("INSERT INTO employees VALUES ");
            for (index, row) in generated.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('(');
                sql.push_str(&sql_opt_text(&row.department));
                sql.push(',');
                sql.push_str(&sql_text(&row.name));
                sql.push(',');
                sql.push_str(&sql_opt_int(row.salary));
                sql.push(',');
                sql.push_str(&sql_opt_bool(row.active));
                sql.push(')');
            }
            database.execute(&sql).unwrap();
        }
        let actual = rows(
            &mut database,
            "SELECT department AS dept, AVG(salary) AS average, MIN(salary), MAX(name), MIN(active)
             FROM employees
             GROUP BY department
             HAVING AVG(salary) > 0 OR AVG(salary) IS NULL
             ORDER BY average ASC;",
        );
        assert_eq!(actual, reference(&generated), "seed {seed:#x}");
    }
}

struct Generated {
    department: Option<String>,
    name: String,
    salary: Option<i64>,
    active: Option<bool>,
}

fn generate(mut seed: u64) -> Vec<Generated> {
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed
    };
    let count = 12 + (next() % 20) as usize;
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let department = match next() % 4 {
            0 => None,
            1 => Some("eng".to_string()),
            2 => Some("sales".to_string()),
            _ => Some(String::new()),
        };
        let name = if next() % 7 == 0 {
            String::new()
        } else {
            format!("n{index}-{}", next() % 5)
        };
        let salary = if next() % 4 == 0 {
            None
        } else {
            let value = match next() % 5 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => -3,
                3 => 0,
                _ => i64::try_from(next() % 17).unwrap(),
            };
            Some(value)
        };
        let active = match next() % 3 {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        };
        rows.push(Generated {
            department,
            name,
            salary,
            active,
        });
    }
    rows
}

fn reference(rows: &[Generated]) -> Vec<Vec<Value>> {
    let mut groups: Vec<Group> = Vec::new();
    for row in rows {
        let slot = groups
            .iter()
            .position(|group| group.department == row.department);
        let group = if let Some(slot) = slot {
            &mut groups[slot]
        } else {
            groups.push(Group {
                department: row.department.clone(),
                sum: 0,
                count: 0,
                min_salary: None,
                max_name: None,
                min_active: None,
            });
            groups.last_mut().unwrap()
        };
        if let Some(salary) = row.salary {
            group.sum += i128::from(salary);
            group.count += 1;
            group.min_salary = Some(group.min_salary.map_or(salary, |min| min.min(salary)));
        }
        group.max_name = Some(match &group.max_name {
            Some(current) if current >= &row.name => current.clone(),
            _ => row.name.clone(),
        });
        if let Some(active) = row.active {
            group.min_active = Some(group.min_active.map_or(active, |min| min && active));
        }
    }
    let mut kept = Vec::new();
    for group in groups {
        let average = if group.count == 0 {
            None
        } else {
            Some(ratio(group.sum, group.count))
        };
        let keep = match &average {
            None => true,
            Some(Value::Ratio {
                numerator,
                denominator,
            }) => *numerator > 0 && *denominator > 0,
            Some(_) => false,
        };
        if !keep {
            continue;
        }
        kept.push((
            average.clone(),
            vec![
                group
                    .department
                    .map(|department| text(&department))
                    .unwrap_or(Value::Null),
                average.unwrap_or(Value::Null),
                group.min_salary.map(integer).unwrap_or(Value::Null),
                group
                    .max_name
                    .map(|name| text(&name))
                    .unwrap_or(Value::Null),
                group.min_active.map(boolean).unwrap_or(Value::Null),
            ],
        ));
    }
    kept.sort_by(|left, right| match (&left.0, &right.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => ratio_cmp(left, right),
    });
    kept.into_iter().map(|(_, row)| row).collect()
}

struct Group {
    department: Option<String>,
    sum: i128,
    count: i64,
    min_salary: Option<i64>,
    max_name: Option<String>,
    min_active: Option<bool>,
}

fn ratio_cmp(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (
            Value::Ratio {
                numerator: left_n,
                denominator: left_d,
            },
            Value::Ratio {
                numerator: right_n,
                denominator: right_d,
            },
        ) => {
            let left = *left_n * i128::from(*right_d);
            let right = *right_n * i128::from(*left_d);
            left.cmp(&right)
        }
        _ => std::cmp::Ordering::Equal,
    }
}

fn sql_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn sql_opt_text(value: &Option<String>) -> String {
    value
        .as_deref()
        .map(sql_text)
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_opt_int(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_opt_bool(value: Option<bool>) -> String {
    match value {
        Some(true) => "TRUE".to_string(),
        Some(false) => "FALSE".to_string(),
        None => "NULL".to_string(),
    }
}

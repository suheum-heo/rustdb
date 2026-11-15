//! `HAVING` aliases and `DISTINCT` aggregates.
//!
//! Randomized comparisons use the documented seeds `0x0d13`, `0x51c0`,
//! `0xc0ff`, `0x5eed`, `0xa11c`, and `0x11ab` for grouped rows, and `0xb017`,
//! `0x5e1f`, and `0xa91e` for joins. The reference model is a `BTreeSet` per
//! group, filled by a nested loop. It does not use the engine's hash table.

use std::cmp::Ordering;
use std::collections::BTreeSet;

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

fn employees() -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE employees (
                department TEXT,
                name TEXT,
                salary INT,
                active BOOLEAN,
                employee_id INT
            );",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO employees VALUES
                ('eng', 'Zoe', 60000, TRUE, 1),
                ('eng', 'Bea', 40000, TRUE, 2),
                ('eng', 'Cy', 60000, FALSE, 3),
                ('sales', 'Amy', 30000, FALSE, 4),
                ('sales', 'Dee', NULL, TRUE, 5),
                (NULL, 'No', 10000, FALSE, 6);",
        )
        .unwrap();
    database
}

#[test]
fn having_can_name_an_aggregate_alias() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(*) AS total
             FROM employees GROUP BY department HAVING total >= 3;"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department HAVING average_salary > 50000;"
        ),
        vec![vec![text("eng"), ratio(160_000, 3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department
             HAVING average_salary IS NULL OR NOT (average_salary > 0);"
        ),
        Vec::<Vec<Value>>::new()
    );
    database
        .execute("INSERT INTO employees VALUES ('ops', 'X', NULL, TRUE, 7);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department
             HAVING AVG(salary) IS NULL;"
        ),
        vec![vec![text("ops")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department
             HAVING NOT (average_salary > 0);"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary, COUNT(*) AS total
             FROM employees GROUP BY department
             HAVING average_salary IS NULL AND department = 'ops';"
        ),
        vec![vec![text("ops"), Value::Null, integer(1)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department
             HAVING average_salary IS NULL OR department = 'eng';"
        ),
        vec![
            vec![text("eng"), ratio(160_000, 3)],
            vec![text("ops"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department
             HAVING NOT (average_salary IS NULL) AND department = 'sales';"
        ),
        vec![vec![text("sales"), ratio(30_000, 1)]]
    );
}

#[test]
fn having_can_name_a_grouped_column_alias() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS dept, COUNT(*) AS total
             FROM employees GROUP BY department
             HAVING dept = 'eng' AND employees.department = 'eng';"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS dept, COUNT(*) AS total
             FROM employees e GROUP BY department HAVING e.department = 'sales';"
        ),
        vec![vec![text("sales"), integer(2)]]
    );
}

#[test]
fn an_explicit_alias_wins_over_the_source_column() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS salary, COUNT(*) AS total
             FROM employees GROUP BY department HAVING salary = 'eng';"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department, COUNT(*) AS total
             FROM employees GROUP BY department HAVING salary > 1;"
        ),
        "column 'salary' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department AS dept, COUNT(*) AS total
             FROM employees WHERE total > 1 GROUP BY department;"
        ),
        "column 'total' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department AS dept, COUNT(*) FROM employees GROUP BY total;"
        ),
        "column 'total' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT e.department, COUNT(*) FROM employees e
             JOIN employees o ON e.total = o.employee_id GROUP BY e.department;"
        ),
        "column 'total' does not exist in table 'e'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department AS dept, COUNT(*) AS total
             FROM employees e GROUP BY department HAVING employees.department = 'eng';"
        ),
        "table qualifier 'employees' does not match 'e'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT salary AS pay, COUNT(*) FROM employees GROUP BY department;"
        ),
        "column 'salary' must appear in GROUP BY"
    );
}

#[test]
fn duplicate_and_ambiguous_aliases_are_rejected() {
    let mut database = employees();
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) AS total, SUM(salary) AS total FROM employees;"
        ),
        "duplicate column alias 'total'"
    );
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (10, 1);")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT u.id, o.id, COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.id, o.id HAVING id > 0;"
        ),
        "column 'id' is ambiguous"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department AS total, total FROM employees GROUP BY department;"
        ),
        "column 'total' does not exist in table 'employees'"
    );
}

#[test]
fn invalid_having_is_rejected_on_empty_input_and_limit_zero() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department HAVING average_salary > 50000;"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary) AS average_salary
             FROM employees GROUP BY department HAVING average_salary > 50000 LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT department, COUNT(*) AS total
             FROM employees GROUP BY department HAVING missing > 1 LIMIT 0;"
        ),
        "column 'missing' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) AS total FROM employees HAVING no_such > 1;"
        ),
        "column 'no_such' does not exist in table 'employees'"
    );
}

#[test]
fn count_distinct_covers_integer_text_and_boolean() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT salary), COUNT(DISTINCT department), COUNT(DISTINCT active),
                    COUNT(DISTINCT name)
             FROM employees;"
        ),
        vec![vec![integer(4), integer(2), integer(2), integer(6)]]
    );
    database
        .execute("CREATE TABLE labels (value TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO labels VALUES ('a'), ('A'), ('a'), (''), (NULL);")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT COUNT(DISTINCT value) FROM labels;"),
        vec![vec![integer(3)]]
    );
}

#[test]
fn sum_distinct_adds_each_integer_once() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT SUM(salary), SUM(DISTINCT salary) FROM employees;"
        ),
        vec![vec![integer(200_000), integer(140_000)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, SUM(DISTINCT salary)
             FROM employees GROUP BY department;"
        ),
        vec![
            vec![text("eng"), integer(100_000)],
            vec![text("sales"), integer(30_000)],
            vec![Value::Null, integer(10_000)],
        ]
    );
}

#[test]
fn avg_distinct_is_an_exact_ratio() {
    let mut database = employees();
    assert_eq!(
        rows(&mut database, "SELECT AVG(DISTINCT salary) FROM employees;"),
        vec![vec![ratio(140_000, 4)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(salary), AVG(DISTINCT salary)
             FROM employees GROUP BY department HAVING AVG(DISTINCT salary) >= 50000;"
        ),
        vec![vec![text("eng"), ratio(160_000, 3), ratio(100_000, 2)]]
    );
}

#[test]
fn empty_and_all_null_inputs_follow_sql_nulls() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (amount INT, label TEXT);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount) FROM items;"
        ),
        vec![vec![integer(0), Value::Null, Value::Null]]
    );
    database
        .execute("INSERT INTO items VALUES (NULL, NULL), (NULL, NULL);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount)
             FROM items;"
        ),
        vec![vec![integer(2), integer(0), Value::Null, Value::Null]]
    );
    assert_eq!(
        failure(&mut database, "SELECT AVG(DISTINCT label) FROM items;"),
        "AVG requires INTEGER, not TEXT"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT label, COUNT(*), COUNT(DISTINCT amount), SUM(DISTINCT amount)
             FROM items GROUP BY label;"
        ),
        vec![vec![Value::Null, integer(2), integer(0), Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT label, COUNT(DISTINCT amount) AS kinds
             FROM items GROUP BY label HAVING kinds > 0 LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn duplicate_heavy_and_null_heavy_inputs_count_once() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (amount INT, label TEXT, flag BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO items VALUES
                (1, 'a', TRUE), (1, 'a', TRUE), (1, 'a', NULL),
                (2, 'a', FALSE), (2, NULL, FALSE),
                (NULL, 'b', NULL), (NULL, 'b', TRUE);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(DISTINCT amount), SUM(amount), SUM(DISTINCT amount),
                    AVG(DISTINCT amount), COUNT(DISTINCT label), COUNT(DISTINCT flag)
             FROM items;"
        ),
        vec![vec![
            integer(7),
            integer(2),
            integer(7),
            integer(3),
            ratio(3, 2),
            integer(2),
            integer(2),
        ]]
    );
}

#[test]
fn distinct_is_independent_for_each_group() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, active, COUNT(DISTINCT salary), SUM(DISTINCT salary)
             FROM employees GROUP BY department, active;"
        ),
        vec![
            vec![text("eng"), boolean(true), integer(2), integer(100_000)],
            vec![text("eng"), boolean(false), integer(1), integer(60_000)],
            vec![text("sales"), boolean(false), integer(1), integer(30_000)],
            vec![text("sales"), boolean(true), integer(0), Value::Null],
            vec![Value::Null, boolean(false), integer(1), integer(10_000)],
        ]
    );
}

#[test]
fn distinct_and_plain_calls_do_not_share_a_slot() {
    let mut database = employees();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(salary), COUNT(DISTINCT salary), SUM(salary), SUM(DISTINCT salary),
                AVG(salary), AVG(DISTINCT salary) FROM employees;",
    );
    assert_eq!(
        explained.rows[0][8],
        text(
            "0:COUNT(salary), 1:COUNT(DISTINCT salary), 2:SUM(salary), 3:SUM(DISTINCT salary), 4:AVG(salary), 5:AVG(DISTINCT salary)"
        )
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(salary), COUNT(DISTINCT salary), SUM(salary), SUM(DISTINCT salary)
             FROM employees;"
        ),
        vec![vec![
            integer(5),
            integer(4),
            integer(200_000),
            integer(140_000)
        ]]
    );
}

#[test]
fn repeated_distinct_calls_share_one_accumulator() {
    let mut database = employees();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(DISTINCT department), COUNT(DISTINCT employees.department),
                COUNT(DISTINCT department) FROM employees;",
    );
    assert_eq!(
        explained.rows[0][2],
        text("COUNT(DISTINCT department), COUNT(DISTINCT employees.department)")
    );
    assert_eq!(explained.rows[0][8], text("0:COUNT(DISTINCT department)"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT department), COUNT(DISTINCT employees.department)
             FROM employees;"
        ),
        vec![vec![integer(2), integer(2)]]
    );
}

#[test]
fn different_distinct_columns_stay_apart() {
    let mut database = employees();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(DISTINCT department), COUNT(DISTINCT name) FROM employees;",
    );
    assert_eq!(
        explained.rows[0][8],
        text("0:COUNT(DISTINCT department), 1:COUNT(DISTINCT name)")
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT department), COUNT(DISTINCT name) FROM employees;"
        ),
        vec![vec![integer(2), integer(6)]]
    );
}

#[test]
fn distinct_aliases_work_in_having_order_by_and_limit() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(DISTINCT employee_id) AS unique_count
             FROM employees GROUP BY department
             HAVING unique_count > 2
             ORDER BY unique_count DESC;"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(DISTINCT employee_id) AS unique_count
             FROM employees GROUP BY department
             HAVING COUNT(DISTINCT employee_id) > 2
             ORDER BY COUNT(DISTINCT employee_id) DESC
             LIMIT 1;"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department
             ORDER BY SUM(DISTINCT salary) DESC LIMIT 1;"
        ),
        vec![vec![text("eng")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, AVG(DISTINCT salary) AS average_salary
             FROM employees GROUP BY department
             HAVING average_salary >= 50000
             ORDER BY average_salary ASC;"
        ),
        vec![vec![text("eng"), ratio(100_000, 2)]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT department AS dept, COUNT(DISTINCT employee_id) AS unique_count
         FROM employees GROUP BY department
         HAVING unique_count > 2
         ORDER BY unique_count DESC;",
    );
    assert_eq!(explained.rows[0][8], text("0:COUNT(DISTINCT employee_id)"));
    assert_eq!(
        explained.rows[0][9],
        text("dept -> group 0 department, unique_count -> aggregate 0 COUNT(DISTINCT employee_id)")
    );
    assert_eq!(explained.rows[0][3], text("SeqScan"));
}

#[test]
fn join_duplicates_are_visible_before_distinct() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount),
                    AVG(DISTINCT o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name ORDER BY u.name ASC;"
        ),
        vec![
            vec![
                text("Ada"),
                integer(3),
                integer(2),
                integer(40),
                integer(30),
                ratio(30, 2),
            ],
            vec![
                text("Bea"),
                integer(2),
                integer(1),
                integer(10),
                integer(10),
                ratio(10, 1),
            ],
        ]
    );
}

#[test]
fn distinct_join_agrees_for_hash_nested_loop_and_index() {
    let mut database = shop();
    let hash_sql = "SELECT u.name, COUNT(DISTINCT o.amount), SUM(DISTINCT o.amount)
        FROM users u JOIN orders o ON u.id = o.user_id
        GROUP BY u.name ORDER BY u.name ASC;";
    let nested_sql = "SELECT u.name, COUNT(DISTINCT o.amount), SUM(DISTINCT o.amount)
        FROM users u JOIN orders o ON u.id = o.user_id OR u.id = o.user_id
        GROUP BY u.name ORDER BY u.name ASC;";
    let expected = rows(&mut database, hash_sql);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {hash_sql}")).rows[0][4],
        text("HashJoin")
    );
    assert_eq!(rows(&mut database, nested_sql), expected);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {nested_sql}")).rows[0][4],
        text("NestedLoopJoin")
    );

    let (_dir, path) = temporary("distinct-index-join");
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
    let mut sql = String::from("INSERT INTO orders VALUES ");
    for id in 0..70 {
        if id > 0 {
            sql.push(',');
        }
        sql.push_str(&format!("(1, {})", id % 5));
    }
    sql.push_str(", (NULL, 99)");
    database.execute(&sql).unwrap();
    let query_sql = "SELECT u.name, COUNT(*), COUNT(DISTINCT o.amount), SUM(DISTINCT o.amount)
        FROM users u JOIN orders o ON u.id = o.user_id
        GROUP BY u.name;";
    let hash_rows = rows(&mut database, query_sql);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let explained = query(&mut database, &format!("EXPLAIN {query_sql}"));
    assert_eq!(explained.rows[0][4], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][8], text("idx_orders_user"));
    let sum: i64 = (0..5).sum();
    assert_eq!(
        rows(&mut database, query_sql),
        vec![vec![text("Ada"), integer(70), integer(5), integer(sum)]]
    );
    assert_eq!(rows(&mut database, query_sql), hash_rows);
}

#[test]
fn late_sum_overflow_reports_an_error_in_either_aggregate_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (label TEXT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO items VALUES ('a', 1), ('b', 9223372036854775807);")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT label), SUM(amount) FROM items;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT SUM(amount), COUNT(DISTINCT label) FROM items;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT SUM(DISTINCT amount), COUNT(DISTINCT label) FROM items LIMIT 0;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(DISTINCT label) FROM items;"),
        vec![vec![integer(2)]]
    );
}

#[test]
fn alias_resolution_with_limit_zero_still_typechecks() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(DISTINCT employee_id) AS unique_count
             FROM employees GROUP BY department
             HAVING unique_count > 2 ORDER BY unique_count DESC LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department AS salary, COUNT(*) AS total
             FROM employees GROUP BY department HAVING salary = 'eng' LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn explain_does_not_execute_and_a_restart_keeps_the_rows() {
    let (_dir, path) = temporary("distinct-restart");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES ('eng', 1), ('eng', 9223372036854775807);")
        .unwrap();
    drop(database);
    let before = std::fs::read(&path).unwrap();
    let version = u32::from_le_bytes(before[32..36].try_into().unwrap());
    assert_eq!(version, 2);
    let mut database = Database::open(&path).unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT department AS dept, COUNT(DISTINCT salary) AS kinds,
                AVG(DISTINCT salary) AS average_salary
         FROM employees GROUP BY department
         HAVING kinds > 1 AND average_salary >= 1
         ORDER BY kinds DESC;",
    );
    assert_eq!(explained.rows[0][0], text("HashAggregate"));
    assert_eq!(
        explained.rows[0][2],
        text("COUNT(DISTINCT salary), AVG(DISTINCT salary)")
    );
    assert_eq!(
        explained.rows[0][9],
        text(
            "dept -> group 0 department, kinds -> aggregate 0 COUNT(DISTINCT salary), average_salary -> aggregate 1 AVG(DISTINCT salary)"
        )
    );
    assert_eq!(explained.rows[0][3], text("SeqScan"));
    let overflow = query(
        &mut database,
        "EXPLAIN SELECT SUM(DISTINCT salary) FROM employees;",
    );
    assert_eq!(overflow.rows[0][2], text("SUM(DISTINCT salary)"));
    assert_eq!(
        failure(&mut database, "SELECT SUM(DISTINCT salary) FROM employees;"),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        failure(
            &mut database,
            "EXPLAIN SELECT MAX(DISTINCT salary) FROM employees;"
        ),
        "DISTINCT is not supported for MAX"
    );
    assert_eq!(
        failure(&mut database, "SELECT MAX(DISTINCT salary) FROM employees;"),
        "DISTINCT is not supported for MAX"
    );
    drop(database);
    let after = std::fs::read(&path).unwrap();
    assert_eq!(after, before);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT salary), AVG(DISTINCT salary) FROM employees;"
        ),
        vec![vec![integer(2), ratio(9_223_372_036_854_775_808, 2)]]
    );
}

#[test]
fn unsupported_distinct_forms_are_rejected() {
    let mut database = employees();
    assert_eq!(
        failure(&mut database, "SELECT DISTINCT department FROM employees;"),
        "DISTINCT is not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT department, salary) FROM employees;"
        ),
        "aggregate functions take one argument"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT(DISTINCT *) FROM employees;"),
        "DISTINCT requires a column"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT(ALL salary) FROM employees;"),
        "ALL aggregates are not supported"
    );
    assert_eq!(
        failure(&mut database, "SELECT MAX(DISTINCT salary) FROM employees;"),
        "DISTINCT is not supported for MAX"
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM(DISTINCT 1) FROM employees;"),
        "aggregate arguments must be columns"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT SUM(DISTINCT salary + 1) FROM employees;"
        ),
        "operator + is not supported"
    );
}

#[test]
fn randomized_groups_match_an_independent_model() {
    // Seeds are fixed so a failure can be replayed.
    for seed in [0x0d13_u64, 0x51c0, 0xc0ff, 0x5eed, 0xa11c, 0x11ab] {
        for (name, sample) in cases(seed) {
            let mut database = Database::new();
            database
                .execute("CREATE TABLE sample (dept INT, amount INT, label TEXT, flag BOOLEAN);")
                .unwrap();
            if !sample.is_empty() {
                database.execute(&insert_sql(&sample)).unwrap();
            }
            let label = format!("seed {seed:#x} {name}");
            if name == "extremes" {
                assert_eq!(
                    rows(&mut database, EXTREME_SQL),
                    reference_extremes(&sample),
                    "{label}"
                );
                assert_eq!(
                    rows(&mut database, EXTREME_SEEN_SQL),
                    reference_extremes_seen(&sample),
                    "{label} first seen"
                );
                assert_eq!(
                    failure(&mut database, "SELECT SUM(DISTINCT amount) FROM sample;"),
                    "SUM overflows INTEGER",
                    "{label}"
                );
            } else {
                assert_eq!(
                    rows(&mut database, GROUP_SQL),
                    reference(&sample),
                    "{label}"
                );
                assert_eq!(
                    rows(&mut database, SEEN_SQL),
                    reference_seen(&sample),
                    "{label} first seen"
                );
            }
            assert_eq!(
                rows(&mut database, HAVING_SQL),
                having_alias_rows(&sample),
                "{label} having"
            );
            let distinct = i64::try_from(all_amounts(&sample).len()).unwrap();
            let average = distinct_average(&sample);
            if name == "extremes" {
                assert_eq!(
                    rows(
                        &mut database,
                        "SELECT COUNT(DISTINCT amount), COUNT(DISTINCT amount),
                                AVG(DISTINCT amount) FROM sample;"
                    ),
                    vec![vec![integer(distinct), integer(distinct), average]],
                    "{label} scalar"
                );
            } else {
                assert_eq!(
                    rows(
                        &mut database,
                        "SELECT COUNT(DISTINCT amount), COUNT(DISTINCT amount),
                                SUM(DISTINCT amount), AVG(DISTINCT amount) FROM sample;"
                    ),
                    vec![vec![
                        integer(distinct),
                        integer(distinct),
                        distinct_sum(&sample),
                        average,
                    ]],
                    "{label} scalar"
                );
            }
        }
    }
}

#[test]
fn having_prefers_an_explicit_alias_over_a_grouped_column() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT name AS department, COUNT(*) AS total
             FROM employees GROUP BY name HAVING department = 'eng';"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT name AS department, department AS who, COUNT(*) AS total
             FROM employees GROUP BY name, department
             HAVING department = 'Zoe' AND employees.department = 'eng';"
        ),
        vec![vec![text("Zoe"), text("eng"), integer(1)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT name AS department, department AS who, COUNT(*) AS total
             FROM employees GROUP BY name, department
             HAVING employees.department = 'sales' AND department <> 'sales';"
        ),
        vec![
            vec![text("Amy"), text("sales"), integer(1)],
            vec![text("Dee"), text("sales"), integer(1)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department dept, COUNT(*) total
             FROM employees GROUP BY department HAVING total >= 3 AND dept = 'eng';"
        ),
        vec![vec![text("eng"), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) AS \"Total\" FROM employees HAVING \"Total\" = 6;"
        ),
        vec![vec![integer(6)]]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) AS \"Total\" FROM employees HAVING total = 6;"
        ),
        "column 'total' does not exist in table 'employees'"
    );
}

#[test]
fn an_error_in_a_later_group_is_not_hidden() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (dept INT, amount INT);")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO items VALUES (1, 5), (1, 5), (2, {}), (2, 1);",
            i64::MAX
        ))
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT dept, SUM(DISTINCT amount) FROM items GROUP BY dept
             HAVING dept = 1 LIMIT 0;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT dept), SUM(DISTINCT amount) FROM items;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT dept, COUNT(DISTINCT amount) FROM items GROUP BY dept;"
        ),
        vec![vec![integer(1), integer(1)], vec![integer(2), integer(2)],]
    );
}

#[test]
fn integer_extremes_follow_distinct_arithmetic() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (amount INT);")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO items VALUES (1), ({min}), ({min}), ({max});",
            min = i64::MIN,
            max = i64::MAX
        ))
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount) FROM items;"
        ),
        vec![vec![integer(3), integer(0), ratio(0, 3)]]
    );

    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (amount INT);")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO items VALUES ({min}), ({min});",
            min = i64::MIN
        ))
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount) FROM items;"
        ),
        vec![vec![
            integer(1),
            integer(i64::MIN),
            ratio(i128::from(i64::MIN), 1),
        ]]
    );

    let mut database = Database::new();
    database
        .execute("CREATE TABLE items (amount INT);")
        .unwrap();
    database
        .execute(&format!(
            "INSERT INTO items VALUES ({min}), (-1);",
            min = i64::MIN
        ))
        .unwrap();
    assert_eq!(
        failure(&mut database, "SELECT SUM(DISTINCT amount) FROM items;"),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT amount), AVG(DISTINCT amount) FROM items;"
        ),
        vec![vec![integer(2), ratio(i128::from(i64::MIN) - 1, 2),]]
    );
}

#[test]
fn explain_and_select_report_the_same_binding_errors() {
    let mut database = employees();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 1), (2, 1);")
        .unwrap();
    let statements = [
        "SELECT MIN(DISTINCT salary) FROM employees",
        "SELECT COUNT(DISTINCT *) FROM employees",
        "SELECT department AS dept FROM employees WHERE dept = 'eng'",
        "SELECT department AS dept, COUNT(*) FROM employees GROUP BY dept",
        "SELECT department AS dept, COUNT(*) FROM employees GROUP BY department HAVING missing > 1",
        "SELECT salary, COUNT(*) FROM employees GROUP BY department HAVING salary > 1",
        "SELECT u.id, o.id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.id, o.id HAVING id > 0",
    ];
    for sql in statements {
        assert_eq!(
            failure(&mut database, &format!("EXPLAIN {sql};")),
            failure(&mut database, &format!("{sql};")),
            "{sql}"
        );
    }
}

#[test]
fn explain_index_scan_matches_the_filtered_distinct_result() {
    let (_dir, path) = temporary("distinct-index-scan");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE items (id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO items VALUES (1, 5), (1, 5), (2, 5), (1, 7);")
        .unwrap();
    let sql = "SELECT COUNT(DISTINCT amount), SUM(DISTINCT amount) FROM items WHERE id = 1;";
    let expected = vec![vec![integer(2), integer(12)]];
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {sql}")).rows[0][3],
        text("SeqScan")
    );
    assert_eq!(rows(&mut database, sql), expected);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_items_id ON items(id);")
        .unwrap();
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][3], text("IndexScan"));
    assert_eq!(explained.rows[0][4], text("idx_items_id"));
    assert_eq!(
        explained.rows[0][8],
        text("0:COUNT(DISTINCT amount), 1:SUM(DISTINCT amount)")
    );
    let before = std::fs::read(&path).unwrap();
    let wal = wal_length(&path);
    let _ = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(wal_length(&path), wal);
    assert_eq!(rows(&mut database, sql), expected);
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
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (1, 10), (1, 10), (1, 20), (2, 10), (2, NULL), (NULL, 99);",
        )
        .unwrap();
    database
}

struct SampleRow {
    dept: Option<i64>,
    amount: Option<i64>,
    label: Option<String>,
    flag: Option<bool>,
}

fn generate(seed: u64, count: usize) -> Vec<SampleRow> {
    let mut state = seed;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let dept_roll = next_value(&mut state);
        let amount_roll = next_value(&mut state);
        let label_roll = next_value(&mut state);
        let flag_roll = next_value(&mut state);
        rows.push(SampleRow {
            dept: (!dept_roll.is_multiple_of(5)).then_some(i64::try_from(dept_roll % 3).unwrap()),
            amount: (!amount_roll.is_multiple_of(4))
                .then_some(i64::try_from(amount_roll % 7).unwrap()),
            label: (!label_roll.is_multiple_of(6)).then(|| {
                let letter = b'a' + u8::try_from(label_roll % 3).unwrap();
                char::from(letter).to_string()
            }),
            flag: (!flag_roll.is_multiple_of(5)).then_some(flag_roll.is_multiple_of(2)),
        });
    }
    rows
}

fn next_value(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

fn insert_sql(sample: &[SampleRow]) -> String {
    let mut sql = String::from("INSERT INTO sample VALUES ");
    for (index, row) in sample.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        push_opt_int(&mut sql, row.dept);
        sql.push(',');
        push_opt_int(&mut sql, row.amount);
        sql.push(',');
        match &row.label {
            Some(label) => sql.push_str(&format!("'{label}'")),
            None => sql.push_str("NULL"),
        }
        sql.push(',');
        match row.flag {
            Some(true) => sql.push_str("TRUE"),
            Some(false) => sql.push_str("FALSE"),
            None => sql.push_str("NULL"),
        }
        sql.push(')');
    }
    sql.push(';');
    sql
}

fn push_opt_int(sql: &mut String, value: Option<i64>) {
    match value {
        Some(value) => sql.push_str(&value.to_string()),
        None => sql.push_str("NULL"),
    }
}

const GROUP_SQL: &str = "SELECT dept, COUNT(*), COUNT(DISTINCT amount), SUM(amount), SUM(DISTINCT amount), AVG(amount), AVG(DISTINCT amount), COUNT(DISTINCT label), COUNT(DISTINCT flag) FROM sample GROUP BY dept ORDER BY dept ASC;";

const SEEN_SQL: &str = "SELECT dept, COUNT(*), COUNT(DISTINCT amount), SUM(amount), SUM(DISTINCT amount), AVG(amount), AVG(DISTINCT amount), COUNT(DISTINCT label), COUNT(DISTINCT flag) FROM sample GROUP BY dept;";

const EXTREME_SQL: &str = "SELECT dept, COUNT(*), COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount), COUNT(DISTINCT label), COUNT(DISTINCT flag) FROM sample GROUP BY dept ORDER BY dept ASC;";

const EXTREME_SEEN_SQL: &str = "SELECT dept, COUNT(*), COUNT(DISTINCT amount), SUM(DISTINCT amount), AVG(DISTINCT amount), COUNT(DISTINCT label), COUNT(DISTINCT flag) FROM sample GROUP BY dept;";

const HAVING_SQL: &str = "SELECT dept AS grp, COUNT(DISTINCT amount) AS kinds, AVG(DISTINCT amount) AS average FROM sample GROUP BY dept HAVING kinds >= 1 ORDER BY average DESC, grp ASC;";

fn cases(seed: u64) -> Vec<(&'static str, Vec<SampleRow>)> {
    vec![
        ("mixed", generate(seed, 40)),
        ("duplicated", generate_duplicated(seed, 40)),
        ("cardinal", generate_cardinal(seed, 48)),
        ("nulls", generate_nulls(seed)),
        ("empty", Vec::new()),
        ("extremes", generate_extremes(24)),
    ]
}

fn generate_duplicated(seed: u64, count: usize) -> Vec<SampleRow> {
    let mut state = seed;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let roll = next_value(&mut state);
        rows.push(SampleRow {
            dept: (!roll.is_multiple_of(5)).then_some(i64::try_from(roll % 3).unwrap()),
            amount: (!roll.is_multiple_of(4)).then_some(7),
            label: (!roll.is_multiple_of(6)).then(|| {
                if roll.is_multiple_of(2) {
                    String::new()
                } else if roll.is_multiple_of(3) {
                    "a".to_string()
                } else {
                    "b".to_string()
                }
            }),
            flag: (!roll.is_multiple_of(5)).then_some(roll.is_multiple_of(2)),
        });
    }
    rows
}

fn generate_cardinal(seed: u64, count: usize) -> Vec<SampleRow> {
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let mixed = (index as u64).wrapping_add(seed);
        rows.push(SampleRow {
            dept: (!mixed.is_multiple_of(17)).then_some(i64::try_from(index % 4).unwrap()),
            amount: (!mixed.is_multiple_of(11)).then_some(i64::try_from(index).unwrap()),
            label: (!mixed.is_multiple_of(13)).then(|| {
                if index == 1 {
                    String::new()
                } else {
                    format!("t{index}")
                }
            }),
            flag: (!mixed.is_multiple_of(9)).then_some(index.is_multiple_of(2)),
        });
    }
    rows
}

fn generate_nulls(seed: u64) -> Vec<SampleRow> {
    let count = 8 + usize::try_from(seed % 5).unwrap();
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(SampleRow {
            dept: None,
            amount: None,
            label: None,
            flag: None,
        });
    }
    rows
}

fn generate_extremes(count: usize) -> Vec<SampleRow> {
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let family = index % 2;
        let amount = if index.is_multiple_of(7) {
            None
        } else if family == 0 {
            Some([i64::MIN, i64::MAX, 0][(index / 2) % 3])
        } else {
            Some([1, 2, -3][(index / 2) % 3])
        };
        rows.push(SampleRow {
            dept: Some(i64::try_from(family).unwrap()),
            amount,
            label: (!index.is_multiple_of(6)).then(|| {
                if family == 0 && index.is_multiple_of(4) {
                    String::new()
                } else if family == 0 {
                    "lo".to_string()
                } else {
                    "hi".to_string()
                }
            }),
            flag: (!index.is_multiple_of(5)).then_some(family == 0),
        });
    }
    rows
}

fn reference(sample: &[SampleRow]) -> Vec<Vec<Value>> {
    project_groups(sample, true, true)
}

fn reference_seen(sample: &[SampleRow]) -> Vec<Vec<Value>> {
    project_groups(sample, true, false)
}

fn reference_extremes(sample: &[SampleRow]) -> Vec<Vec<Value>> {
    project_groups(sample, false, true)
}

fn reference_extremes_seen(sample: &[SampleRow]) -> Vec<Vec<Value>> {
    project_groups(sample, false, false)
}

fn project_groups(sample: &[SampleRow], plain: bool, ordered: bool) -> Vec<Vec<Value>> {
    let mut groups = collect_groups(sample);
    if ordered {
        groups.sort_by(|left, right| cmp_dept(&left.0, &right.0));
    }
    groups
        .into_iter()
        .map(|(dept, group)| project(dept, &group, plain))
        .collect()
}

fn collect_groups(sample: &[SampleRow]) -> Vec<(Option<i64>, Group)> {
    let mut groups = Vec::new();
    for row in sample {
        if let Some((_, group)) = groups.iter_mut().find(|(dept, _)| *dept == row.dept) {
            absorb(group, row);
        } else {
            let mut group = Group::default();
            absorb(&mut group, row);
            groups.push((row.dept, group));
        }
    }
    groups
}

fn absorb(group: &mut Group, row: &SampleRow) {
    group.count += 1;
    if let Some(amount) = row.amount {
        group.amount_sum += i128::from(amount);
        group.amount_count += 1;
        group.amounts.insert(amount);
    }
    if let Some(label) = &row.label {
        group.labels.insert(label.clone());
    }
    if let Some(flag) = row.flag {
        group.flags.insert(flag);
    }
}

fn project(dept: Option<i64>, group: &Group, plain: bool) -> Vec<Value> {
    let distinct_sum: i128 = group.amounts.iter().copied().map(i128::from).sum();
    let mut row = vec![
        dept.map(integer).unwrap_or(Value::Null),
        integer(group.count),
        integer(i64::try_from(group.amounts.len()).unwrap()),
    ];
    if plain {
        row.push(optional_sum(group.amount_count, group.amount_sum));
    }
    row.push(if group.amounts.is_empty() {
        Value::Null
    } else {
        integer(i64::try_from(distinct_sum).unwrap())
    });
    if plain {
        row.push(if group.amount_count == 0 {
            Value::Null
        } else {
            ratio(group.amount_sum, group.amount_count)
        });
    }
    row.push(if group.amounts.is_empty() {
        Value::Null
    } else {
        ratio(distinct_sum, i64::try_from(group.amounts.len()).unwrap())
    });
    row.push(integer(i64::try_from(group.labels.len()).unwrap()));
    row.push(integer(i64::try_from(group.flags.len()).unwrap()));
    row
}

fn optional_sum(count: i64, sum: i128) -> Value {
    if count == 0 {
        Value::Null
    } else {
        integer(i64::try_from(sum).unwrap())
    }
}

fn having_alias_rows(sample: &[SampleRow]) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for (dept, group) in collect_groups(sample) {
        if group.amounts.is_empty() {
            continue;
        }
        let sum: i128 = group.amounts.iter().copied().map(i128::from).sum();
        let count = i64::try_from(group.amounts.len()).unwrap();
        kept.push((dept, sum, count));
    }
    kept.sort_by(|left, right| {
        cmp_ratio_desc((left.1, left.2), (right.1, right.2))
            .then_with(|| cmp_dept(&left.0, &right.0))
    });
    kept.into_iter()
        .map(|(dept, sum, count)| {
            vec![
                dept.map(integer).unwrap_or(Value::Null),
                integer(count),
                ratio(sum, count),
            ]
        })
        .collect()
}

fn cmp_dept(left: &Option<i64>, right: &Option<i64>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    }
}

fn cmp_ratio_desc(left: (i128, i64), right: (i128, i64)) -> Ordering {
    let left_cross = left.0 * i128::from(right.1);
    let right_cross = right.0 * i128::from(left.1);
    right_cross.cmp(&left_cross)
}

fn all_amounts(sample: &[SampleRow]) -> BTreeSet<i64> {
    sample.iter().filter_map(|row| row.amount).collect()
}

fn distinct_sum(sample: &[SampleRow]) -> Value {
    let amounts = all_amounts(sample);
    if amounts.is_empty() {
        return Value::Null;
    }
    let sum: i128 = amounts.iter().copied().map(i128::from).sum();
    integer(i64::try_from(sum).unwrap())
}

fn distinct_average(sample: &[SampleRow]) -> Value {
    let amounts = all_amounts(sample);
    if amounts.is_empty() {
        return Value::Null;
    }
    let sum: i128 = amounts.iter().copied().map(i128::from).sum();
    ratio(sum, i64::try_from(amounts.len()).unwrap())
}

#[derive(Default)]
struct Group {
    count: i64,
    amount_sum: i128,
    amount_count: i64,
    amounts: BTreeSet<i64>,
    labels: BTreeSet<String>,
    flags: BTreeSet<bool>,
}

#[test]
fn randomized_joins_match_a_nested_loop() {
    for seed in [0xb017_u64, 0x5e1f, 0xa91e] {
        let people = review_people();
        let orders = review_orders(seed);
        let wide = join_rows(&people, &orders, false, false);
        assert!(wide.len() >= 3, "seed {seed:#x}");
        assert_eq!(
            execute_join(&people, &orders, WIDE_SQL),
            wide,
            "seed {seed:#x} hash"
        );
        assert_eq!(
            execute_join(&people, &orders, WIDE_NESTED_SQL),
            wide,
            "seed {seed:#x} nested"
        );
        let tags = tag_rows(&people, &orders);
        assert_eq!(execute_join(&people, &orders, TAG_SQL), tags);
        assert_eq!(execute_join(&people, &orders, TAG_NESTED_SQL), tags);
        let filtered = having_join_rows(&people, &orders, 4);
        assert!(!filtered.is_empty(), "seed {seed:#x}");
        assert_eq!(execute_join(&people, &orders, FILTERED_SQL), filtered);
        assert_eq!(
            execute_join(&people, &orders, FILTERED_NESTED_SQL),
            filtered
        );
        assert_eq!(
            execute_join(&people, &orders, FILTERED_LIMIT_SQL),
            having_join_rows(&people, &orders, 1)
        );
        let items = review_items(seed);
        let joined = self_rows(&items);
        assert_eq!(execute_items(&items, SELF_SQL), joined, "seed {seed:#x}");
        assert_eq!(execute_items(&items, SELF_NESTED_SQL), joined);
        let kept = self_having_rows(&items);
        assert_eq!(kept, vec![vec![integer(1), integer(2)]]);
        assert_eq!(execute_items(&items, SELF_HAVING_SQL), kept);
        let mut database = Database::new();
        load_items(&mut database, &items);
        let explained = query(&mut database, &format!("EXPLAIN {SELF_HAVING_SQL}"));
        assert_eq!(explained.rows[0][4], text("HashJoin"));
        assert_eq!(explained.rows[0][7], text("b (items)"));
        assert_eq!(explained.rows[0][12], text("0:COUNT(DISTINCT a.amount)"));
        assert_eq!(
            explained.rows[0][13],
            text("grp -> group 0 a.bucket, kinds -> aggregate 0 COUNT(DISTINCT a.amount)")
        );

        for (people_n, order_n, build) in [(2, 8, "l (lefts)"), (8, 2, "r (rights)")] {
            let (people, orders) = band(seed, people_n, order_n);
            let expected = bucket_rows(&people, &orders);
            assert_eq!(execute_join(&people, &orders, BUCKET_SQL), expected);
            assert_eq!(execute_join(&people, &orders, BUCKET_NESTED_SQL), expected);
            let mut database = Database::new();
            load_join(&mut database, &people, &orders);
            let explained = query(&mut database, &format!("EXPLAIN {BUCKET_SQL}"));
            assert_eq!(explained.rows[0][4], text("HashJoin"));
            assert_eq!(explained.rows[0][7], text(build));
            let nested = query(&mut database, &format!("EXPLAIN {BUCKET_NESTED_SQL}"));
            assert_eq!(nested.rows[0][4], text("NestedLoopJoin"));
            assert_eq!(nested.rows[0][7], Value::Null);
        }
    }
    index_join_matches(0xb017);
}

fn review_people() -> Vec<Person> {
    vec![
        Person::new(Some(1), "a", 0, Some(true)),
        Person::new(Some(1), "a", 0, Some(false)),
        Person::new(Some(1), "b", 0, Some(true)),
        Person::new(Some(2), "a", 1, Some(true)),
        Person::new(None, "z", 2, Some(true)),
        Person::new(Some(3), "c", 2, Some(false)),
        Person::new(Some(4), "d", 3, None),
    ]
}

fn review_orders(seed: u64) -> Vec<Order> {
    let mut rows = vec![
        Order::new(Some(1), Some(10), "a"),
        Order::new(Some(1), Some(10), "a"),
        Order::new(Some(1), Some(20), "b"),
        Order::new(Some(1), None, "keep"),
        Order::new(Some(1), Some(-4), "a"),
        Order::new(Some(2), Some(10), "b"),
        Order::new(Some(2), Some(30), "a"),
        Order::new(None, Some(99), "a"),
        Order::new(Some(9), Some(5), "b"),
        Order::new(Some(4), Some(7), "a"),
        Order::new(Some(3), Some(8), "b"),
    ];
    let mut state = seed;
    for _ in 0..8 {
        let roll = next_value(&mut state);
        let person = (!roll.is_multiple_of(5)).then_some(i64::try_from(roll % 5).unwrap());
        let amount = (!roll.is_multiple_of(4)).then_some(i64::try_from(roll % 6).unwrap() - 1);
        let tag = if roll.is_multiple_of(2) { "a" } else { "b" };
        rows.push(Order::new(person, amount, tag));
    }
    rows
}

fn review_items(seed: u64) -> Vec<Item> {
    let nudge = i64::try_from((seed % 5) + 1).unwrap();
    vec![
        Item::new(1, 1, "a", Some(10)),
        Item::new(2, 1, "a", Some(10)),
        Item::new(3, 1, "b", Some(nudge)),
        Item::new(4, 2, "a", Some(10)),
        Item::new(5, 2, "c", None),
        Item::new(6, 2, "c", Some(10)),
    ]
}

fn band(seed: u64, people_n: usize, order_n: usize) -> (Vec<Person>, Vec<Order>) {
    let nudge = i64::try_from(seed % 3).unwrap();
    let mut people = Vec::with_capacity(people_n);
    for index in 0..people_n {
        let id = if index % 11 == 10 {
            None
        } else {
            Some(i64::try_from(index % 3).unwrap())
        };
        let name = char::from(b'a' + u8::try_from(index % 3).unwrap()).to_string();
        people.push(Person::new(
            id,
            &name,
            i64::try_from(index % 2).unwrap(),
            Some(true),
        ));
    }
    let mut orders = Vec::with_capacity(order_n);
    for index in 0..order_n {
        let person = if index.is_multiple_of(5) {
            None
        } else {
            Some(i64::try_from(index % 3).unwrap())
        };
        let amount = if index.is_multiple_of(4) {
            None
        } else {
            Some(i64::try_from(index % 5).unwrap() + nudge)
        };
        let tag = if index.is_multiple_of(2) { "a" } else { "b" };
        orders.push(Order::new(person, amount, tag));
    }
    (people, orders)
}

fn index_join_matches(seed: u64) {
    let (people, orders) = band(seed, 2, 140);
    let expected = positive_bucket_rows(&people, &orders);
    assert!(!expected.is_empty());
    assert_eq!(execute_join(&people, &orders, INDEX_SQL), expected);
    assert_eq!(execute_join(&people, &orders, INDEX_NESTED_SQL), expected);

    let (_dir, path) = temporary("distinct-index-reference");
    let mut database = Database::open(&path).unwrap();
    load_join(&mut database, &people, &orders);
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {INDEX_SQL}")).rows[0][4],
        text("HashJoin")
    );
    assert_eq!(rows(&mut database, INDEX_SQL), expected);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_rights_person ON rights(left_id);")
        .unwrap();
    let explained = query(&mut database, &format!("EXPLAIN {INDEX_SQL}"));
    assert_eq!(explained.rows[0][4], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][8], text("idx_rights_person"));
    assert_eq!(
        explained.rows[0][10],
        text("l.id = r.left_id AND r.amount > 0")
    );
    assert_eq!(explained.rows[0][11], text("l.flag = TRUE"));
    assert_eq!(
        explained.rows[0][12],
        text(
            "0:COUNT(*), 1:SUM(r.amount), 2:SUM(DISTINCT r.amount), 3:COUNT(DISTINCT r.amount), 4:COUNT(DISTINCT l.name)"
        )
    );
    let before = std::fs::read(&path).unwrap();
    let wal = wal_length(&path);
    let _ = query(&mut database, &format!("EXPLAIN {INDEX_SQL}"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(wal_length(&path), wal);
    assert_eq!(rows(&mut database, INDEX_SQL), expected);
    assert_eq!(rows(&mut database, INDEX_NESTED_SQL), expected);
}

fn wal_length(path: &std::path::Path) -> u64 {
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    std::fs::metadata(wal).map(|meta| meta.len()).unwrap_or(0)
}

fn execute_join(people: &[Person], orders: &[Order], sql: &str) -> Vec<Vec<Value>> {
    let mut database = Database::new();
    load_join(&mut database, people, orders);
    rows(&mut database, sql)
}

fn execute_items(items: &[Item], sql: &str) -> Vec<Vec<Value>> {
    let mut database = Database::new();
    load_items(&mut database, items);
    rows(&mut database, sql)
}

fn load_join(database: &mut Database, people: &[Person], orders: &[Order]) {
    database
        .execute("CREATE TABLE lefts (id INT, name TEXT, bucket INT, flag BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE rights (left_id INT, amount INT, tag TEXT);")
        .unwrap();
    database.execute(&people_sql(people)).unwrap();
    database.execute(&orders_sql(orders)).unwrap();
}

fn load_items(database: &mut Database, items: &[Item]) {
    database
        .execute("CREATE TABLE items (id INT, bucket INT, name TEXT, amount INT);")
        .unwrap();
    let mut sql = String::from("INSERT INTO items VALUES ");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        sql.push_str(&item.id.to_string());
        sql.push(',');
        sql.push_str(&item.bucket.to_string());
        sql.push_str(",'");
        sql.push_str(&item.name);
        sql.push_str("',");
        push_opt_int(&mut sql, item.amount);
        sql.push(')');
    }
    sql.push(';');
    database.execute(&sql).unwrap();
}

fn people_sql(people: &[Person]) -> String {
    let mut sql = String::from("INSERT INTO lefts VALUES ");
    for (index, person) in people.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        push_opt_int(&mut sql, person.id);
        sql.push_str(",'");
        sql.push_str(&person.name);
        sql.push_str("',");
        sql.push_str(&person.bucket.to_string());
        sql.push(',');
        match person.flag {
            Some(true) => sql.push_str("TRUE"),
            Some(false) => sql.push_str("FALSE"),
            None => sql.push_str("NULL"),
        }
        sql.push(')');
    }
    sql.push(';');
    sql
}

fn orders_sql(orders: &[Order]) -> String {
    let mut sql = String::from("INSERT INTO rights VALUES ");
    for (index, order) in orders.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        push_opt_int(&mut sql, order.person);
        sql.push(',');
        push_opt_int(&mut sql, order.amount);
        sql.push_str(",'");
        sql.push_str(&order.tag);
        sql.push_str("')");
    }
    sql.push(';');
    sql
}

fn join_rows(
    people: &[Person],
    orders: &[Order],
    positive: bool,
    flag_true: bool,
) -> Vec<Vec<Value>> {
    join_groups(people, orders, positive, flag_true)
        .into_iter()
        .map(|(key, agg)| project_join(&key, &agg))
        .collect()
}

fn join_groups(
    people: &[Person],
    orders: &[Order],
    positive: bool,
    flag_true: bool,
) -> Vec<((i64, String), JoinAgg)> {
    let mut groups = Vec::new();
    for person in people {
        if flag_true && person.flag != Some(true) {
            continue;
        }
        for order in orders {
            if person.id.is_none() || order.person != person.id {
                continue;
            }
            if positive && !order.amount.is_some_and(|amount| amount > 0) {
                continue;
            }
            let key = (person.bucket, person.name.clone());
            let index = group_index(&mut groups, &key);
            let agg = &mut groups[index].1;
            agg.pairs += 1;
            if let Some(amount) = order.amount {
                agg.amount_sum += i128::from(amount);
                agg.amount_count += 1;
                agg.amounts.insert(amount);
            }
            agg.tags.insert(order.tag.clone());
            agg.names.insert(person.name.clone());
            if let Some(flag) = person.flag {
                agg.flags.insert(flag);
            }
        }
    }
    groups
}

fn group_index(groups: &mut Vec<((i64, String), JoinAgg)>, key: &(i64, String)) -> usize {
    if let Some(index) = groups.iter().position(|(found, _)| found == key) {
        return index;
    }
    groups.push((key.clone(), JoinAgg::default()));
    groups.len() - 1
}

fn project_join(key: &(i64, String), agg: &JoinAgg) -> Vec<Value> {
    let distinct: i128 = agg.amounts.iter().copied().map(i128::from).sum();
    vec![
        integer(key.0),
        text(&key.1),
        integer(agg.pairs),
        integer(i64::try_from(agg.amounts.len()).unwrap()),
        optional_sum(agg.amount_count, agg.amount_sum),
        if agg.amounts.is_empty() {
            Value::Null
        } else {
            integer(i64::try_from(distinct).unwrap())
        },
        if agg.amounts.is_empty() {
            Value::Null
        } else {
            ratio(distinct, i64::try_from(agg.amounts.len()).unwrap())
        },
        integer(i64::try_from(agg.tags.len()).unwrap()),
        integer(i64::try_from(agg.names.len()).unwrap()),
        integer(i64::try_from(agg.flags.len()).unwrap()),
    ]
}

fn having_join_rows(people: &[Person], orders: &[Order], limit: usize) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for (key, agg) in join_groups(people, orders, true, true) {
        if agg.amounts.len() <= 1 || key.1.is_empty() {
            continue;
        }
        let sum: i128 = agg.amounts.iter().copied().map(i128::from).sum();
        let count = i64::try_from(agg.amounts.len()).unwrap();
        kept.push((key, sum, count));
    }
    kept.sort_by(|left, right| {
        cmp_ratio_desc((left.1, left.2), (right.1, right.2)).then_with(|| left.0.1.cmp(&right.0.1))
    });
    kept.truncate(limit);
    kept.into_iter()
        .map(|(key, sum, count)| {
            vec![
                integer(key.0),
                text(&key.1),
                integer(count),
                ratio(sum, count),
            ]
        })
        .collect()
}

fn tag_rows(people: &[Person], orders: &[Order]) -> Vec<Vec<Value>> {
    let mut groups: Vec<(String, JoinAgg)> = Vec::new();
    for person in people {
        for order in orders {
            if person.id.is_none() || order.person != person.id || order.amount.is_none() {
                continue;
            }
            let index = if let Some(index) = groups.iter().position(|(tag, _)| tag == &order.tag) {
                index
            } else {
                groups.push((order.tag.clone(), JoinAgg::default()));
                groups.len() - 1
            };
            let agg = &mut groups[index].1;
            let amount = order.amount.expect("amount was checked");
            agg.amount_sum += i128::from(amount);
            agg.amount_count += 1;
            agg.amounts.insert(amount);
            agg.names.insert(person.name.clone());
        }
    }
    groups
        .into_iter()
        .map(|(tag, agg)| {
            let distinct: i128 = agg.amounts.iter().copied().map(i128::from).sum();
            vec![
                text(&tag),
                integer(i64::try_from(agg.names.len()).unwrap()),
                optional_sum(agg.amount_count, agg.amount_sum),
                integer(i64::try_from(distinct).unwrap()),
                ratio(distinct, i64::try_from(agg.amounts.len()).unwrap()),
            ]
        })
        .collect()
}

fn bucket_rows(people: &[Person], orders: &[Order]) -> Vec<Vec<Value>> {
    bucket_aggregate(people, orders, false)
}

fn positive_bucket_rows(people: &[Person], orders: &[Order]) -> Vec<Vec<Value>> {
    bucket_aggregate(people, orders, true)
}

fn bucket_aggregate(people: &[Person], orders: &[Order], positive: bool) -> Vec<Vec<Value>> {
    let mut groups: Vec<(i64, JoinAgg)> = Vec::new();
    for person in people {
        if positive && person.flag != Some(true) {
            continue;
        }
        for order in orders {
            if person.id.is_none() || order.person != person.id {
                continue;
            }
            if positive && !order.amount.is_some_and(|amount| amount > 0) {
                continue;
            }
            let index = if let Some(index) = groups
                .iter()
                .position(|(bucket, _)| *bucket == person.bucket)
            {
                index
            } else {
                groups.push((person.bucket, JoinAgg::default()));
                groups.len() - 1
            };
            let agg = &mut groups[index].1;
            agg.pairs += 1;
            if let Some(amount) = order.amount {
                agg.amount_sum += i128::from(amount);
                agg.amount_count += 1;
                agg.amounts.insert(amount);
            }
            agg.names.insert(person.name.clone());
        }
    }
    groups
        .into_iter()
        .map(|(bucket, agg)| {
            let distinct: i128 = agg.amounts.iter().copied().map(i128::from).sum();
            vec![
                integer(bucket),
                integer(agg.pairs),
                optional_sum(agg.amount_count, agg.amount_sum),
                if agg.amounts.is_empty() {
                    Value::Null
                } else {
                    integer(i64::try_from(distinct).unwrap())
                },
                integer(i64::try_from(agg.amounts.len()).unwrap()),
                integer(i64::try_from(agg.names.len()).unwrap()),
            ]
        })
        .collect()
}

fn self_rows(items: &[Item]) -> Vec<Vec<Value>> {
    self_groups(items)
        .into_iter()
        .map(|(bucket, agg)| {
            let left_sum: i128 = agg.left_amounts.iter().copied().map(i128::from).sum();
            vec![
                integer(bucket),
                integer(agg.pairs),
                optional_sum(agg.left_count, agg.left_sum),
                if agg.left_amounts.is_empty() {
                    Value::Null
                } else {
                    integer(i64::try_from(left_sum).unwrap())
                },
                integer(i64::try_from(agg.right_amounts.len()).unwrap()),
                integer(i64::try_from(agg.names.len()).unwrap()),
            ]
        })
        .collect()
}

fn self_having_rows(items: &[Item]) -> Vec<Vec<Value>> {
    self_groups(items)
        .into_iter()
        .filter(|(_, agg)| agg.left_amounts.len() > 1)
        .map(|(bucket, agg)| {
            vec![
                integer(bucket),
                integer(i64::try_from(agg.left_amounts.len()).unwrap()),
            ]
        })
        .collect()
}

fn self_groups(items: &[Item]) -> Vec<(i64, SelfAgg)> {
    let mut groups = Vec::new();
    for left in items {
        for right in items {
            if left.bucket != right.bucket || left.id == right.id {
                continue;
            }
            let index =
                if let Some(index) = groups.iter().position(|(bucket, _)| *bucket == left.bucket) {
                    index
                } else {
                    groups.push((left.bucket, SelfAgg::default()));
                    groups.len() - 1
                };
            let agg = &mut groups[index].1;
            agg.pairs += 1;
            if let Some(amount) = left.amount {
                agg.left_sum += i128::from(amount);
                agg.left_count += 1;
                agg.left_amounts.insert(amount);
            }
            if let Some(amount) = right.amount {
                agg.right_amounts.insert(amount);
            }
            agg.names.insert(left.name.clone());
        }
    }
    groups
}

struct Person {
    id: Option<i64>,
    name: String,
    bucket: i64,
    flag: Option<bool>,
}

impl Person {
    fn new(id: Option<i64>, name: &str, bucket: i64, flag: Option<bool>) -> Self {
        Self {
            id,
            name: name.to_string(),
            bucket,
            flag,
        }
    }
}

struct Order {
    person: Option<i64>,
    amount: Option<i64>,
    tag: String,
}

impl Order {
    fn new(person: Option<i64>, amount: Option<i64>, tag: &str) -> Self {
        Self {
            person,
            amount,
            tag: tag.to_string(),
        }
    }
}

struct Item {
    id: i64,
    bucket: i64,
    name: String,
    amount: Option<i64>,
}

impl Item {
    fn new(id: i64, bucket: i64, name: &str, amount: Option<i64>) -> Self {
        Self {
            id,
            bucket,
            name: name.to_string(),
            amount,
        }
    }
}

#[derive(Default)]
struct JoinAgg {
    pairs: i64,
    amount_sum: i128,
    amount_count: i64,
    amounts: BTreeSet<i64>,
    tags: BTreeSet<String>,
    names: BTreeSet<String>,
    flags: BTreeSet<bool>,
}

#[derive(Default)]
struct SelfAgg {
    pairs: i64,
    left_sum: i128,
    left_count: i64,
    left_amounts: BTreeSet<i64>,
    right_amounts: BTreeSet<i64>,
    names: BTreeSet<String>,
}

const WIDE_SQL: &str = "SELECT l.bucket, l.name, COUNT(*), COUNT(DISTINCT r.amount), SUM(r.amount), SUM(DISTINCT r.amount), AVG(DISTINCT r.amount), COUNT(DISTINCT r.tag), COUNT(DISTINCT l.name), COUNT(DISTINCT l.flag) FROM lefts l JOIN rights r ON l.id = r.left_id GROUP BY l.bucket, l.name;";

const WIDE_NESTED_SQL: &str = "SELECT l.bucket, l.name, COUNT(*), COUNT(DISTINCT r.amount), SUM(r.amount), SUM(DISTINCT r.amount), AVG(DISTINCT r.amount), COUNT(DISTINCT r.tag), COUNT(DISTINCT l.name), COUNT(DISTINCT l.flag) FROM lefts l JOIN rights r ON l.id = r.left_id OR l.id = r.left_id GROUP BY l.bucket, l.name;";

const TAG_SQL: &str = "SELECT r.tag, COUNT(DISTINCT l.name), SUM(r.amount), SUM(DISTINCT r.amount), AVG(DISTINCT r.amount) FROM lefts l JOIN rights r ON l.id = r.left_id WHERE r.amount IS NOT NULL GROUP BY r.tag;";

const TAG_NESTED_SQL: &str = "SELECT r.tag, COUNT(DISTINCT l.name), SUM(r.amount), SUM(DISTINCT r.amount), AVG(DISTINCT r.amount) FROM lefts l JOIN rights r ON l.id = r.left_id OR l.id = r.left_id WHERE r.amount IS NOT NULL GROUP BY r.tag;";

const FILTERED_SQL: &str = "SELECT l.bucket AS grp, l.name AS who, COUNT(DISTINCT r.amount) AS kinds, AVG(DISTINCT r.amount) AS average FROM lefts l JOIN rights r ON l.id = r.left_id AND r.amount > 0 WHERE l.flag = TRUE GROUP BY l.bucket, l.name HAVING kinds > 1 AND who IS NOT NULL ORDER BY average DESC, who ASC LIMIT 4;";

const FILTERED_NESTED_SQL: &str = "SELECT l.bucket AS grp, l.name AS who, COUNT(DISTINCT r.amount) AS kinds, AVG(DISTINCT r.amount) AS average FROM lefts l JOIN rights r ON (l.id = r.left_id OR l.id = r.left_id) AND r.amount > 0 WHERE l.flag = TRUE GROUP BY l.bucket, l.name HAVING kinds > 1 AND who IS NOT NULL ORDER BY average DESC, who ASC LIMIT 4;";

const FILTERED_LIMIT_SQL: &str = "SELECT l.bucket AS grp, l.name AS who, COUNT(DISTINCT r.amount) AS kinds, AVG(DISTINCT r.amount) AS average FROM lefts l JOIN rights r ON l.id = r.left_id AND r.amount > 0 WHERE l.flag = TRUE GROUP BY l.bucket, l.name HAVING kinds > 1 AND who IS NOT NULL ORDER BY average DESC, who ASC LIMIT 1;";

const BUCKET_SQL: &str = "SELECT l.bucket, COUNT(*), SUM(r.amount), SUM(DISTINCT r.amount), COUNT(DISTINCT r.amount), COUNT(DISTINCT l.name) FROM lefts l JOIN rights r ON l.id = r.left_id GROUP BY l.bucket;";

const BUCKET_NESTED_SQL: &str = "SELECT l.bucket, COUNT(*), SUM(r.amount), SUM(DISTINCT r.amount), COUNT(DISTINCT r.amount), COUNT(DISTINCT l.name) FROM lefts l JOIN rights r ON l.id = r.left_id OR l.id = r.left_id GROUP BY l.bucket;";

const SELF_SQL: &str = "SELECT a.bucket, COUNT(*), SUM(a.amount), SUM(DISTINCT a.amount), COUNT(DISTINCT b.amount), COUNT(DISTINCT a.name) FROM items a JOIN items b ON a.bucket = b.bucket AND a.id <> b.id GROUP BY a.bucket;";

const SELF_NESTED_SQL: &str = "SELECT a.bucket, COUNT(*), SUM(a.amount), SUM(DISTINCT a.amount), COUNT(DISTINCT b.amount), COUNT(DISTINCT a.name) FROM items a JOIN items b ON (a.bucket = b.bucket OR a.bucket = b.bucket) AND a.id <> b.id GROUP BY a.bucket;";

const SELF_HAVING_SQL: &str = "SELECT a.bucket AS grp, COUNT(DISTINCT a.amount) AS kinds FROM items a JOIN items b ON a.bucket = b.bucket AND a.id <> b.id GROUP BY a.bucket HAVING kinds > 1;";

const INDEX_SQL: &str = "SELECT l.bucket, COUNT(*), SUM(r.amount), SUM(DISTINCT r.amount), COUNT(DISTINCT r.amount), COUNT(DISTINCT l.name) FROM lefts l JOIN rights r ON l.id = r.left_id AND r.amount > 0 WHERE l.flag = TRUE GROUP BY l.bucket;";

const INDEX_NESTED_SQL: &str = "SELECT l.bucket, COUNT(*), SUM(r.amount), SUM(DISTINCT r.amount), COUNT(DISTINCT r.amount), COUNT(DISTINCT l.name) FROM lefts l JOIN rights r ON (l.id = r.left_id OR l.id = r.left_id) AND r.amount > 0 WHERE l.flag = TRUE GROUP BY l.bucket;";

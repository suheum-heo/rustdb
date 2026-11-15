//! `HAVING` and `COUNT` / `SUM` over one `INNER JOIN`.

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

fn shop() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, dept TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (id INT, user_id INT, amount INT, paid BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES
                (1, 'Ada', 'eng'),
                (2, 'Bea', 'eng'),
                (3, 'Cy', 'sales'),
                (4, 'Dee', NULL),
                (NULL, 'Eve', 'eng');",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (1, 1, 10, TRUE),
                (2, 1, NULL, TRUE),
                (3, 1, 30, FALSE),
                (4, 2, 5, TRUE),
                (5, 3, NULL, TRUE),
                (6, NULL, 7, TRUE),
                (7, 4, 100, TRUE);",
        )
        .unwrap();
    database
}

const GROUPED: &str = "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
    FROM users u JOIN orders o ON u.id = o.user_id
    GROUP BY u.name";

#[test]
fn having_count_keeps_groups_at_or_above_the_threshold() {
    let mut database = shop();
    assert_eq!(
        rows(&mut database, &format!("{GROUPED} HAVING COUNT(*) >= 2;")),
        vec![vec![text("Ada"), integer(3), integer(2), integer(40)]]
    );
    assert_eq!(
        rows(&mut database, &format!("{GROUPED} HAVING COUNT(*) >= 1;")),
        vec![
            vec![text("Ada"), integer(3), integer(2), integer(40)],
            vec![text("Bea"), integer(1), integer(1), integer(5)],
            vec![text("Cy"), integer(1), integer(0), Value::Null],
            vec![text("Dee"), integer(1), integer(1), integer(100)],
        ]
    );
    assert!(rows(&mut database, &format!("{GROUPED} HAVING COUNT(*) > 3;")).is_empty());
}

#[test]
fn having_sum_uses_the_aggregate_value() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            &format!("{GROUPED} HAVING SUM(o.amount) > 10;")
        ),
        vec![
            vec![text("Ada"), integer(3), integer(2), integer(40)],
            vec![text("Dee"), integer(1), integer(1), integer(100)],
        ]
    );
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES ('eng', 60000), ('eng', 50000), ('sales', 40000);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, SUM(salary) FROM employees
             GROUP BY department HAVING SUM(salary) > 100000;"
        ),
        vec![vec![text("eng"), integer(110_000)]]
    );
}

#[test]
fn having_without_group_by_can_drop_the_scalar_row() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             HAVING COUNT(*) >= 1;"
        ),
        vec![vec![integer(6), integer(145)]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id HAVING COUNT(*) > 6;"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users HAVING COUNT(*) = 5;"
        ),
        vec![vec![integer(5)]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users HAVING COUNT(*) > 5;"
        )
        .is_empty()
    );
}

#[test]
fn having_on_an_empty_table_does_not_invent_a_group() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             HAVING COUNT(*) = 0;"
        ),
        vec![vec![integer(0), Value::Null]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id HAVING COUNT(*) >= 1;"
        )
        .is_empty()
    );
    assert!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name HAVING COUNT(*) = 0;"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id
             HAVING SUM(o.amount) IS NULL;"
        ),
        vec![vec![Value::Null]]
    );
}

#[test]
fn having_unknown_drops_the_group() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            &format!("{GROUPED} HAVING SUM(o.amount) > 0;")
        ),
        vec![
            vec![text("Ada"), integer(3), integer(2), integer(40)],
            vec![text("Bea"), integer(1), integer(1), integer(5)],
            vec![text("Dee"), integer(1), integer(1), integer(100)],
        ]
    );
    assert!(rows(&mut database, &format!("{GROUPED} HAVING COUNT(*) > NULL;")).is_empty());
    assert_eq!(
        rows(
            &mut database,
            &format!("{GROUPED} HAVING COUNT(*) >= 1 OR SUM(o.amount) > NULL;")
        )
        .len(),
        4
    );
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.name = 'Cy' HAVING SUM(o.amount) > 0;"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.name = 'Cy';"
        ),
        vec![vec![integer(1), Value::Null]]
    );
}

#[test]
fn having_only_aggregates_are_computed() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name HAVING SUM(o.amount) > 10;"
        ),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name HAVING SUM(o.amount) > 10;",
    );
    assert_eq!(explained.rows[0][2], text("SUM(o.amount)"));
    assert_eq!(explained.rows[0][3], text("SUM(o.amount) > 10"));
}

#[test]
fn repeated_aggregates_share_one_value_across_select_having_and_order() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             HAVING COUNT(*) >= 1
             ORDER BY COUNT(*);"
        ),
        vec![vec![integer(6), integer(6)]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*)
         FROM users u JOIN orders o ON u.id = o.user_id
         HAVING COUNT(*) >= 1
         ORDER BY COUNT(*);",
    );
    assert_eq!(explained.rows[0][2], text("COUNT(*)"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(amount), COUNT(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(4), integer(4)]]
    );
}

#[test]
fn ungrouped_columns_are_rejected_before_reading_rows() {
    let mut database = shop();
    assert_eq!(
        failure(&mut database, "SELECT name, COUNT(*) FROM users;"),
        "column 'name' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id HAVING u.name = 'Ada';"
        ),
        "column 'name' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name HAVING u.dept = 'eng';"
        ),
        "column 'dept' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM users HAVING COUNT(*) > 'x';"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    database.execute("CREATE TABLE empty (id INT);").unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM empty HAVING COUNT(*) > 'x';"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM empty HAVING lower(id) = 'a';"
        ),
        "function calls are not supported"
    );
}

#[test]
fn nested_aggregates_are_rejected() {
    let mut database = shop();
    assert_eq!(
        failure(&mut database, "SELECT SUM(COUNT(*)) FROM users;"),
        "nested aggregates are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM users HAVING SUM(COUNT(*)) > 1;"
        ),
        "nested aggregates are not supported"
    );
    assert_eq!(
        failure(&mut database, "SELECT percentile(amount) FROM orders;"),
        "function calls are not supported"
    );
}

#[test]
fn ambiguous_join_columns_are_rejected() {
    let mut database = shop();
    assert_eq!(
        failure(
            &mut database,
            "SELECT id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY id;"
        ),
        "column 'id' is ambiguous"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.id;"
        ),
        vec![
            vec![integer(1), integer(3)],
            vec![integer(2), integer(1)],
            vec![integer(3), integer(1)],
            vec![integer(4), integer(1)],
        ]
    );
}

#[test]
fn aliases_scope_columns() {
    let mut database = shop();
    assert_eq!(
        failure(
            &mut database,
            "SELECT users.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY users.name;"
        ),
        "table 'users' is not in scope"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY name;"
        )
        .len(),
        4
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;"
        )
        .len(),
        6
    );
}

#[test]
fn count_star_counts_joined_rows() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(6)]]
    );
}

#[test]
fn count_column_skips_nulls_on_a_join() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(o.amount), COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(4), integer(6)]]
    );
    let grouped = rows(&mut database, &format!("{GROUPED};"));
    assert_eq!(
        grouped[2],
        vec![text("Cy"), integer(1), integer(0), Value::Null]
    );
}

#[test]
fn sum_adds_joined_integers() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(145)]]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT SUM(o.paid) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        "SUM requires INTEGER, not BOOLEAN"
    );
}

#[test]
fn group_by_either_table() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept;"
        ),
        vec![
            vec![text("eng"), integer(4), integer(45)],
            vec![text("sales"), integer(1), Value::Null],
            vec![Value::Null, integer(1), integer(100)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT o.user_id, COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY o.user_id;"
        ),
        vec![
            vec![integer(1), integer(3)],
            vec![integer(2), integer(1)],
            vec![integer(3), integer(1)],
            vec![integer(4), integer(1)],
        ]
    );
}

#[test]
fn multiple_grouping_columns() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept, u.name, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept, u.name;"
        ),
        vec![
            vec![text("eng"), text("Ada"), integer(3), integer(40)],
            vec![text("eng"), text("Bea"), integer(1), integer(5)],
            vec![text("sales"), text("Cy"), integer(1), Value::Null],
            vec![Value::Null, text("Dee"), integer(1), integer(100)],
        ]
    );
}

#[test]
fn null_and_duplicate_join_keys_follow_sql_equality() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (1, 'Al'), (NULL, 'Eve');")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, NULL), (NULL, 7);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name;"
        ),
        vec![
            vec![text("Ada"), integer(2), integer(1), integer(10)],
            vec![text("Al"), integer(2), integer(1), integer(10)],
        ]
    );
}

#[test]
fn where_filters_before_aggregation() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE o.paid = TRUE
             GROUP BY u.name;"
        ),
        vec![
            vec![text("Ada"), integer(2), integer(10)],
            vec![text("Bea"), integer(1), integer(5)],
            vec![text("Cy"), integer(1), Value::Null],
            vec![text("Dee"), integer(1), integer(100)],
        ]
    );
    assert!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE o.paid = TRUE
             GROUP BY u.name
             HAVING COUNT(*) >= 3;"
        )
        .is_empty()
    );
}

#[test]
fn having_filters_after_aggregation() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING COUNT(*) >= 3;"
        ),
        vec![vec![text("Ada"), integer(3)]]
    );
}

#[test]
fn order_by_and_limit_run_after_having() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             ORDER BY SUM(o.amount) DESC NULLS LAST
             LIMIT 1;"
        ),
        vec![vec![text("Dee"), integer(100)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING SUM(o.amount) > 10
             ORDER BY SUM(o.amount) ASC
             LIMIT 1;"
        ),
        vec![vec![text("Ada"), integer(40)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name ORDER BY SUM(o.amount) DESC NULLS LAST LIMIT 2;"
        ),
        vec![vec![text("Dee")], vec![text("Ada")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name LIMIT 1;"
        ),
        vec![vec![text("Ada"), integer(3)]]
    );
}

#[test]
fn a_self_join_groups_matched_pairs() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE staff (team INT, name TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO staff VALUES (1, 'Ada'), (1, 'Bea'), (NULL, 'Cy'), (2, 'Dee');")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT e.name, COUNT(*)
             FROM staff e JOIN staff b ON e.team = b.team
             GROUP BY e.name;"
        ),
        vec![
            vec![text("Ada"), integer(2)],
            vec![text("Bea"), integer(2)],
            vec![text("Dee"), integer(1)],
        ]
    );
}

#[test]
fn restart_keeps_joined_aggregates_and_explain_does_not_write() {
    let (_dir, path) = temporary("join-agg");
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
        .execute("INSERT INTO orders VALUES (1, 10), (1, 30), (2, NULL);")
        .unwrap();
    drop(database);
    let before = std::fs::metadata(&path).unwrap().len();
    let mut database = Database::open(&path).unwrap();
    let sql = "SELECT u.name, COUNT(*), SUM(o.amount)
        FROM users u JOIN orders o ON u.id = o.user_id
        GROUP BY u.name
        HAVING COUNT(*) >= 1;";
    let expected = vec![
        vec![text("Ada"), integer(2), integer(40)],
        vec![text("Bea"), integer(1), Value::Null],
    ];
    assert_eq!(rows(&mut database, sql), expected);
    let _ = query(&mut database, &format!("EXPLAIN {sql}"));
    drop(database);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, sql), expected);
}

#[test]
fn explain_names_having_and_the_join_without_executing() {
    let mut database = shop();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, COUNT(*), SUM(o.amount)
         FROM users u JOIN orders o ON u.id = o.user_id
         WHERE o.paid = TRUE
         GROUP BY u.name
         HAVING COUNT(*) >= 2;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "groups".to_string(),
            "aggregates".to_string(),
            "having".to_string(),
            "join".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "build".to_string(),
            "index".to_string(),
            "predicate".to_string(),
            "residual".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(explained.rows[0][0], text("HashAggregate"));
    assert_eq!(explained.rows[0][1], text("u.name"));
    assert_eq!(explained.rows[0][2], text("COUNT(*), SUM(o.amount)"));
    assert_eq!(explained.rows[0][3], text("COUNT(*) >= 2"));
    assert_eq!(explained.rows[0][4], text("HashJoin"));
    assert_eq!(explained.rows[0][5], text("u (users)"));
    assert_eq!(explained.rows[0][6], text("o (orders)"));
    assert_eq!(explained.rows[0][7], text("u (users)"));
    assert_eq!(explained.rows[0][8], Value::Null);
    assert_eq!(explained.rows[0][9], text("u.id = o.user_id"));
    assert_eq!(explained.rows[0][10], Value::Null);
    assert_eq!(explained.rows[0][11], text("o.paid = TRUE"));

    let plain = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        plain.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "build".to_string(),
            "predicate".to_string(),
            "residual".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(plain.rows[0][0], text("HashJoin"));

    let nested = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id OR u.name = 'Ada';",
    );
    assert_eq!(
        nested.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "condition".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(nested.rows[0][0], text("NestedLoopJoin"));

    let scalar = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM users HAVING COUNT(*) > 1;",
    );
    assert_eq!(scalar.rows[0][0], text("Aggregate"));
    assert_eq!(scalar.rows[0][7], text("COUNT(*) > 1"));

    let (_dir, path) = temporary("join-agg-explain");
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
        sql.push_str(&format!("(1, {id})"));
    }
    database.execute(&sql).unwrap();
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
        .unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, COUNT(*), SUM(o.amount)
         FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name;",
    );
    assert_eq!(explained.rows[0][0], text("HashAggregate"));
    assert_eq!(explained.rows[0][4], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][8], text("idx_orders_user"));
    assert_eq!(explained.rows[0][3], Value::Null);
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name;"
        ),
        vec![vec![text("Ada"), integer(70), integer((0..70).sum())]]
    );
}

#[test]
fn sum_overflow_after_a_join_is_an_error_before_any_row() {
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
        .execute("INSERT INTO orders VALUES (1, 1), (2, 9223372036854775807), (2, 1);")
        .unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name, SUM(o.amount)
         FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name;",
    );
    assert_eq!(explained.rows[0][2], text("SUM(o.amount)"));
    assert_eq!(
        failure(
            &mut database,
            "SELECT u.name, SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             LIMIT 1;"
        ),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(3)]]
    );
}

#[test]
fn having_without_an_aggregate_filters_grouped_columns() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES ('eng', 1), ('sales', 2), ('eng', 3), (NULL, 4);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING department = 'eng';"
        ),
        vec![vec![text("eng")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING department IS NULL;"
        ),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING department = department;"
        ),
        vec![vec![text("eng")], vec![text("sales")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING TRUE;"
        ),
        vec![vec![text("eng")], vec![text("sales")], vec![Value::Null]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING FALSE;"
        )
        .is_empty()
    );
    assert!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department HAVING NULL;"
        )
        .is_empty()
    );
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept HAVING u.dept = 'eng';"
        ),
        vec![vec![text("eng")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept HAVING u.dept IS NULL;"
        ),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept HAVING NOT (u.dept IS NULL);"
        ),
        vec![
            vec![text("eng"), integer(4)],
            vec![text("sales"), integer(1)]
        ]
    );
}

#[test]
fn having_combines_several_aggregates_with_three_valued_logic() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING COUNT(*) >= 2 AND SUM(o.amount) > 30 AND COUNT(o.amount) >= 2;"
        ),
        vec![vec![text("Ada")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING NOT (COUNT(*) >= 2) AND SUM(o.amount) IS NOT NULL;"
        ),
        vec![vec![text("Bea")], vec![text("Dee")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name HAVING SUM(o.amount) IS NULL;"
        ),
        vec![vec![text("Cy")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING COUNT(*) >= 1 AND SUM(o.amount) > 10 OR COUNT(*) >= 3;"
        ),
        vec![vec![text("Ada")], vec![text("Dee")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.name
             HAVING SUM(o.amount) > 10 AND SUM(o.amount) < 50;"
        ),
        vec![vec![text("Ada"), integer(40)]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id
         GROUP BY u.name
         HAVING SUM(o.amount) > 10 AND SUM(o.amount) < 50
         ORDER BY SUM(o.amount), COUNT(*);",
    );
    assert_eq!(explained.rows[0][2], text("SUM(o.amount), COUNT(*)"));
    assert!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users HAVING COUNT(*) = 5 LIMIT 0;"
        )
        .is_empty()
    );

    let mut database = Database::new();
    database
        .execute("CREATE TABLE bits (flag BOOLEAN, n INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO bits VALUES (TRUE, 1), (TRUE, 2), (FALSE, 4), (NULL, 8), (FALSE, NULL);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag, COUNT(*), SUM(n) FROM bits GROUP BY flag HAVING flag;"
        ),
        vec![vec![Value::Boolean(true), integer(2), integer(3)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag, COUNT(*), SUM(n) FROM bits GROUP BY flag HAVING NOT flag;"
        ),
        vec![vec![Value::Boolean(false), integer(2), integer(4)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag, COUNT(*) FROM bits GROUP BY flag HAVING flag IS NULL;"
        ),
        vec![vec![Value::Null, integer(1)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag FROM bits GROUP BY flag HAVING flag = flag;"
        ),
        vec![vec![Value::Boolean(true)], vec![Value::Boolean(false)]]
    );
}

#[test]
fn explain_and_select_report_the_same_binding_errors() {
    let mut database = shop();
    let queries = [
        "SELECT name, COUNT(*) FROM users;",
        "SELECT COUNT(*) FROM users HAVING COUNT(*) > 'x';",
        "SELECT COUNT(*) FROM users HAVING SUM(COUNT(*)) > 1;",
        "SELECT COUNT(*) FROM users HAVING lower(id) = 'a';",
        "SELECT dept FROM users GROUP BY dept HAVING dept;",
        "SELECT id, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY id;",
        "SELECT COUNT(*) FROM users HAVING COUNT(*) > 'x' LIMIT 0;",
    ];
    for sql in queries {
        assert_eq!(
            failure(&mut database, sql),
            failure(&mut database, &format!("EXPLAIN {sql}")),
            "{sql}"
        );
    }
    database
        .execute("CREATE TABLE empty (id INT, name TEXT);")
        .unwrap();
    let queries = [
        "SELECT COUNT(*) FROM empty HAVING COUNT(*) > 'x' LIMIT 0;",
        "SELECT name FROM empty GROUP BY name HAVING name LIMIT 0;",
        "SELECT name, COUNT(*) FROM empty LIMIT 0;",
    ];
    for sql in queries {
        assert_eq!(
            failure(&mut database, sql),
            failure(&mut database, &format!("EXPLAIN {sql}")),
            "{sql}"
        );
    }
}

#[test]
fn a_later_overflow_is_not_hidden_by_having_or_limit() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO employees VALUES
                ('eng', 10),
                ('eng', 20),
                ('sales', 9223372036854775807),
                ('sales', 1);",
        )
        .unwrap();
    let hidden = [
        "SELECT department, SUM(salary) FROM employees GROUP BY department HAVING department = 'eng' LIMIT 1;",
        "SELECT department, SUM(salary) FROM employees GROUP BY department HAVING SUM(salary) < 0 LIMIT 0;",
    ];
    for sql in hidden {
        assert_eq!(failure(&mut database, sql), "SUM overflows INTEGER");
    }
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM employees;"),
        vec![vec![integer(4)]]
    );

    let (_dir, path) = temporary("having-overflow");
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
        .execute("INSERT INTO orders VALUES (1, 10), (1, 5), (2, 9223372036854775807), (2, 1);")
        .unwrap();
    drop(database);
    let mut database = Database::open(&path).unwrap();
    let before = std::fs::metadata(&path).unwrap().len();
    let sql = "SELECT u.name, SUM(o.amount)
        FROM users u JOIN orders o ON u.id = o.user_id
        GROUP BY u.name
        HAVING u.name = 'Ada'
        LIMIT 0;";
    let _ = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(failure(&mut database, sql), "SUM overflows INTEGER");
    drop(database);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        vec![vec![integer(4)]]
    );
}

#[test]
fn randomized_join_aggregates_match_a_reference_model() {
    check_reference(&[], &[]);
    let null_users = vec![User {
        id: None,
        name: "n".to_string(),
        dept: None,
    }];
    let null_orders = vec![Order {
        user_id: None,
        amount: None,
        paid: None,
    }];
    check_reference(&null_users, &null_orders);
    for seed in [0x0a99_u64, 0x51c0, 0xc0ff, 0x5eed, 0xa11c, 0x11ab] {
        let (users, orders) = generate(seed);
        check_reference(&users, &orders);
    }
}

struct User {
    id: Option<i64>,
    name: String,
    dept: Option<String>,
}

struct Order {
    user_id: Option<i64>,
    amount: Option<i64>,
    paid: Option<bool>,
}

struct Spec {
    paid_only: bool,
    having_min: Option<i64>,
    order: bool,
    limit: Option<u64>,
}

fn generate(seed: u64) -> (Vec<User>, Vec<Order>) {
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        state
    };
    let n_users = 8 + (next() % 12) as usize;
    let mut users: Vec<User> = Vec::with_capacity(n_users);
    for index in 0..n_users {
        let mut id = if next() % 7 == 0 {
            None
        } else {
            Some(i64::try_from(index).unwrap())
        };
        if index > 0 && next() % 5 == 0 {
            id = users[0].id;
        }
        let dept = match next() % 5 {
            0 => None,
            1 => Some("eng".to_string()),
            2 => Some("sales".to_string()),
            _ => Some("ops".to_string()),
        };
        users.push(User {
            id,
            name: format!("n{index}"),
            dept,
        });
    }
    let n_orders = 10 + (next() % 30) as usize;
    let mut orders = Vec::with_capacity(n_orders);
    for index in 0..n_orders {
        let user_id = match next() % 6 {
            0 => None,
            1 => Some(10_000 + i64::try_from(index).unwrap()),
            _ => users[(next() as usize) % users.len()].id,
        };
        let amount = if next() % 3 == 0 {
            None
        } else {
            Some(i64::try_from(next() % 50).unwrap())
        };
        let paid = match next() % 4 {
            0 => None,
            1 => Some(false),
            _ => Some(true),
        };
        orders.push(Order {
            user_id,
            amount,
            paid,
        });
    }
    (users, orders)
}

fn check_reference(users: &[User], orders: &[Order]) {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, dept TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
        .unwrap();
    if !users.is_empty() {
        let mut sql = String::from("INSERT INTO users VALUES ");
        for (index, user) in users.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, {}, {})",
                sql_int(user.id),
                sql_text(&Some(user.name.clone())),
                sql_text(&user.dept)
            ));
        }
        database.execute(&sql).unwrap();
    }
    if !orders.is_empty() {
        let mut sql = String::from("INSERT INTO orders VALUES ");
        for (index, order) in orders.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            sql.push_str(&format!(
                "({}, {}, {})",
                sql_int(order.user_id),
                sql_int(order.amount),
                sql_bool(order.paid)
            ));
        }
        database.execute(&sql).unwrap();
    }
    let specs = [
        Spec {
            paid_only: false,
            having_min: None,
            order: false,
            limit: None,
        },
        Spec {
            paid_only: true,
            having_min: Some(2),
            order: true,
            limit: Some(5),
        },
        Spec {
            paid_only: false,
            having_min: Some(1),
            order: true,
            limit: None,
        },
        Spec {
            paid_only: true,
            having_min: None,
            order: false,
            limit: Some(3),
        },
    ];
    for spec in &specs {
        let sql = spec_sql(spec);
        assert_eq!(
            rows(&mut database, &sql),
            reference(users, orders, spec),
            "{sql}"
        );
    }
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.dept, u.name, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.dept, u.name;"
        ),
        reference_pairs(users, orders)
    );
}

fn spec_sql(spec: &Spec) -> String {
    let selection = if spec.paid_only {
        " WHERE o.paid = TRUE"
    } else {
        ""
    };
    let having = match spec.having_min {
        Some(min) => format!(" HAVING COUNT(*) >= {min}"),
        None => String::new(),
    };
    let order = if spec.order {
        " ORDER BY SUM(o.amount) DESC, u.dept ASC"
    } else {
        ""
    };
    let limit = match spec.limit {
        Some(limit) => format!(" LIMIT {limit}"),
        None => String::new(),
    };
    format!(
        "SELECT u.dept, COUNT(*), COUNT(o.amount), SUM(o.amount)
         FROM users u JOIN orders o ON u.id = o.user_id
         {selection} GROUP BY u.dept{having}{order}{limit};"
    )
}

struct RefGroup {
    dept: Option<String>,
    count: i64,
    count_amount: i64,
    sum: Option<i64>,
}

fn reference(users: &[User], orders: &[Order], spec: &Spec) -> Vec<Vec<Value>> {
    let mut groups: Vec<RefGroup> = Vec::new();
    for user in users {
        for order in orders {
            if !same_int(user.id, order.user_id) {
                continue;
            }
            if spec.paid_only && order.paid != Some(true) {
                continue;
            }
            if let Some(group) = groups.iter_mut().find(|group| group.dept == user.dept) {
                absorb(group, order.amount);
            } else {
                let mut group = RefGroup {
                    dept: user.dept.clone(),
                    count: 0,
                    count_amount: 0,
                    sum: None,
                };
                absorb(&mut group, order.amount);
                groups.push(group);
            }
        }
    }
    if let Some(min) = spec.having_min {
        groups.retain(|group| group.count >= min);
    }
    if spec.order {
        groups.sort_by(|left, right| {
            cmp_sum_desc(&left.sum, &right.sum).then_with(|| cmp_text_asc(&left.dept, &right.dept))
        });
    }
    if let Some(limit) = spec.limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if groups.len() > limit {
            groups.truncate(limit);
        }
    }
    groups.into_iter().map(project_group).collect()
}

fn reference_pairs(users: &[User], orders: &[Order]) -> Vec<Vec<Value>> {
    let mut groups: Vec<(Option<String>, String, i64, Option<i64>)> = Vec::new();
    for user in users {
        for order in orders {
            if !same_int(user.id, order.user_id) {
                continue;
            }
            if let Some(group) = groups
                .iter_mut()
                .find(|group| group.0 == user.dept && group.1 == user.name)
            {
                group.2 += 1;
                if let Some(amount) = order.amount {
                    group.3 = Some(group.3.unwrap_or(0).checked_add(amount).unwrap());
                }
            } else {
                groups.push((user.dept.clone(), user.name.clone(), 1, order.amount));
            }
        }
    }
    groups
        .into_iter()
        .map(|(dept, name, count, sum)| {
            vec![opt_text(&dept), text(&name), integer(count), opt_int(sum)]
        })
        .collect()
}

fn absorb(group: &mut RefGroup, amount: Option<i64>) {
    group.count += 1;
    if let Some(amount) = amount {
        group.count_amount += 1;
        group.sum = Some(group.sum.unwrap_or(0).checked_add(amount).unwrap());
    }
}

fn project_group(group: RefGroup) -> Vec<Value> {
    vec![
        opt_text(&group.dept),
        integer(group.count),
        integer(group.count_amount),
        opt_int(group.sum),
    ]
}

fn same_int(left: Option<i64>, right: Option<i64>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left == right)
}

fn cmp_sum_desc(left: &Option<i64>, right: &Option<i64>) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => right.cmp(left),
    }
}

fn cmp_text_asc(left: &Option<String>, right: &Option<String>) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    }
}

fn opt_text(value: &Option<String>) -> Value {
    match value {
        Some(value) => text(value),
        None => Value::Null,
    }
}

fn opt_int(value: Option<i64>) -> Value {
    match value {
        Some(value) => integer(value),
        None => Value::Null,
    }
}

fn sql_int(value: Option<i64>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "NULL".to_string(),
    }
}

fn sql_text(value: &Option<String>) -> String {
    match value {
        Some(value) => format!("'{}'", value.replace('\'', "''")),
        None => "NULL".to_string(),
    }
}

fn sql_bool(value: Option<bool>) -> String {
    match value {
        Some(true) => "TRUE".to_string(),
        Some(false) => "FALSE".to_string(),
        None => "NULL".to_string(),
    }
}

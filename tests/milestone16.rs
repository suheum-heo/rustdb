//! `SELECT DISTINCT` for one non-aggregate `INNER JOIN`.
//!
//! Randomized comparisons use the seeds `0x1616`, `0x0a16`, `0xb116`, and
//! `0x5e16`. The reference joins with its own nested loop, projects the output
//! tuple, and keeps the first copy by `==`. It does not call the engine hash.

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

fn boolean(value: bool) -> Value {
    Value::Boolean(value)
}

fn shop() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN, label TEXT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO users VALUES
                (1, 'Ada', TRUE),
                (2, 'Ada', FALSE),
                (3, 'Bea', TRUE),
                (4, NULL, TRUE),
                (5, 'Cy', NULL);",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO orders VALUES
                (1, 10, TRUE, 'alpha'),
                (1, 10, TRUE, 'alpha'),
                (1, 30, FALSE, 'Alpha'),
                (2, 10, TRUE, 'beta'),
                (3, NULL, TRUE, 'gamma'),
                (4, 10, FALSE, 'delta'),
                (NULL, 10, TRUE, 'none'),
                (5, 5, TRUE, 'cy');",
        )
        .unwrap();
    database
}

const DISTINCT_NAMES: &str = "SELECT DISTINCT u.name, o.amount
    FROM users u
    INNER JOIN orders o ON u.id = o.user_id;";

#[test]
fn distinct_join_keeps_the_first_projected_tuple() {
    let mut database = shop();
    let result = query(&mut database, DISTINCT_NAMES);
    assert_eq!(
        result.columns,
        vec!["name".to_string(), "amount".to_string()]
    );
    assert_eq!(
        result.rows,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(30)],
            vec![text("Bea"), Value::Null],
            vec![Value::Null, integer(10)],
            vec![text("Cy"), integer(5)],
        ]
    );
    let plain = rows(
        &mut database,
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(plain.len(), 7);
}

#[test]
fn star_and_qualified_star_keep_declared_names() {
    let mut database = shop();
    let star = query(
        &mut database,
        "SELECT DISTINCT * FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        star.columns,
        vec![
            "id".to_string(),
            "name".to_string(),
            "active".to_string(),
            "user_id".to_string(),
            "amount".to_string(),
            "paid".to_string(),
            "label".to_string()
        ]
    );
    assert_eq!(star.rows.len(), 6);
    let qualified = query(
        &mut database,
        "SELECT DISTINCT u.*, o.*
         FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(qualified.columns, star.columns);
    assert_eq!(qualified.rows, star.rows);
    let reversed = query(
        &mut database,
        "SELECT DISTINCT o.*, u.*
         FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        reversed.columns,
        vec![
            "user_id".to_string(),
            "amount".to_string(),
            "paid".to_string(),
            "label".to_string(),
            "id".to_string(),
            "name".to_string(),
            "active".to_string()
        ]
    );
    let one = query(
        &mut database,
        "SELECT DISTINCT u.* FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        one.columns,
        vec!["id".to_string(), "name".to_string(), "active".to_string()]
    );
    assert_eq!(one.rows.len(), 5);
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.*, o.amount FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        "mixing * with other select items is not supported"
    );
}

#[test]
fn nulls_types_and_case_stay_distinct() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.label, o.paid, u.active
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.id = 1;"
        ),
        vec![
            vec![text("alpha"), boolean(true), boolean(true)],
            vec![text("Alpha"), boolean(false), boolean(true)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             WHERE o.amount IS NULL;"
        ),
        vec![vec![text("Bea")]]
    );
}

#[test]
fn where_residual_order_and_limit_run_after_dedup() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name AS who, o.amount
             FROM users u
             JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
             WHERE o.amount > 5
             ORDER BY who ASC, amount DESC
             LIMIT 10;"
        ),
        vec![vec![text("Ada"), integer(10)]]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY who;"
        ),
        "column 'who' does not exist"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name AS who FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY who ASC
             LIMIT 1;"
        ),
        vec![vec![text("Ada")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name AS who FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY who ASC
             LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn order_by_uses_output_slots_and_rejects_hidden_columns() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.amount AS name, u.name AS amount
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.id = 1
             ORDER BY amount ASC;"
        ),
        vec![
            vec![integer(10), text("Ada")],
            vec![integer(30), text("Ada")],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.amount AS name, u.name AS amount
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.id = 1
             ORDER BY u.name ASC;"
        ),
        vec![
            vec![integer(10), text("Ada")],
            vec![integer(30), text("Ada")],
        ]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY o.amount;"
        ),
        "column 'o.amount' must appear in the SELECT DISTINCT list"
    );
    database
        .execute("CREATE TABLE tags (name TEXT, id INT);")
        .unwrap();
    database
        .execute("INSERT INTO tags VALUES ('Ada', 1), ('Ada', 1), ('Bea', 3);")
        .unwrap();
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT name FROM users u JOIN tags t ON u.id = t.id;"
        ),
        "column 'name' is ambiguous"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name, t.name FROM users u
             JOIN tags t ON u.id = t.id
             ORDER BY name;"
        ),
        "column 'name' is ambiguous"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name, t.name AS tag
             FROM users u JOIN tags t ON u.id = t.id
             ORDER BY u.name ASC, tag ASC;"
        ),
        vec![
            vec![text("Ada"), text("Ada")],
            vec![text("Bea"), text("Bea")],
        ]
    );
}

#[test]
fn aliases_self_join_and_plain_join_order_stay_distinct() {
    let mut database = shop();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT a.name, b.name AS other
             FROM users a
             JOIN users b ON a.id = b.id
             ORDER BY a.name ASC;"
        ),
        vec![
            vec![text("Ada"), text("Ada")],
            vec![text("Bea"), text("Bea")],
            vec![text("Cy"), text("Cy")],
            vec![Value::Null, Value::Null],
        ]
    );
    let plain = rows(
        &mut database,
        "SELECT u.name FROM users u
         JOIN orders o ON u.id = o.user_id
         ORDER BY o.amount ASC
         LIMIT 1;",
    );
    assert_eq!(plain, vec![vec![text("Cy")]]);
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             WHERE u.id > 'x'
             LIMIT 0;"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    let mut empty = Database::new();
    empty
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    empty
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    assert_eq!(
        failure(
            &mut empty,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             WHERE o.amount > 'x'
             LIMIT 0;"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    assert_eq!(
        rows(
            &mut empty,
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        Vec::<Vec<Value>>::new()
    );
}

#[test]
fn explain_names_the_join_and_does_not_write() {
    let (dir, path) = temporary("distinct-join-explain");
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
        .execute("INSERT INTO orders VALUES (1, 10), (1, 10), (2, 20);")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT DISTINCT u.name AS who, o.amount
         FROM users u
         JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 10
         ORDER BY who ASC
         LIMIT 5;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "outer".to_string(),
            "inner".to_string(),
            "build".to_string(),
            "index".to_string(),
            "predicate".to_string(),
            "residual".to_string(),
            "filter".to_string(),
            "distinct".to_string(),
            "projection".to_string(),
            "order_by".to_string(),
            "limit".to_string(),
        ]
    );
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(explained.rows[0][1], text("u (users)"));
    assert_eq!(explained.rows[0][2], text("o (orders)"));
    assert_eq!(explained.rows[0][8], text("HashDistinct"));
    assert_eq!(explained.rows[0][9], text("u.name AS who, o.amount"));
    assert_eq!(explained.rows[0][10], text("who ASC"));
    assert_eq!(explained.rows[0][11], integer(5));
    let plain = query(
        &mut database,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(plain.columns[0], "access".to_string());
    assert!(!plain.columns.iter().any(|column| column == "distinct"));
    let single = query(&mut database, "EXPLAIN SELECT DISTINCT name FROM users;");
    assert_eq!(single.columns[0], "access".to_string());
    assert_eq!(single.rows[0][3], text("HashDistinct"));
    drop(database);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let _ = dir;
}

#[test]
fn an_index_restart_matches_the_hash_join() {
    let (dir, path) = temporary("distinct-join-index");
    let sql = "SELECT DISTINCT u.name, o.amount
        FROM users u JOIN orders o ON u.id = o.user_id
        ORDER BY amount ASC;";
    let hash_rows = {
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
        database
            .execute("INSERT INTO orders VALUES (1, 10), (1, 10), (1, 20);")
            .unwrap();
        rows(&mut database, sql)
    };
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_orders_user ON orders (user_id);")
        .unwrap();
    let mut extra = String::from("INSERT INTO orders VALUES (1, 20), (1, NULL)");
    for _ in 0..70 {
        extra.push_str(", (1, 10)");
    }
    extra.push(';');
    database.execute(&extra).unwrap();
    let explained = query(&mut database, &format!("EXPLAIN {sql}"));
    assert_eq!(explained.rows[0][0], text("IndexNestedLoopJoin"));
    assert_eq!(explained.rows[0][4], text("idx_orders_user"));
    let indexed = rows(&mut database, sql);
    assert_eq!(
        indexed,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(20)],
            vec![text("Ada"), Value::Null],
        ]
    );
    database.execute("DROP INDEX idx_orders_user;").unwrap();
    let after_drop = rows(&mut database, sql);
    assert_eq!(after_drop, indexed);
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, sql), indexed);
    assert_eq!(
        hash_rows,
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(20)]
        ]
    );
    let _ = dir;
}

#[test]
fn both_hash_build_sides_and_a_nested_loop_match_the_reference() {
    let mut database = shop();
    let equality = "SELECT DISTINCT u.name, o.amount
        FROM users u JOIN orders o ON u.id = o.user_id;";
    let explained = query(&mut database, &format!("EXPLAIN {equality}"));
    assert_eq!(explained.rows[0][0], text("HashJoin"));
    assert_eq!(
        rows(&mut database, equality),
        reference_names(&shop_users(), &shop_orders(), true)
    );

    database
        .execute("INSERT INTO users VALUES (6, 'Dee', TRUE), (7, 'Eve', FALSE), (8, 'Fay', TRUE), (9, 'Gia', TRUE);")
        .unwrap();
    let build_inner = query(
        &mut database,
        "EXPLAIN SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(build_inner.rows[0][0], text("HashJoin"));
    assert_eq!(build_inner.rows[0][3], text("o (orders)"));

    let nested = "SELECT DISTINCT u.name, o.amount
        FROM users u
        JOIN orders o ON u.id = o.user_id AND o.paid = TRUE;";
    let nested_plan = query(&mut database, &format!("EXPLAIN {nested}"));
    assert_eq!(nested_plan.rows[0][0], text("HashJoin"));
    assert!(matches!(nested_plan.rows[0][6], Value::Text(_)));
    assert_eq!(
        rows(&mut database, nested),
        reference_names(&extended_users(), &shop_orders(), false)
    );

    let scan = "SELECT DISTINCT u.name
        FROM users u JOIN orders o ON u.name < o.label;";
    assert_eq!(
        query(&mut database, &format!("EXPLAIN {scan}")).rows[0][0],
        text("NestedLoopJoin")
    );
}

#[test]
fn grouped_distinct_and_single_table_distinct_stay_unchanged() {
    let mut database = shop();
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             GROUP BY u.name;"
        ),
        "DISTINCT is not supported with GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             HAVING u.name = 'Ada';"
        ),
        "DISTINCT is not supported with HAVING"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;"
        ),
        "DISTINCT is not supported with aggregate functions"
    );
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT name FROM users;"),
        vec![
            vec![text("Ada")],
            vec![text("Bea")],
            vec![Value::Null],
            vec![text("Cy")]
        ]
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(DISTINCT name) FROM users;"),
        vec![vec![integer(3)]]
    );
}

#[test]
fn projection_order_keeps_every_selected_cell() {
    let mut database = shop();
    let orders = query(
        &mut database,
        "SELECT DISTINCT o.* FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(
        orders.columns,
        vec![
            "user_id".to_string(),
            "amount".to_string(),
            "paid".to_string(),
            "label".to_string()
        ]
    );
    assert_eq!(
        orders.rows,
        vec![
            vec![integer(1), integer(10), boolean(true), text("alpha")],
            vec![integer(1), integer(30), boolean(false), text("Alpha")],
            vec![integer(2), integer(10), boolean(true), text("beta")],
            vec![integer(3), Value::Null, boolean(true), text("gamma")],
            vec![integer(4), integer(10), boolean(false), text("delta")],
            vec![integer(5), integer(5), boolean(true), text("cy")],
        ]
    );
    let people = query(
        &mut database,
        "SELECT DISTINCT u.* FROM users u JOIN orders o ON u.id = o.user_id;",
    );
    assert_eq!(people.rows.len(), 5);
    assert_eq!(people.rows[0][0], integer(1));
    let repeated = query(
        &mut database,
        "SELECT DISTINCT u.name, u.name, o.amount
         FROM users u JOIN orders o ON u.id = o.user_id
         WHERE u.id = 1;",
    );
    assert_eq!(
        repeated.columns,
        vec!["name".to_string(), "name".to_string(), "amount".to_string()]
    );
    assert_eq!(
        repeated.rows,
        vec![
            vec![text("Ada"), text("Ada"), integer(10)],
            vec![text("Ada"), text("Ada"), integer(30)],
        ]
    );
    let self_join = query(
        &mut database,
        "SELECT DISTINCT a.name, b.name, a.name
         FROM users a JOIN users b ON a.id = b.id;",
    );
    assert_eq!(
        self_join.rows,
        vec![
            vec![text("Ada"), text("Ada"), text("Ada")],
            vec![text("Bea"), text("Bea"), text("Bea")],
            vec![Value::Null, Value::Null, Value::Null],
            vec![text("Cy"), text("Cy"), text("Cy")],
        ]
    );
}

#[test]
fn order_by_uses_projected_slots_and_null_placement() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE users (id INT, name TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE orders (user_id INT, amount INT);")
        .unwrap();
    database
        .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, NULL);")
        .unwrap();
    database
        .execute("INSERT INTO orders VALUES (1, 10), (1, 5), (2, NULL), (3, 5);")
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name, o.amount
             FROM users u JOIN orders o ON u.id = o.user_id
             ORDER BY name ASC, amount DESC;",
        ),
        vec![
            vec![text("Ada"), integer(10)],
            vec![text("Ada"), integer(5)],
            vec![text("Bea"), Value::Null],
            vec![Value::Null, integer(5)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT u.name, o.amount
             FROM users u JOIN orders o ON u.id = o.user_id
             ORDER BY name ASC NULLS FIRST, amount ASC NULLS LAST;",
        ),
        vec![
            vec![Value::Null, integer(5)],
            vec![text("Ada"), integer(5)],
            vec![text("Ada"), integer(10)],
            vec![text("Bea"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.amount AS name, u.name AS label
             FROM users u JOIN orders o ON u.id = o.user_id
             ORDER BY label ASC, name DESC;",
        ),
        vec![
            vec![integer(10), text("Ada")],
            vec![integer(5), text("Ada")],
            vec![Value::Null, text("Bea")],
            vec![integer(5), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.amount AS name, u.name AS label
             FROM users u JOIN orders o ON u.id = o.user_id
             WHERE u.id = 1
             ORDER BY o.amount ASC;",
        ),
        vec![
            vec![integer(5), text("Ada")],
            vec![integer(10), text("Ada")],
        ]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY o.amount
             LIMIT 0;",
        ),
        "column 'o.amount' must appear in the SELECT DISTINCT list"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT u.name, u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY name
             LIMIT 0;",
        ),
        "column 'name' is ambiguous"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT u.name FROM users u
             JOIN orders o ON u.id = o.user_id
             ORDER BY o.amount ASC NULLS FIRST
             LIMIT 1;",
        ),
        vec![vec![text("Bea")]]
    );
    let explained = query(
        &mut database,
        "EXPLAIN SELECT DISTINCT u.name AS who, o.amount
         FROM users u JOIN orders o ON u.id = o.user_id
         WHERE o.amount > 0
         ORDER BY who DESC NULLS LAST, amount ASC
         LIMIT 2;",
    );
    assert_eq!(explained.rows[0][8], text("HashDistinct"));
    assert_eq!(explained.rows[0][9], text("u.name AS who, o.amount"));
    assert_eq!(explained.rows[0][7], text("o.amount > 0"));
    assert_eq!(
        explained.rows[0][10],
        text("who DESC NULLS LAST, amount ASC")
    );
    assert_eq!(explained.rows[0][11], integer(2));
}

#[test]
fn quoted_names_and_empty_inputs_keep_select_list_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE \"People\" (\"Id\" INT, \"Name\" TEXT);")
        .unwrap();
    database
        .execute("CREATE TABLE \"Orders\" (\"User\" INT, \"Amount\" INT, \"Name\" TEXT);")
        .unwrap();
    assert_eq!(
        query(
            &mut database,
            "SELECT DISTINCT p.*, o.*
             FROM \"People\" p JOIN \"Orders\" o ON p.\"Id\" = o.\"User\";",
        )
        .columns,
        vec![
            "Id".to_string(),
            "Name".to_string(),
            "User".to_string(),
            "Amount".to_string(),
            "Name".to_string()
        ]
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT p.\"Name\", o.\"Name\"
             FROM \"People\" p JOIN \"Orders\" o ON p.\"Id\" = o.\"User\"
             ORDER BY \"Name\"
             LIMIT 0;",
        ),
        "column 'Name' is ambiguous"
    );
    database
        .execute("INSERT INTO \"People\" VALUES (1, 'Ada'), (2, 'Ada'), (3, NULL);")
        .unwrap();
    database
        .execute(
            "INSERT INTO \"Orders\" VALUES
                (1, 10, 'Ada'),
                (1, 10, 'Ada'),
                (2, NULL, 'Bea'),
                (NULL, 10, 'Ada');",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT p.\"Name\", o.\"Amount\", p.\"Name\" AS \"Again\"
             FROM \"People\" p
             JOIN \"Orders\" o ON p.\"Id\" = o.\"User\"
             ORDER BY \"Again\" ASC NULLS LAST, \"Amount\" DESC NULLS LAST;",
        ),
        vec![
            vec![text("Ada"), integer(10), text("Ada")],
            vec![text("Ada"), Value::Null, text("Ada")],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT p.*, o.*
             FROM \"People\" p JOIN \"Orders\" o ON p.\"Id\" = o.\"User\";",
        ),
        vec![
            vec![
                integer(1),
                text("Ada"),
                integer(1),
                integer(10),
                text("Ada")
            ],
            vec![
                integer(2),
                text("Ada"),
                integer(2),
                Value::Null,
                text("Bea")
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT o.*, p.*
             FROM \"People\" p JOIN \"Orders\" o ON p.\"Id\" = o.\"User\";",
        ),
        vec![
            vec![
                integer(1),
                integer(10),
                text("Ada"),
                integer(1),
                text("Ada")
            ],
            vec![
                integer(2),
                Value::Null,
                text("Bea"),
                integer(2),
                text("Ada")
            ],
        ]
    );
}

#[test]
fn randomized_joins_match_a_linear_reference() {
    for seed in [0x1616_u64, 0x0a16, 0xb116, 0x5e16] {
        for count in [0_usize, 1, 12, 40] {
            let (users, orders) = generate(seed, count);
            let mut database = Database::new();
            database
                .execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN, label TEXT);")
                .unwrap();
            if !users.is_empty() {
                database.execute(&insert_users(&users)).unwrap();
            }
            if !orders.is_empty() {
                database.execute(&insert_orders(&orders)).unwrap();
            }
            let sql = "SELECT DISTINCT u.name, o.amount, o.paid
                FROM users u
                JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
                WHERE o.amount > 0
                ORDER BY name ASC, amount DESC
                LIMIT 5;";
            assert_eq!(
                rows(&mut database, sql),
                reference_ordered(&users, &orders),
                "seed {seed:#x} count {count}"
            );
        }
    }
}

struct Person {
    id: Option<i64>,
    name: Option<String>,
    active: Option<bool>,
}

struct Order {
    user_id: Option<i64>,
    amount: Option<i64>,
    paid: Option<bool>,
    label: String,
}

fn shop_users() -> Vec<Person> {
    vec![
        person(Some(1), Some("Ada"), Some(true)),
        person(Some(2), Some("Ada"), Some(false)),
        person(Some(3), Some("Bea"), Some(true)),
        person(Some(4), None, Some(true)),
        person(Some(5), Some("Cy"), None),
    ]
}

fn extended_users() -> Vec<Person> {
    let mut users = shop_users();
    users.extend([
        person(Some(6), Some("Dee"), Some(true)),
        person(Some(7), Some("Eve"), Some(false)),
        person(Some(8), Some("Fay"), Some(true)),
        person(Some(9), Some("Gia"), Some(true)),
    ]);
    users
}

fn shop_orders() -> Vec<Order> {
    vec![
        order(Some(1), Some(10), Some(true), "alpha"),
        order(Some(1), Some(10), Some(true), "alpha"),
        order(Some(1), Some(30), Some(false), "Alpha"),
        order(Some(2), Some(10), Some(true), "beta"),
        order(Some(3), None, Some(true), "gamma"),
        order(Some(4), Some(10), Some(false), "delta"),
        order(None, Some(10), Some(true), "none"),
        order(Some(5), Some(5), Some(true), "cy"),
    ]
}

fn person(id: Option<i64>, name: Option<&str>, active: Option<bool>) -> Person {
    Person {
        id,
        name: name.map(str::to_string),
        active,
    }
}

fn order(user_id: Option<i64>, amount: Option<i64>, paid: Option<bool>, label: &str) -> Order {
    Order {
        user_id,
        amount,
        paid,
        label: label.to_string(),
    }
}

fn reference_names(users: &[Person], orders: &[Order], every_paid: bool) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for user in users {
        for order in orders {
            if user.id.is_none() || user.id != order.user_id {
                continue;
            }
            if !every_paid && order.paid != Some(true) {
                continue;
            }
            let row = vec![
                optional_text(user.name.as_deref()),
                optional_integer(order.amount),
            ];
            if !kept.iter().any(|seen| seen == &row) {
                kept.push(row);
            }
        }
    }
    kept
}

fn reference_ordered(users: &[Person], orders: &[Order]) -> Vec<Vec<Value>> {
    let mut kept = Vec::new();
    for user in users {
        for order in orders {
            if user.id.is_none() || user.id != order.user_id || order.paid != Some(true) {
                continue;
            }
            if order.amount.is_none_or(|amount| amount <= 0) {
                continue;
            }
            let row = vec![
                optional_text(user.name.as_deref()),
                optional_integer(order.amount),
                optional_boolean(order.paid),
            ];
            if !kept.iter().any(|seen| seen == &row) {
                kept.push(row);
            }
        }
    }
    kept.sort_by(|left, right| {
        compare_cell(&left[0], &right[0], true).then(compare_cell(&left[1], &right[1], false))
    });
    kept.truncate(5);
    kept
}

fn compare_cell(left: &Value, right: &Value, ascending: bool) -> Ordering {
    let (left, right) = if ascending {
        (left, right)
    } else {
        (right, left)
    };
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

fn optional_text(value: Option<&str>) -> Value {
    value.map(text).unwrap_or(Value::Null)
}

fn optional_integer(value: Option<i64>) -> Value {
    value.map(integer).unwrap_or(Value::Null)
}

fn optional_boolean(value: Option<bool>) -> Value {
    value.map(boolean).unwrap_or(Value::Null)
}

fn generate(seed: u64, count: usize) -> (Vec<Person>, Vec<Order>) {
    let mut state = seed;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        state
    };
    let mut users = Vec::new();
    let mut orders = Vec::new();
    for index in 0..count {
        let id = i64::try_from(index).unwrap_or(i64::MAX);
        let name = if next().is_multiple_of(5) {
            None
        } else if next().is_multiple_of(3) {
            Some("Ada".to_string())
        } else {
            let mut label = ((index % 7) as u8).to_string();
            if next().is_multiple_of(4) {
                label.push_str(&"x".repeat(48));
            }
            Some(label)
        };
        users.push(Person {
            id: if next().is_multiple_of(11) {
                None
            } else {
                Some(id)
            },
            name,
            active: Some(next().is_multiple_of(2)),
        });
        let user_id = if next().is_multiple_of(7) {
            None
        } else {
            Some(i64::try_from(next() as usize % count.max(1)).unwrap_or(0))
        };
        orders.push(Order {
            user_id,
            amount: if next().is_multiple_of(4) {
                None
            } else {
                Some(i64::try_from(next() % 6).unwrap_or(0))
            },
            paid: Some(!next().is_multiple_of(3)),
            label: "row".to_string(),
        });
    }
    (users, orders)
}

fn insert_users(users: &[Person]) -> String {
    let mut sql = String::from("INSERT INTO users VALUES ");
    for (index, user) in users.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        push_optional_integer(&mut sql, user.id);
        sql.push(',');
        push_optional_text(&mut sql, user.name.as_deref());
        sql.push(',');
        push_optional_boolean(&mut sql, user.active);
        sql.push(')');
    }
    sql.push(';');
    sql
}

fn insert_orders(orders: &[Order]) -> String {
    let mut sql = String::from("INSERT INTO orders VALUES ");
    for (index, order) in orders.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('(');
        push_optional_integer(&mut sql, order.user_id);
        sql.push(',');
        push_optional_integer(&mut sql, order.amount);
        sql.push(',');
        push_optional_boolean(&mut sql, order.paid);
        sql.push_str(", '");
        sql.push_str(&order.label);
        sql.push_str("')");
    }
    sql.push(';');
    sql
}

fn push_optional_integer(sql: &mut String, value: Option<i64>) {
    match value {
        Some(value) => sql.push_str(&value.to_string()),
        None => sql.push_str("NULL"),
    }
}

fn push_optional_text(sql: &mut String, value: Option<&str>) {
    match value {
        Some(value) => {
            sql.push('\'');
            sql.push_str(value);
            sql.push('\'');
        }
        None => sql.push_str("NULL"),
    }
}

fn push_optional_boolean(sql: &mut String, value: Option<bool>) {
    match value {
        Some(true) => sql.push_str("TRUE"),
        Some(false) => sql.push_str("FALSE"),
        None => sql.push_str("NULL"),
    }
}

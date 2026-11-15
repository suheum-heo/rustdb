//! `SELECT DISTINCT` for one non-aggregate table.
//!
//! Randomized comparisons use the seeds `0x0d14`, `0x51d4`, `0xc014`,
//! `0x5e14`, `0xa114`, and `0x1114`. The reference keeps the first copy of
//! each projected tuple with a linear scan. It does not use the engine's
//! hash table.

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
                (NULL, 'No', 10000, FALSE, 6),
                ('eng', 'Zoe', 60000, TRUE, 1);",
        )
        .unwrap();
    database
}

#[test]
fn one_column_distinct_keeps_the_first_row() {
    let mut database = employees();
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT department FROM employees;"),
        vec![text("eng"), text("sales"), Value::Null]
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        rows(&mut database, "SELECT department FROM employees;").len(),
        7
    );
}

#[test]
fn several_columns_are_one_tuple() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department, name FROM employees;"
        ),
        vec![
            vec![text("eng"), text("Zoe")],
            vec![text("eng"), text("Bea")],
            vec![text("eng"), text("Cy")],
            vec![text("sales"), text("Amy")],
            vec![text("sales"), text("Dee")],
            vec![Value::Null, text("No")],
        ]
    );
}

#[test]
fn star_and_qualified_star_collapse_a_repeated_row() {
    let mut database = employees();
    let star = rows(&mut database, "SELECT DISTINCT * FROM employees;");
    assert_eq!(star.len(), 6);
    assert_eq!(star[0][1], text("Zoe"));
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT e.* FROM employees e;"),
        star
    );
    assert_eq!(
        query(&mut database, "SELECT DISTINCT * FROM employees;").columns,
        vec![
            "department".to_string(),
            "name".to_string(),
            "salary".to_string(),
            "active".to_string(),
            "employee_id".to_string(),
        ]
    );
}

#[test]
fn empty_all_null_and_mixed_nulls_compare_the_whole_row() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE vacant (department TEXT);")
        .unwrap();
    let empty = query(&mut database, "SELECT DISTINCT department FROM vacant;");
    assert_eq!(empty.columns, vec!["department".to_string()]);
    assert!(empty.rows.is_empty());

    database
        .execute("CREATE TABLE blanks (department TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO blanks VALUES (NULL), (NULL);")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT department FROM blanks;"),
        vec![vec![Value::Null]]
    );

    database
        .execute("CREATE TABLE pairs (left_id INT, right_id INT);")
        .unwrap();
    database
        .execute(
            "INSERT INTO pairs VALUES
                (NULL, NULL),
                (NULL, NULL),
                (NULL, 1),
                (1, NULL),
                (1, 1),
                (NULL, 1);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT left_id, right_id FROM pairs;"
        ),
        vec![
            vec![Value::Null, Value::Null],
            vec![Value::Null, integer(1)],
            vec![integer(1), Value::Null],
            vec![integer(1), integer(1)],
        ]
    );
}

#[test]
fn distinct_keeps_integer_text_and_boolean_apart() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE cells (n INT, label TEXT, active BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO cells VALUES
                (1, 'A', TRUE),
                (1, 'A', TRUE),
                (1, 'a', TRUE),
                (1, 'A', FALSE),
                (0, 'A', FALSE);",
        )
        .unwrap();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT n, label, active FROM cells;"
        ),
        vec![
            vec![integer(1), text("A"), boolean(true)],
            vec![integer(1), text("a"), boolean(true)],
            vec![integer(1), text("A"), boolean(false)],
            vec![integer(0), text("A"), boolean(false)],
        ]
    );
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT label FROM cells;"),
        vec![vec![text("A")], vec![text("a")]]
    );
}

#[test]
fn duplicate_heavy_and_unique_inputs_keep_first_seen_order() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (n INT, label TEXT);")
        .unwrap();
    let mut sql = String::from("INSERT INTO samples VALUES ");
    for index in 0..80 {
        if index != 0 {
            sql.push(',');
        }
        let label = (b'a' + (index % 4) as u8) as char;
        sql.push_str(&format!("({}, '{label}')", index % 4));
    }
    database.execute(&sql).unwrap();
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT n, label FROM samples;"),
        vec![
            vec![integer(0), text("a")],
            vec![integer(1), text("b")],
            vec![integer(2), text("c")],
            vec![integer(3), text("d")],
        ]
    );

    database.execute("DELETE FROM samples;").unwrap();
    let mut sql = String::from("INSERT INTO samples VALUES ");
    for index in 0..200 {
        if index != 0 {
            sql.push(',');
        }
        sql.push_str(&format!("({index}, 'u')"));
    }
    database.execute(&sql).unwrap();
    let unique = rows(&mut database, "SELECT DISTINCT n FROM samples;");
    assert_eq!(unique.len(), 200);
    assert_eq!(unique[0], vec![integer(0)]);
    assert_eq!(unique[199], vec![integer(199)]);
}

#[test]
fn where_runs_before_deduplication() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department FROM employees WHERE active = TRUE;"
        ),
        vec![vec![text("eng")], vec![text("sales")]]
    );
}

#[test]
fn order_by_uses_selected_columns_aliases_and_null_order() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department AS dept FROM employees ORDER BY dept ASC;"
        ),
        vec![vec![text("eng")], vec![text("sales")], vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department AS dept FROM employees ORDER BY department DESC;"
        ),
        vec![vec![Value::Null], vec![text("sales")], vec![text("eng")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department, name FROM employees
             ORDER BY department ASC, name DESC;"
        ),
        vec![
            vec![text("eng"), text("Zoe")],
            vec![text("eng"), text("Cy")],
            vec![text("eng"), text("Bea")],
            vec![text("sales"), text("Dee")],
            vec![text("sales"), text("Amy")],
            vec![Value::Null, text("No")],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT name, department FROM employees ORDER BY department ASC;"
        ),
        vec![
            vec![text("Zoe"), text("eng")],
            vec![text("Bea"), text("eng")],
            vec![text("Cy"), text("eng")],
            vec![text("Amy"), text("sales")],
            vec![text("Dee"), text("sales")],
            vec![text("No"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT name AS department, department AS team
             FROM employees ORDER BY employees.department ASC;"
        ),
        vec![
            vec![text("Zoe"), text("eng")],
            vec![text("Bea"), text("eng")],
            vec![text("Cy"), text("eng")],
            vec![text("Amy"), text("sales")],
            vec![text("Dee"), text("sales")],
            vec![text("No"), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT name AS department, department AS team
             FROM employees ORDER BY department ASC;"
        ),
        vec![
            vec![text("Amy"), text("sales")],
            vec![text("Bea"), text("eng")],
            vec![text("Cy"), text("eng")],
            vec![text("Dee"), text("sales")],
            vec![text("No"), Value::Null],
            vec![text("Zoe"), text("eng")],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department AS \"Dept\" FROM employees ORDER BY \"Dept\" ASC;"
        ),
        vec![vec![text("eng")], vec![text("sales")], vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT * FROM employees ORDER BY department ASC, name ASC;"
        )
        .len(),
        6
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT e.* FROM employees e ORDER BY e.name ASC;"
        )
        .len(),
        6
    );
    let mut nulls = Database::new();
    nulls.execute("CREATE TABLE labels (name TEXT);").unwrap();
    nulls
        .execute("INSERT INTO labels VALUES ('b'), (NULL), ('a');")
        .unwrap();
    let ascending = "SELECT name FROM labels ORDER BY name ASC;";
    let distinct_ascending = "SELECT DISTINCT name FROM labels ORDER BY name ASC;";
    let explicit = "SELECT name FROM labels ORDER BY name DESC NULLS LAST;";
    let distinct_explicit = "SELECT DISTINCT name FROM labels ORDER BY name DESC NULLS LAST;";
    assert_eq!(
        rows(&mut nulls, distinct_ascending),
        rows(&mut nulls, ascending)
    );
    assert_eq!(
        rows(&mut nulls, distinct_explicit),
        rows(&mut nulls, explicit)
    );
}

#[test]
fn order_by_rejects_hidden_ambiguous_and_duplicate_names() {
    let mut database = employees();
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department FROM employees ORDER BY salary;"
        ),
        "column 'salary' must appear in the SELECT DISTINCT list"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department FROM employees ORDER BY employees.salary;"
        ),
        "column 'employees.salary' must appear in the SELECT DISTINCT list"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department FROM employees ORDER BY missing;"
        ),
        "column 'missing' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department AS \"Dept\" FROM employees ORDER BY dept;"
        ),
        "column 'dept' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department AS name, name AS name FROM employees;"
        ),
        "duplicate column alias 'name'"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT employee_id, employee_id FROM employees ORDER BY employee_id;"
        ),
        "column 'employee_id' is ambiguous"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT employee_id, employee_id FROM employees;"
        )
        .len(),
        6
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department, department AS dept
             FROM employees ORDER BY employees.department;"
        ),
        "column 'department' is ambiguous"
    );
    assert_eq!(
        failure(
            &mut database,
            "EXPLAIN SELECT DISTINCT department FROM employees ORDER BY salary;"
        ),
        "column 'salary' must appear in the SELECT DISTINCT list"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT name FROM employees ORDER BY salary ASC;"
        )
        .len(),
        7
    );
}

#[test]
fn limit_follows_deduplication_and_still_validates() {
    let mut database = employees();
    let limited = query(
        &mut database,
        "SELECT DISTINCT department FROM employees ORDER BY department ASC LIMIT 0;",
    );
    assert_eq!(limited.columns, vec!["department".to_string()]);
    assert!(limited.rows.is_empty());
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department FROM employees ORDER BY department DESC LIMIT 1;"
        ),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department FROM employees LIMIT 1;"
        ),
        vec![vec![text("eng")]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department FROM employees LIMIT 10;"
        )
        .len(),
        3
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT salary AS pay FROM employees WHERE salary > 'x' LIMIT 0;"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
    let mut empty = Database::new();
    empty.execute("CREATE TABLE vacant (age INT);").unwrap();
    assert_eq!(
        failure(
            &mut empty,
            "SELECT DISTINCT age FROM vacant WHERE age > 'x' LIMIT 0;"
        ),
        "cannot apply operator '>' to INTEGER and TEXT"
    );
}

#[test]
fn index_and_sequential_scans_return_the_same_distinct_rows() {
    let (_dir, path) = temporary("select-distinct-index");
    let mut database = Database::open(&path).unwrap();
    database
        .execute(
            "CREATE TABLE people (
                employee_id INT,
                department TEXT,
                active BOOLEAN
            );",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO people VALUES
                (1, 'eng', TRUE),
                (2, 'eng', TRUE),
                (3, 'eng', FALSE),
                (4, 'sales', FALSE),
                (5, 'sales', TRUE),
                (6, NULL, FALSE),
                (NULL, 'ghost', TRUE),
                (1, 'eng', TRUE);",
        )
        .unwrap();
    let equality = "SELECT DISTINCT department FROM people WHERE employee_id = 1;";
    let range = "SELECT DISTINCT department FROM people
        WHERE employee_id >= 2 AND employee_id <= 4;";
    let residual = "SELECT DISTINCT department FROM people
        WHERE employee_id >= 2 AND employee_id <= 4 AND active = TRUE;";
    let contradictory = "SELECT DISTINCT department FROM people
        WHERE employee_id > 3 AND employee_id < 2;";
    let ordered = "SELECT DISTINCT department, active FROM people
        WHERE employee_id >= 1 AND employee_id <= 6
        ORDER BY department ASC, active DESC
        LIMIT 4;";
    let nulls = "SELECT DISTINCT department FROM people ORDER BY department ASC NULLS FIRST;";
    let before_equality = rows(&mut database, equality);
    let before_range = rows(&mut database, range);
    let before_residual = rows(&mut database, residual);
    let before_ordered = rows(&mut database, ordered);
    let before_nulls = rows(&mut database, nulls);
    assert_eq!(before_equality, vec![vec![text("eng")]]);
    assert_eq!(before_range, vec![vec![text("eng")], vec![text("sales")]]);
    assert_eq!(before_residual, vec![vec![text("eng")]]);
    assert_eq!(
        before_ordered,
        vec![
            vec![text("eng"), boolean(true)],
            vec![text("eng"), boolean(false)],
            vec![text("sales"), boolean(true)],
            vec![text("sales"), boolean(false)],
        ]
    );
    assert_eq!(
        before_nulls,
        vec![
            vec![Value::Null],
            vec![text("eng")],
            vec![text("ghost")],
            vec![text("sales")],
        ]
    );
    assert!(rows(&mut database, contradictory).is_empty());
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT employee_id FROM people;"),
        [1, 2, 3, 4, 5, 6]
            .into_iter()
            .map(|id| vec![integer(id)])
            .chain(std::iter::once(vec![Value::Null]))
            .collect::<Vec<_>>()
    );
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_people_id ON people (employee_id);")
        .unwrap();
    assert_eq!(rows(&mut database, equality), before_equality);
    assert_eq!(rows(&mut database, range), before_range);
    assert_eq!(rows(&mut database, residual), before_residual);
    assert_eq!(rows(&mut database, ordered), before_ordered);
    assert_eq!(rows(&mut database, nulls), before_nulls);
    assert!(rows(&mut database, contradictory).is_empty());
    let explained = query(&mut database, &format!("EXPLAIN {range}"));
    assert_eq!(explained.rows[0][0], text("IndexScan"));
    assert_eq!(explained.rows[0][1], text("idx_people_id"));
    assert_eq!(
        explained.rows[0][2],
        text("employee_id >= 2 AND employee_id <= 4")
    );
    let none = query(&mut database, &format!("EXPLAIN {contradictory}"));
    assert_eq!(none.rows[0][0], text("IndexScan"));
    assert_eq!(none.rows[0][2], text("none"));
    let equal = query(&mut database, &format!("EXPLAIN {equality}"));
    assert_eq!(equal.rows[0][2], text("employee_id = 1"));
    drop(database);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(rows(&mut database, equality), before_equality);
    assert_eq!(rows(&mut database, range), before_range);
    assert_eq!(rows(&mut database, residual), before_residual);
    assert_eq!(rows(&mut database, ordered), before_ordered);
    assert_eq!(rows(&mut database, nulls), before_nulls);
    let reopened = query(&mut database, &format!("EXPLAIN {range}"));
    assert_eq!(reopened.rows[0][0], text("IndexScan"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM people WHERE employee_id = 1;"
        )
        .len(),
        2
    );

    database.execute("DROP INDEX idx_people_id;").unwrap();
    assert_eq!(rows(&mut database, range), before_range);
    assert_eq!(rows(&mut database, ordered), before_ordered);
    assert_eq!(rows(&mut database, nulls), before_nulls);
    let scanned = query(&mut database, &format!("EXPLAIN {range}"));
    assert_eq!(scanned.rows[0][0], text("SeqScan"));
}

#[test]
fn explain_plans_without_writing_and_a_restart_keeps_the_rows() {
    let (_dir, path) = temporary("select-distinct");
    let mut database = Database::open(&path).unwrap();
    database
        .execute(
            "CREATE TABLE employees (
                department TEXT,
                name TEXT,
                active BOOLEAN
            );",
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO employees VALUES
                ('eng', 'Zoe', TRUE),
                ('eng', 'Bea', TRUE),
                ('sales', 'Amy', FALSE);",
        )
        .unwrap();
    drop(database);
    let before = std::fs::read(&path).unwrap();
    let version = u32::from_le_bytes(before[32..36].try_into().unwrap());
    assert_eq!(version, 2);
    let mut database = Database::open(&path).unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT DISTINCT department AS dept
         FROM employees WHERE active = TRUE
         ORDER BY dept ASC LIMIT 10;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string(),
            "distinct".to_string(),
            "projection".to_string(),
            "filter".to_string(),
            "order_by".to_string(),
            "limit".to_string(),
        ]
    );
    assert_eq!(explained.rows[0][0], text("SeqScan"));
    assert_eq!(explained.rows[0][1], Value::Null);
    assert_eq!(explained.rows[0][2], Value::Null);
    assert_eq!(explained.rows[0][3], text("HashDistinct"));
    assert_eq!(explained.rows[0][4], text("department AS dept"));
    assert_eq!(explained.rows[0][5], text("active = TRUE"));
    assert_eq!(explained.rows[0][6], text("dept ASC"));
    assert_eq!(explained.rows[0][7], integer(10));
    let plain = query(&mut database, "EXPLAIN SELECT department FROM employees;");
    assert_eq!(
        plain.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string()
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT DISTINCT department FROM employees WHERE active = TRUE ORDER BY department;"
        ),
        vec![vec![text("eng")]]
    );
    drop(database);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    let mut database = Database::open(&path).unwrap();
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT department FROM employees;"),
        vec![vec![text("eng")], vec![text("sales")]]
    );
}

#[test]
fn join_group_by_and_aggregates_stay_rejected() {
    let mut database = employees();
    for sql in [
        "SELECT DISTINCT a.department FROM employees a
         JOIN employees b ON a.employee_id = b.employee_id;",
        "EXPLAIN SELECT DISTINCT a.department FROM employees a
         JOIN employees b ON a.employee_id = b.employee_id;",
    ] {
        assert_eq!(
            failure(&mut database, sql),
            "DISTINCT is not supported for JOIN",
            "{sql}"
        );
    }
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department FROM employees GROUP BY department;"
        ),
        "DISTINCT is not supported with GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT department FROM employees HAVING department = 'eng';"
        ),
        "DISTINCT is not supported with HAVING"
    );
    for sql in [
        "SELECT DISTINCT COUNT(*) FROM employees;",
        "SELECT DISTINCT department, SUM(salary) FROM employees;",
        "SELECT DISTINCT department FROM employees ORDER BY COUNT(*);",
        "SELECT DISTINCT MIN(salary) FROM employees;",
    ] {
        assert_eq!(
            failure(&mut database, sql),
            "DISTINCT is not supported with aggregate functions",
            "{sql}"
        );
    }
    assert_eq!(
        failure(&mut database, "SELECT ALL department FROM employees;"),
        "ALL is not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT DISTINCT ON (department) department FROM employees;"
        ),
        "DISTINCT ON is not supported"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(DISTINCT salary), COUNT(DISTINCT department) FROM employees;"
        ),
        vec![vec![integer(4), integer(2)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT DISTINCT salary FROM employees;"),
        vec![
            vec![integer(60000)],
            vec![integer(40000)],
            vec![integer(30000)],
            vec![Value::Null],
            vec![integer(10000)],
        ]
    );
}

#[test]
fn randomized_rows_match_a_linear_first_seen_model() {
    for seed in [0x0d14_u64, 0x51d4, 0xc014, 0x5e14, 0xa114, 0x1114] {
        for count in [0_usize, 1, 40, 80] {
            let sample = generate(seed, count);
            let mut database = Database::new();
            database
                .execute("CREATE TABLE sample (amount INT, label TEXT, flag BOOLEAN);")
                .unwrap();
            if !sample.is_empty() {
                let mut sql = String::from("INSERT INTO sample VALUES ");
                for (index, row) in sample.iter().enumerate() {
                    if index != 0 {
                        sql.push(',');
                    }
                    sql.push('(');
                    sql.push_str(&sql_int(row.amount));
                    sql.push_str(", '");
                    sql.push_str(&row.label);
                    sql.push_str("', ");
                    sql.push_str(&sql_bool(row.flag));
                    sql.push(')');
                }
                database.execute(&sql).unwrap();
            }
            expect(
                &mut database,
                "SELECT DISTINCT label FROM sample;",
                &distinct_of(&project(&sample, &[1])),
            );
            expect(
                &mut database,
                "SELECT DISTINCT amount, flag FROM sample;",
                &distinct_of(&project(&sample, &[0, 2])),
            );
            expect(
                &mut database,
                "SELECT DISTINCT label AS name FROM sample ORDER BY name ASC;",
                &sorted(&distinct_of(&project(&sample, &[1])), &[(0, true)]),
            );
            let filtered = sample
                .iter()
                .filter(|row| matches!(row.amount, Some(amount) if amount > 0))
                .cloned()
                .collect::<Vec<_>>();
            expect(
                &mut database,
                "SELECT DISTINCT amount, label FROM sample WHERE amount > 0;",
                &distinct_of(&project(&filtered, &[0, 1])),
            );
            let mut limited = sorted(&distinct_of(&project(&sample, &[1])), &[(0, false)]);
            limited.truncate(2);
            expect(
                &mut database,
                "SELECT DISTINCT label FROM sample ORDER BY label DESC LIMIT 2;",
                &limited,
            );
            expect(
                &mut database,
                "SELECT DISTINCT amount, flag FROM sample ORDER BY flag DESC, amount ASC;",
                &sorted(
                    &distinct_of(&project(&sample, &[0, 2])),
                    &[(1, false), (0, true)],
                ),
            );
        }
    }
}

fn expect(database: &mut Database, sql: &str, expected: &[Vec<Value>]) {
    assert_eq!(rows(database, sql), expected, "{sql}");
}

#[derive(Clone)]
struct SampleRow {
    amount: Option<i64>,
    label: String,
    flag: Option<bool>,
}

fn generate(seed: u64, count: usize) -> Vec<SampleRow> {
    let mut state = seed;
    let mut rows = Vec::with_capacity(count);
    for index in 0..count {
        let roll = next_value(&mut state) % 11;
        let amount = if roll == 0 {
            None
        } else if count > 40 {
            Some(i64::try_from(index % 4).unwrap())
        } else {
            Some(i64::try_from(next_value(&mut state) % 7).unwrap() - 3)
        };
        let letter = b'a' + u8::try_from(next_value(&mut state) % 4).unwrap_or(0);
        let letter = if roll == 1 {
            letter.to_ascii_uppercase()
        } else {
            letter
        };
        let flag = match next_value(&mut state) % 3 {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        };
        let letter = if index.is_multiple_of(5) {
            format!("{letter}{}", "x".repeat(48))
        } else {
            letter.to_string()
        };
        rows.push(SampleRow {
            amount,
            label: letter,
            flag,
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

fn project(rows: &[SampleRow], columns: &[usize]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| match column {
                    0 => row.amount.map(integer).unwrap_or(Value::Null),
                    1 => text(&row.label),
                    _ => row.flag.map(boolean).unwrap_or(Value::Null),
                })
                .collect()
        })
        .collect()
}

fn distinct_of(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    let mut kept: Vec<Vec<Value>> = Vec::new();
    for row in rows {
        if !kept.iter().any(|seen| same_tuple(seen, row)) {
            kept.push(row.clone());
        }
    }
    kept
}

fn same_tuple(left: &[Value], right: &[Value]) -> bool {
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

fn sorted(rows: &[Vec<Value>], keys: &[(usize, bool)]) -> Vec<Vec<Value>> {
    let mut ordered = rows.to_vec();
    ordered.sort_by(|left, right| {
        for (index, ascending) in keys {
            let ordering = compare_cell(&left[*index], &right[*index], *ascending);
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    });
    ordered
}

fn compare_cell(left: &Value, right: &Value, ascending: bool) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => {
            if ascending {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (_, Value::Null) => {
            if ascending {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Value::Integer(left), Value::Integer(right)) => directed(left.cmp(right), ascending),
        (Value::Text(left), Value::Text(right)) => directed(left.cmp(right), ascending),
        (Value::Boolean(left), Value::Boolean(right)) => directed(left.cmp(right), ascending),
        _ => Ordering::Equal,
    }
}

fn directed(ordering: Ordering, ascending: bool) -> Ordering {
    if ascending {
        ordering
    } else {
        ordering.reverse()
    }
}

fn sql_int(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_bool(value: Option<bool>) -> String {
    match value {
        None => "NULL".to_string(),
        Some(true) => "TRUE".to_string(),
        Some(false) => "FALSE".to_string(),
    }
}

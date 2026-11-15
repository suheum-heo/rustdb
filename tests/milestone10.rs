//! Single-table `GROUP BY`, `COUNT`, and `SUM`.

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

fn employees() -> Database {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, team INT, salary INT, active BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO employees VALUES
                ('eng', 1, 10, TRUE),
                ('sales', 1, NULL, FALSE),
                ('eng', 1, 30, TRUE),
                (NULL, 2, 5, TRUE),
                ('eng', NULL, NULL, FALSE),
                (NULL, 2, NULL, TRUE);",
        )
        .unwrap();
    database
}

#[test]
fn count_star_count_column_and_sum_match_a_reference() {
    let mut database = employees();
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM employees;"),
        vec![vec![integer(6)]]
    );
    assert_eq!(
        query(&mut database, "SELECT COUNT(*) FROM employees;").columns,
        vec!["COUNT(*)".to_string()]
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(salary) FROM employees;"),
        vec![vec![integer(3)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT SUM(salary) FROM employees;"),
        vec![vec![integer(45)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(*), COUNT(salary), SUM(salary)
             FROM employees GROUP BY department;"
        ),
        vec![
            vec![text("eng"), integer(3), integer(2), integer(40)],
            vec![text("sales"), integer(1), integer(0), Value::Null],
            vec![Value::Null, integer(2), integer(1), integer(5)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(salary), SUM(salary), COUNT(*), SUM(salary),
                    COUNT(employees.salary), SUM(employees.salary)
             FROM employees;"
        ),
        vec![vec![
            integer(6),
            integer(3),
            integer(45),
            integer(6),
            integer(45),
            integer(3),
            integer(45),
        ]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM employees GROUP BY department ORDER BY SUM(salary);"
        ),
        vec![vec![integer(2)], vec![integer(3)], vec![integer(1)]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT active, COUNT(*), COUNT(salary), SUM(salary)
             FROM employees GROUP BY active;"
        ),
        vec![
            vec![Value::Boolean(true), integer(4), integer(3), integer(45)],
            vec![Value::Boolean(false), integer(2), integer(0), Value::Null],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees GROUP BY department;"
        ),
        vec![text("eng"), text("sales"), Value::Null]
            .into_iter()
            .map(|value| vec![value])
            .collect::<Vec<_>>()
    );
}

#[test]
fn empty_input_returns_zero_counts_and_a_null_sum() {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    assert_eq!(
        rows(&mut database, "SELECT COUNT(*) FROM employees;"),
        vec![vec![integer(0)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT COUNT(salary) FROM employees;"),
        vec![vec![integer(0)]]
    );
    assert_eq!(
        rows(&mut database, "SELECT SUM(salary) FROM employees;"),
        vec![vec![Value::Null]]
    );
    let grouped = query(
        &mut database,
        "SELECT department, COUNT(*) FROM employees GROUP BY department;",
    );
    assert!(grouped.rows.is_empty());
    assert_eq!(
        grouped.columns,
        vec!["department".to_string(), "COUNT(*)".to_string()]
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM(department) FROM employees;"),
        "SUM requires INTEGER, not TEXT"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM employees WHERE department = 1;"
        ),
        "cannot apply operator '=' to TEXT and INTEGER"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(salary), SUM(salary)
             FROM employees WHERE salary > 1 AND salary < 0;"
        ),
        vec![vec![integer(0), integer(0), Value::Null]]
    );
    let removed = query(
        &mut database,
        "SELECT department, COUNT(*) FROM employees
         WHERE salary > 1 AND salary < 0 GROUP BY department;",
    );
    assert!(removed.rows.is_empty());
    assert_eq!(
        removed.columns,
        vec!["department".to_string(), "COUNT(*)".to_string()]
    );
}

#[test]
fn where_order_and_limit_run_around_grouping() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, SUM(salary) FROM employees
             WHERE active = TRUE GROUP BY department ORDER BY department;"
        ),
        vec![
            vec![text("eng"), integer(40)],
            vec![Value::Null, integer(5)],
        ]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(*) FROM employees
             GROUP BY department ORDER BY COUNT(*) DESC, department LIMIT 2;"
        ),
        vec![vec![text("eng"), integer(3)], vec![Value::Null, integer(2)],]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM employees WHERE active = TRUE LIMIT 0;"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(salary), SUM(salary)
             FROM employees WHERE active = TRUE AND active = FALSE;"
        ),
        vec![vec![integer(0), integer(0), Value::Null]]
    );
    assert!(
        rows(
            &mut database,
            "SELECT department, COUNT(*) FROM employees
             WHERE active = TRUE AND active = FALSE GROUP BY department;"
        )
        .is_empty()
    );
}

#[test]
fn multiple_columns_group_nulls_together_and_keep_first_seen_order() {
    let mut database = employees();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, team, COUNT(*) FROM employees GROUP BY department, team;"
        ),
        vec![
            vec![text("eng"), integer(1), integer(2)],
            vec![text("sales"), integer(1), integer(1)],
            vec![Value::Null, integer(2), integer(2)],
            vec![text("eng"), Value::Null, integer(1)],
        ]
    );
}

#[test]
fn invalid_aggregates_are_rejected_before_reading_rows() {
    let mut database = employees();
    assert_eq!(
        failure(&mut database, "SELECT department, COUNT(*) FROM employees;"),
        "column 'department' must appear in GROUP BY"
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM(COUNT(*)) FROM employees;"),
        "nested aggregates are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM employees WHERE COUNT(*) > 1;"
        ),
        "aggregates are not allowed in WHERE"
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM(active) FROM employees;"),
        "SUM requires INTEGER, not BOOLEAN"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT(missing) FROM employees;"),
        "column 'missing' does not exist in table 'employees'"
    );
    assert_eq!(
        failure(&mut database, "SELECT lower(department) FROM employees;"),
        "function calls are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(DISTINCT salary) FROM employees;"
        ),
        "DISTINCT aggregates are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM employees HAVING COUNT(*) > 1;"
        ),
        "HAVING is not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM employees e JOIN employees b ON e.team = b.team;"
        ),
        "aggregates over JOIN are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT * FROM employees e JOIN employees b ON COUNT(*) = 1;"
        ),
        "aggregates are not allowed in ON"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT * FROM employees e JOIN employees b ON SUM(e.salary) > 1;"
        ),
        "aggregates are not allowed in ON"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT() FROM employees;"),
        "aggregate functions take one argument"
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM() FROM employees;"),
        "aggregate functions take one argument"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT(1) FROM employees;"),
        "aggregate arguments must be columns"
    );
    assert_eq!(
        failure(&mut database, "SELECT SUM(1) FROM employees;"),
        "aggregate arguments must be columns"
    );
    assert_eq!(
        failure(&mut database, "SELECT COUNT(*) AS total FROM employees;"),
        "column aliases are not supported"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT * FROM employees GROUP BY department;"
        ),
        "SELECT * is not supported with GROUP BY or aggregates"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT COUNT(*) FROM employees GROUP BY department ORDER BY salary;"
        ),
        "column 'salary' must appear in GROUP BY"
    );
    assert_eq!(
        failure(
            &mut database,
            "SELECT employees.department, COUNT(*) FROM employees e GROUP BY e.department;"
        ),
        "table qualifier 'employees' does not match 'e'"
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT e.department, COUNT(*) FROM employees e GROUP BY department;"
        )
        .len(),
        3
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees ORDER BY department;"
        ),
        vec![
            vec![text("eng")],
            vec![text("eng")],
            vec![text("eng")],
            vec![text("sales")],
            vec![Value::Null],
            vec![Value::Null],
        ]
    );
}

#[test]
fn sum_overflow_is_an_error() {
    let mut database = Database::new();
    database.execute("CREATE TABLE numbers (n INT);").unwrap();
    database
        .execute("INSERT INTO numbers VALUES (9223372036854775807), (1);")
        .unwrap();
    assert_eq!(
        failure(&mut database, "SELECT SUM(n) FROM numbers;"),
        "SUM overflows INTEGER"
    );
    assert_eq!(
        rows(&mut database, "SELECT SUM(n) FROM numbers WHERE n = 1;"),
        vec![vec![integer(1)]]
    );
    database
        .execute("INSERT INTO numbers VALUES (-9223372036854775808), (-1);")
        .unwrap();
    assert_eq!(
        failure(&mut database, "SELECT SUM(n) FROM numbers WHERE n < 0;"),
        "SUM overflows INTEGER"
    );
}

#[test]
fn explain_names_the_aggregate_and_leaves_ordinary_selects_alone() {
    let mut database = employees();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT department, COUNT(*), SUM(salary)
         FROM employees WHERE active = TRUE GROUP BY department;",
    );
    assert_eq!(
        explained.columns,
        vec![
            "access".to_string(),
            "groups".to_string(),
            "aggregates".to_string(),
            "scan".to_string(),
            "index".to_string(),
            "bounds".to_string(),
            "filter".to_string(),
        ]
    );
    assert_eq!(explained.rows[0][0], text("HashAggregate"));
    assert_eq!(explained.rows[0][1], text("department"));
    assert_eq!(explained.rows[0][2], text("COUNT(*), SUM(salary)"));
    assert_eq!(explained.rows[0][3], text("SeqScan"));
    assert_eq!(explained.rows[0][6], text("active = TRUE"));
    let scalar = query(&mut database, "EXPLAIN SELECT COUNT(*) FROM employees;");
    assert_eq!(scalar.rows[0][0], text("Aggregate"));
    assert_eq!(scalar.rows[0][1], Value::Null);
    assert_eq!(scalar.rows[0][2], text("COUNT(*)"));
    let ordered = query(
        &mut database,
        "EXPLAIN SELECT department, COUNT(*) FROM employees
         GROUP BY department ORDER BY SUM(salary);",
    );
    assert_eq!(ordered.rows[0][2], text("COUNT(*), SUM(salary)"));
    let plain = query(
        &mut database,
        "EXPLAIN SELECT department FROM employees WHERE salary = 10;",
    );
    assert_eq!(
        plain.columns,
        vec![
            "access".to_string(),
            "index".to_string(),
            "bounds".to_string()
        ]
    );
    assert_eq!(plain.rows[0][0], text("SeqScan"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT department FROM employees ORDER BY department;"
        )
        .len(),
        6
    );
}

#[test]
fn an_indexed_filter_aggregates_the_index_candidates() {
    let (_dir, path) = temporary("indexed-aggregate");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE employees (salary INT, department TEXT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES (10, 'eng'), (10, 'eng'), (30, 'sales');")
        .unwrap();
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_salary ON employees(salary);")
        .unwrap();
    let explained = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM employees WHERE salary = 10;",
    );
    assert_eq!(explained.rows[0][0], text("Aggregate"));
    assert_eq!(explained.rows[0][3], text("IndexScan"));
    assert_eq!(explained.rows[0][4], text("idx_salary"));
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*) FROM employees WHERE salary = 10;"
        ),
        vec![vec![integer(2)]]
    );
}

#[test]
fn restart_keeps_the_groups_and_explain_does_not_write() {
    let (_dir, path) = temporary("aggregate");
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE TABLE employees (department TEXT, salary INT);")
        .unwrap();
    database
        .execute("INSERT INTO employees VALUES ('eng', 10), ('eng', 30), (NULL, NULL);")
        .unwrap();
    drop(database);
    let before = std::fs::metadata(&path).unwrap().len();
    let mut database = Database::open(&path).unwrap();
    let expected = vec![
        vec![text("eng"), integer(2), integer(40)],
        vec![Value::Null, integer(1), Value::Null],
    ];
    let sql = "SELECT department, COUNT(*), SUM(salary) FROM employees GROUP BY department;";
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
fn randomized_groups_match_a_reference_model() {
    check_reference(&[]);
    let all_null = vec![
        Input {
            seq: None,
            department: None,
            team: None,
            salary: None,
            flag: None,
        };
        4
    ];
    check_reference(&all_null);
    for seed in [0x0a99_u64, 0x51c0, 0xc0ff, 0x5eed, 0xa11c] {
        check_seed(seed);
    }
}

fn check_seed(seed: u64) {
    let mut state = seed;
    let mut stored = Vec::with_capacity(48);
    for n in 0..48_u64 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        let department = if n.is_multiple_of(5) {
            None
        } else if n.is_multiple_of(2) {
            Some("eng".to_string())
        } else {
            Some(format!("d{}", state % 4))
        };
        let team = if n.is_multiple_of(7) {
            None
        } else {
            Some((state % 3) as i64)
        };
        let salary = if n.is_multiple_of(3) {
            None
        } else {
            Some((state % 20) as i64)
        };
        let flag = if n.is_multiple_of(11) {
            None
        } else {
            Some(n.is_multiple_of(2))
        };
        stored.push(Input {
            seq: Some(i64::try_from(n).unwrap()),
            department,
            team,
            salary,
            flag,
        });
    }
    check_reference(&stored);
    let mut database = load_inputs(&stored);
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag, COUNT(*) FROM employees GROUP BY flag;"
        )
        .len(),
        3
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT seq, COUNT(*), SUM(salary) FROM employees GROUP BY seq;"
        )
        .len(),
        48
    );
}

fn check_reference(stored: &[Input]) {
    let mut database = load_inputs(stored);
    assert_eq!(
        rows(
            &mut database,
            "SELECT COUNT(*), COUNT(salary), SUM(salary), COUNT(*), SUM(salary) FROM employees;"
        ),
        vec![vec![
            integer(i64::try_from(stored.len()).unwrap()),
            integer(
                i64::try_from(stored.iter().filter(|row| row.salary.is_some()).count()).unwrap()
            ),
            sum_value(stored),
            integer(i64::try_from(stored.len()).unwrap()),
            sum_value(stored),
        ]]
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, team, COUNT(*), COUNT(salary), SUM(salary)
             FROM employees GROUP BY department, team;"
        ),
        project(
            &accumulate(stored, |row| vec![
                cell_text(row.department.clone()),
                cell_int(row.team),
            ]),
            false
        )
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, team, flag, COUNT(*), COUNT(*), COUNT(salary), SUM(salary), SUM(salary)
             FROM employees GROUP BY department, team, flag;"
        ),
        project(
            &accumulate(stored, |row| vec![
                cell_text(row.department.clone()),
                cell_int(row.team),
                cell_bool(row.flag),
            ]),
            true
        )
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT flag, COUNT(*), COUNT(salary), SUM(salary) FROM employees GROUP BY flag;"
        ),
        project(&accumulate(stored, |row| vec![cell_bool(row.flag)]), false)
    );
    assert_eq!(
        rows(
            &mut database,
            "SELECT seq, COUNT(*), SUM(salary) FROM employees GROUP BY seq;"
        ),
        accumulate(stored, |row| vec![cell_int(row.seq)])
            .iter()
            .map(|group| {
                vec![
                    cell_value(&group.key[0]),
                    integer(group.count_star),
                    group.sum.map(integer).unwrap_or(Value::Null),
                ]
            })
            .collect::<Vec<_>>()
    );
    let filtered: Vec<Input> = stored
        .iter()
        .filter(|row| row.salary.is_some_and(|salary| salary > 10))
        .cloned()
        .collect();
    let mut ordered = project(
        &accumulate(&filtered, |row| {
            vec![cell_text(row.department.clone()), cell_int(row.team)]
        }),
        false,
    );
    ordered.sort_by(|left, right| cmp_text_asc(&left[0], &right[0]));
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, team, COUNT(*), COUNT(salary), SUM(salary)
             FROM employees WHERE salary > 10
             GROUP BY department, team ORDER BY department;"
        ),
        ordered
    );
    let flagged: Vec<Input> = stored
        .iter()
        .filter(|row| row.flag == Some(true))
        .cloned()
        .collect();
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(*), SUM(salary)
             FROM employees WHERE flag = TRUE GROUP BY department;"
        ),
        accumulate(&flagged, |row| vec![cell_text(row.department.clone())])
            .iter()
            .map(|group| {
                vec![
                    cell_value(&group.key[0]),
                    integer(group.count_star),
                    group.sum.map(integer).unwrap_or(Value::Null),
                ]
            })
            .collect::<Vec<_>>()
    );
    let mut limited = accumulate(stored, |row| vec![cell_text(row.department.clone())])
        .iter()
        .map(|group| {
            vec![
                cell_value(&group.key[0]),
                integer(group.count_star),
                group.sum.map(integer).unwrap_or(Value::Null),
            ]
        })
        .collect::<Vec<_>>();
    limited.sort_by(|left, right| {
        let sum = cmp_int_desc(&left[2], &right[2]);
        if sum == std::cmp::Ordering::Equal {
            cmp_text_asc(&left[0], &right[0])
        } else {
            sum
        }
    });
    limited.truncate(3);
    assert_eq!(
        rows(
            &mut database,
            "SELECT department, COUNT(*), SUM(salary)
             FROM employees GROUP BY department
             ORDER BY SUM(salary) DESC, department LIMIT 3;"
        ),
        limited
    );
}

fn load_inputs(stored: &[Input]) -> Database {
    let mut database = Database::new();
    database
        .execute(
            "CREATE TABLE employees (seq INT, department TEXT, team INT, salary INT, flag BOOLEAN);",
        )
        .unwrap();
    for row in stored {
        database
            .execute(&format!(
                "INSERT INTO employees VALUES ({}, {}, {}, {}, {});",
                sql_int(row.seq),
                sql_text(row.department.as_deref()),
                sql_int(row.team),
                sql_int(row.salary),
                sql_bool(row.flag),
            ))
            .unwrap();
    }
    database
}

fn sum_value(rows: &[Input]) -> Value {
    let mut total = None;
    for row in rows {
        if let Some(salary) = row.salary {
            total = Some(
                total
                    .unwrap_or(0_i64)
                    .checked_add(salary)
                    .expect("reference sum"),
            );
        }
    }
    total.map(integer).unwrap_or(Value::Null)
}

#[derive(Clone)]
struct Input {
    seq: Option<i64>,
    department: Option<String>,
    team: Option<i64>,
    salary: Option<i64>,
    flag: Option<bool>,
}

#[derive(Clone, PartialEq, Eq)]
enum Cell {
    Null,
    Int(i64),
    Text(String),
    Bool(bool),
}

struct Acc {
    key: Vec<Cell>,
    count_star: i64,
    count_salary: i64,
    sum: Option<i64>,
}

fn accumulate(rows: &[Input], key_of: impl Fn(&Input) -> Vec<Cell>) -> Vec<Acc> {
    let mut groups: Vec<Acc> = Vec::new();
    for row in rows {
        let key = key_of(row);
        let group = if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
            group
        } else {
            groups.push(Acc {
                key,
                count_star: 0,
                count_salary: 0,
                sum: None,
            });
            groups.last_mut().unwrap()
        };
        group.count_star += 1;
        if let Some(salary) = row.salary {
            group.count_salary += 1;
            group.sum = Some(
                group
                    .sum
                    .unwrap_or(0)
                    .checked_add(salary)
                    .expect("reference sum"),
            );
        }
    }
    groups
}

fn project(groups: &[Acc], repeat: bool) -> Vec<Vec<Value>> {
    groups
        .iter()
        .map(|group| {
            let mut row = group.key.iter().map(cell_value).collect::<Vec<_>>();
            row.push(integer(group.count_star));
            if repeat {
                row.push(integer(group.count_star));
            }
            row.push(integer(group.count_salary));
            let sum = group.sum.map(integer).unwrap_or(Value::Null);
            row.push(sum.clone());
            if repeat {
                row.push(sum);
            }
            row
        })
        .collect()
}

fn cell_text(value: Option<String>) -> Cell {
    value.map(Cell::Text).unwrap_or(Cell::Null)
}

fn cell_int(value: Option<i64>) -> Cell {
    value.map(Cell::Int).unwrap_or(Cell::Null)
}

fn cell_bool(value: Option<bool>) -> Cell {
    value.map(Cell::Bool).unwrap_or(Cell::Null)
}

fn cell_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Int(value) => integer(*value),
        Cell::Text(value) => text(value),
        Cell::Bool(value) => Value::Boolean(*value),
    }
}

fn cmp_text_asc(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        _ => std::cmp::Ordering::Equal,
    }
}

fn cmp_int_desc(left: &Value, right: &Value) -> std::cmp::Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (Value::Integer(left), Value::Integer(right)) => right.cmp(left),
        _ => std::cmp::Ordering::Equal,
    }
}

fn sql_text(value: Option<&str>) -> String {
    match value {
        Some(value) => format!("'{value}'"),
        None => "NULL".to_string(),
    }
}

fn sql_int(value: Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NULL".to_string())
}

fn sql_bool(value: Option<bool>) -> String {
    match value {
        Some(true) => "TRUE".to_string(),
        Some(false) => "FALSE".to_string(),
        None => "NULL".to_string(),
    }
}

#[test]
fn indexed_filters_match_a_sequential_aggregation() {
    let queries = [
        "SELECT label, COUNT(*), COUNT(amount), SUM(amount) FROM samples WHERE bucket = 10 GROUP BY label;",
        "SELECT label, COUNT(*), SUM(amount) FROM samples WHERE bucket >= 15 AND bucket <= 30 GROUP BY label;",
        "SELECT label, COUNT(*), SUM(amount) FROM samples WHERE bucket >= 15 AND flag = TRUE GROUP BY label;",
        "SELECT label, SUM(amount) FROM samples WHERE bucket >= 15 GROUP BY label ORDER BY SUM(amount) DESC, label LIMIT 2;",
        "SELECT label FROM samples WHERE bucket = 10 GROUP BY label;",
        "SELECT label, COUNT(*) FROM samples WHERE bucket = 10 GROUP BY label ORDER BY label LIMIT 1;",
        "SELECT COUNT(*), COUNT(amount), SUM(amount) FROM samples WHERE bucket > 30 AND bucket < 10;",
        "SELECT label, COUNT(*) FROM samples WHERE bucket > 30 AND bucket < 10 GROUP BY label;",
        "SELECT COUNT(*) FROM samples WHERE bucket = 10;",
        "SELECT label, COUNT(*), SUM(amount) FROM samples WHERE bucket >= 0 GROUP BY label;",
        "SELECT bucket, label, COUNT(*), SUM(amount) FROM samples WHERE bucket >= 15 GROUP BY bucket, label;",
    ];
    let mut memory = Database::new();
    load_samples(&mut memory);
    let expected: Vec<Vec<Vec<Value>>> = queries.iter().map(|sql| rows(&mut memory, sql)).collect();
    assert_eq!(
        expected[0],
        vec![
            vec![text("a"), integer(2), integer(2), integer(7)],
            vec![text("b"), integer(1), integer(0), Value::Null],
            vec![Value::Null, integer(1), integer(1), integer(1)],
        ]
    );
    assert_eq!(
        expected[1],
        vec![
            vec![text("b"), integer(2), integer(4)],
            vec![text("a"), integer(1), integer(0)],
            vec![text("c"), integer(1), integer(1)],
        ]
    );
    assert_eq!(
        expected[3],
        vec![vec![text("a"), integer(7)], vec![text("b"), integer(4)]]
    );
    assert_eq!(
        expected[4],
        vec![vec![text("a")], vec![text("b")], vec![Value::Null]]
    );
    assert_eq!(expected[6], vec![vec![integer(0), integer(0), Value::Null]]);
    assert!(expected[7].is_empty());
    assert_eq!(expected[8], vec![vec![integer(4)]]);
    assert_eq!(
        expected[9],
        vec![
            vec![text("a"), integer(4), integer(14)],
            vec![text("b"), integer(3), integer(4)],
            vec![Value::Null, integer(1), integer(1)],
            vec![text("c"), integer(1), integer(1)],
        ]
    );

    let (_dir, path) = temporary("agg-scan");
    let mut database = Database::open(&path).unwrap();
    load_samples(&mut database);
    drop(database);
    Database::migrate(&path).unwrap();
    let mut database = Database::open(&path).unwrap();
    database
        .execute("CREATE INDEX idx_bucket ON samples(bucket);")
        .unwrap();
    let indexed: Vec<Vec<Vec<Value>>> =
        queries.iter().map(|sql| rows(&mut database, sql)).collect();
    assert_eq!(indexed, expected);

    let equality = query(
        &mut database,
        "EXPLAIN SELECT label, COUNT(*) FROM samples WHERE bucket = 10 GROUP BY label;",
    );
    assert_eq!(equality.rows[0][0], text("HashAggregate"));
    assert_eq!(equality.rows[0][3], text("IndexScan"));
    assert_eq!(equality.rows[0][4], text("idx_bucket"));
    assert_eq!(equality.rows[0][5], text("bucket = 10"));
    let range = query(
        &mut database,
        "EXPLAIN SELECT label, COUNT(*) FROM samples WHERE bucket >= 15 AND bucket <= 30 GROUP BY label;",
    );
    assert_eq!(range.rows[0][3], text("IndexScan"));
    assert_eq!(range.rows[0][5], text("bucket >= 15 AND bucket <= 30"));
    let residual = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM samples WHERE bucket >= 15 AND flag = TRUE;",
    );
    assert_eq!(residual.rows[0][0], text("Aggregate"));
    assert_eq!(residual.rows[0][3], text("IndexScan"));
    assert_eq!(residual.rows[0][6], text("bucket >= 15 AND flag = TRUE"));
    let none = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM samples WHERE bucket > 30 AND bucket < 10;",
    );
    assert_eq!(none.rows[0][3], text("IndexScan"));
    assert_eq!(none.rows[0][5], text("none"));
    let before = std::fs::metadata(&path).unwrap().len();
    let _ = query(
        &mut database,
        "EXPLAIN SELECT label, SUM(amount) FROM samples WHERE bucket = 10 GROUP BY label ORDER BY SUM(amount);",
    );
    assert_eq!(rows(&mut database, queries[0]), expected[0]);
    let mut wal = path.clone().into_os_string();
    wal.push(".wal");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    drop(database);

    let mut database = Database::open(&path).unwrap();
    let reopened: Vec<Vec<Vec<Value>>> =
        queries.iter().map(|sql| rows(&mut database, sql)).collect();
    assert_eq!(reopened, expected);
    let still = query(
        &mut database,
        "EXPLAIN SELECT COUNT(*) FROM samples WHERE bucket = 10;",
    );
    assert_eq!(still.rows[0][3], text("IndexScan"));
    assert_eq!(still.rows[0][4], text("idx_bucket"));
    database.execute("DROP INDEX idx_bucket;").unwrap();
    let sequential: Vec<Vec<Vec<Value>>> =
        queries.iter().map(|sql| rows(&mut database, sql)).collect();
    assert_eq!(sequential, expected);
    let scanned = query(
        &mut database,
        "EXPLAIN SELECT label, COUNT(*) FROM samples WHERE bucket = 10 GROUP BY label;",
    );
    assert_eq!(scanned.rows[0][3], text("SeqScan"));
}

fn load_samples(database: &mut Database) {
    database
        .execute("CREATE TABLE samples (bucket INT, amount INT, label TEXT, flag BOOLEAN);")
        .unwrap();
    database
        .execute(
            "INSERT INTO samples VALUES
                (10, 5, 'a', TRUE),
                (10, NULL, 'b', FALSE),
                (40, 7, 'a', TRUE),
                (NULL, 9, 'a', TRUE),
                (10, 1, NULL, TRUE),
                (30, NULL, 'b', FALSE),
                (20, 4, 'b', TRUE),
                (15, 0, 'a', FALSE),
                (10, 2, 'a', TRUE),
                (25, 1, 'c', TRUE);",
        )
        .unwrap();
}

#[test]
#[ignore = "release profile of aggregation"]
fn profile_aggregation() {
    let cpu = command_text("sysctl", &["-n", "machdep.cpu.brand_string"]);
    let memory = command_text("sysctl", &["-n", "hw.memsize"]);
    let rustc = command_text("rustc", &["--version"]);
    println!("aggregate cpu={cpu} mem_bytes={memory} rustc={rustc}");
    for rows in [1_000_i64, 10_000, 100_000] {
        measure(rows);
    }
}

fn measure(rows: i64) {
    let mut database = Database::new();
    database
        .execute("CREATE TABLE samples (id INT, bucket INT, amount INT, flag BOOLEAN);")
        .unwrap();
    for id in 0..rows {
        let bucket = id % 10;
        let amount = if id % 2 == 0 {
            "NULL".to_string()
        } else {
            id.to_string()
        };
        let flag = if id % 3 == 0 { "FALSE" } else { "TRUE" };
        database
            .execute(&format!(
                "INSERT INTO samples VALUES ({id}, {bucket}, {amount}, {flag});"
            ))
            .unwrap();
    }
    time(
        "count",
        rows,
        &mut database,
        "SELECT COUNT(*) FROM samples;",
        1,
        0,
        false,
    );
    time(
        "few",
        rows,
        &mut database,
        "SELECT bucket, COUNT(*) FROM samples GROUP BY bucket;",
        10,
        1,
        true,
    );
    time(
        "many",
        rows,
        &mut database,
        "SELECT id, SUM(amount) FROM samples GROUP BY id;",
        rows as usize,
        1,
        true,
    );
    time(
        "null-sum",
        rows,
        &mut database,
        "SELECT SUM(amount) FROM samples;",
        1,
        0,
        false,
    );
    time(
        "filtered",
        rows,
        &mut database,
        "SELECT bucket, SUM(amount) FROM samples WHERE flag = TRUE GROUP BY bucket;",
        10,
        1,
        true,
    );
}

fn time(
    shape: &str,
    rows: i64,
    database: &mut Database,
    sql: &str,
    groups: usize,
    key_cells: usize,
    hashed: bool,
) {
    let mut samples = Vec::with_capacity(6);
    let mut checksum = 0i64;
    let mut returned = 0usize;
    for _ in 0..6 {
        let started = std::time::Instant::now();
        let result = query(database, sql);
        let elapsed = started.elapsed().as_micros();
        checksum = 0;
        returned = result.rows.len();
        for row in &result.rows {
            for value in row {
                if let Value::Integer(value) = value {
                    checksum = checksum.wrapping_add(*value);
                }
            }
        }
        std::hint::black_box(checksum);
        std::hint::black_box(returned);
        samples.push(elapsed);
    }
    let cold = samples[0];
    let mut warm = samples[1..].to_vec();
    warm.sort_unstable();
    let warm = warm[warm.len() / 2];
    let bytes = aggregate_bytes(groups, key_cells, 1, hashed);
    println!(
        "aggregate shape={shape} rows={rows} groups={groups} returned={returned} checksum={checksum} bytes={bytes} cold_us={cold} warm_us={warm}"
    );
}

fn aggregate_bytes(groups: usize, key_cells: usize, aggs: usize, hashed: bool) -> usize {
    let value = std::mem::size_of::<Value>();
    let vector = std::mem::size_of::<Vec<Value>>();
    // `AccValue` is 16 bytes. `executor::tests` locks that size.
    let accumulator = 16usize;
    let keys = if hashed {
        groups.saturating_mul(vector.saturating_add(key_cells.saturating_mul(value)))
    } else {
        0
    };
    let acc = groups.saturating_mul(vector.saturating_add(aggs.saturating_mul(accumulator)));
    let map_groups = if hashed { groups } else { 0 };
    let map_entry = std::mem::size_of::<u64>()
        + std::mem::size_of::<Vec<usize>>()
        + std::mem::size_of::<usize>();
    keys.saturating_add(acc)
        .saturating_add(map_groups.saturating_mul(map_entry))
}

fn command_text(program: &str, args: &[&str]) -> String {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

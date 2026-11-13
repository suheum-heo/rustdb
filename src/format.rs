//! Text rendering for statement results.

use crate::executor::StatementResult;
use crate::value::Value;

/// Render one statement result the way the REPL prints it.
#[must_use]
pub fn render_result(result: &StatementResult) -> String {
    match result {
        StatementResult::Ok => "OK".to_string(),
        StatementResult::Insert { count } => row_phrase(*count, "inserted"),
        StatementResult::Update { count } => row_phrase(*count, "updated"),
        StatementResult::Delete { count } => row_phrase(*count, "deleted"),
        StatementResult::Query(query) => {
            let mut rendered = render_table(&query.columns, &query.rows);
            rendered.push('\n');
            rendered.push_str(&row_phrase(query.rows.len(), "returned"));
            rendered
        }
    }
}

fn row_phrase(count: usize, verb: &str) -> String {
    if count == 1 {
        format!("1 row {verb}")
    } else {
        format!("{count} rows {verb}")
    }
}

fn render_table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let cells = render_cells(columns, rows);
    let widths = column_widths(columns, &cells);
    let rule = horizontal_rule(&widths);

    let mut out = String::new();
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&format_row(columns, &widths));
    out.push('\n');
    out.push_str(&rule);
    for row in &cells {
        out.push('\n');
        out.push_str(&format_row(row, &widths));
    }
    if !cells.is_empty() {
        out.push('\n');
        out.push_str(&rule);
    }
    out
}

fn render_cells(columns: &[String], rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| row.get(index).map(ToString::to_string).unwrap_or_default())
                .collect()
        })
        .collect()
}

fn column_widths(columns: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = columns
        .iter()
        .map(|column| column.chars().count())
        .collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(index) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }
    widths
}

fn horizontal_rule(widths: &[usize]) -> String {
    let mut line = String::from("+");
    for width in widths {
        line.push_str(&"-".repeat(width.saturating_add(2)));
        line.push('+');
    }
    line
}

fn format_row(cells: &[String], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, width) in widths.iter().enumerate() {
        let cell = cells.get(index).map(String::as_str).unwrap_or("");
        let padding = width.saturating_sub(cell.chars().count());
        line.push('|');
        line.push(' ');
        line.push_str(cell);
        line.push_str(&" ".repeat(padding));
        line.push(' ');
    }
    line.push('|');
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::QueryResult;

    #[test]
    fn pads_columns_and_uses_singular_row_text() {
        let rendered = render_result(&StatementResult::Query(QueryResult {
            columns: vec!["name".to_string()],
            rows: vec![vec![Value::Text("Bob".to_string())]],
        }));
        assert_eq!(
            rendered,
            "\
+------+
| name |
+------+
| Bob  |
+------+
1 row returned"
        );
    }

    #[test]
    fn aligns_a_value_wider_than_its_header() {
        let rendered = render_table(
            &["id".to_string(), "name".to_string()],
            &[
                vec![Value::Integer(1), Value::Text("Ada".to_string())],
                vec![Value::Integer(20), Value::Text("Christopher".to_string())],
            ],
        );
        assert_eq!(
            rendered,
            "\
+----+-------------+
| id | name        |
+----+-------------+
| 1  | Ada         |
| 20 | Christopher |
+----+-------------+"
        );
    }

    #[test]
    fn pluralizes_insert_and_empty_results() {
        assert_eq!(
            render_result(&StatementResult::Insert { count: 1 }),
            "1 row inserted"
        );
        assert_eq!(
            render_result(&StatementResult::Insert { count: 2 }),
            "2 rows inserted"
        );
        assert_eq!(
            render_result(&StatementResult::Update { count: 1 }),
            "1 row updated"
        );
        assert_eq!(
            render_result(&StatementResult::Delete { count: 0 }),
            "0 rows deleted"
        );
        assert_eq!(
            render_result(&StatementResult::Query(QueryResult {
                columns: vec!["age".to_string()],
                rows: vec![vec![Value::Null], vec![Value::Integer(3)]],
            })),
            "\
+------+
| age  |
+------+
| NULL |
| 3    |
+------+
2 rows returned"
        );
        assert_eq!(
            render_result(&StatementResult::Query(QueryResult {
                columns: vec!["name".to_string()],
                rows: Vec::new(),
            })),
            "\
+------+
| name |
+------+
0 rows returned"
        );
    }
}

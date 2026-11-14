//! Execute parsed statements against the catalog.
//!
//! Filters are bound and type-checked before any row is read. `UPDATE` and
//! `DELETE` build the replacement rows and install them only after every row
//! has been planned, so a failure leaves the table unchanged. A durable
//! database publishes that result only after the statement succeeds.
//!
//! `SELECT` applies `WHERE`, then `ORDER BY`, then `LIMIT`, then projection.
//! Sort keys may name columns that are not in the select list. Equal keys keep
//! insertion order because the sort is stable. NULL is greater than every
//! non-NULL value unless the query overrides that with `NULLS FIRST` or
//! `NULLS LAST`.

use std::cmp::Ordering;

use crate::ast::{Assignment, Expr, OrderKey, Projection, Statement};
use crate::database::{Database, DurableChange};
use crate::error::DbError;
use crate::expr::{self, BoundExpr};
use crate::parser;
use crate::table::Table;
use crate::value::Value;

/// The outcome of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementResult {
    /// `CREATE TABLE` or `DROP TABLE` succeeded.
    Ok,
    /// `INSERT` added this many rows.
    Insert {
        /// Number of rows stored.
        count: usize,
    },
    /// `UPDATE` matched this many rows.
    ///
    /// A row counts even when the assigned values equal the old ones.
    Update {
        /// Number of rows matched by `WHERE`.
        count: usize,
    },
    /// `DELETE` removed this many rows.
    Delete {
        /// Number of rows removed.
        count: usize,
    },
    /// `SELECT` produced these columns and rows.
    Query(QueryResult),
}

/// Rows returned by a `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    /// Output column names, in schema order for `SELECT *`.
    pub columns: Vec<String>,
    /// Output rows, in the order produced by the query.
    pub rows: Vec<Vec<Value>>,
}

struct BoundAssignment {
    index: usize,
    value: BoundExpr,
}

struct SortKey {
    index: usize,
    ascending: bool,
    nulls_first: bool,
}

impl Database {
    /// Parse `sql` and run every statement in order.
    ///
    /// Statements are autocommitted one by one. If a later statement fails,
    /// earlier statements in the same string stay applied. Each individual
    /// `INSERT`, `UPDATE`, or `DELETE` is atomic. A durable database writes a
    /// redo log and checkpoints it before reporting success. A failure before
    /// the commit record is synced restores memory when the log is still
    /// incomplete. A failed sync of that record does not prove the record is
    /// absent: memory stays, the log stays, and further statements are refused
    /// until the file is reopened. A failure after a successful log sync does
    /// the same.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        if let Some(error) = self.fenced() {
            return Err(error);
        }
        let statements = parser::parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_statement(statement)?);
        }
        Ok(results)
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult, DbError> {
        if !self.stores_on_disk() {
            return self.apply(statement);
        }
        let change = durable_change(&statement);
        let undo = self.prepare_durable(&change);
        let result = self.apply(statement)?;
        if matches!(change, DurableChange::None) {
            return Ok(result);
        }
        if let Err(error) = self.publish() {
            if let crate::storage::StepError::BeforeCommit(_) = &error {
                self.rollback(undo);
            }
            return Err(error.into_db());
        }
        Ok(result)
    }

    fn apply(&mut self, statement: Statement) -> Result<StatementResult, DbError> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.create_table(name, columns)?;
                Ok(StatementResult::Ok)
            }
            Statement::DropTable { names } => {
                self.drop_tables(&names)?;
                Ok(StatementResult::Ok)
            }
            Statement::CreateIndex {
                name,
                table,
                column,
            } => {
                self.create_index(name, table, column)?;
                Ok(StatementResult::Ok)
            }
            Statement::DropIndex { names } => {
                self.drop_indexes(&names)?;
                Ok(StatementResult::Ok)
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                let count = self.table_mut(&table)?.insert(&columns, rows)?;
                Ok(StatementResult::Insert { count })
            }
            Statement::Update {
                table,
                assignments,
                selection,
            } => self.update(&table, &assignments, selection.as_ref()),
            Statement::Delete { table, selection } => self.delete(&table, selection.as_ref()),
            Statement::Select {
                table,
                projection,
                selection,
                order_by,
                limit,
            } => self.query(&table, &projection, selection.as_ref(), &order_by, limit),
        }
    }

    fn query(
        &self,
        table_name: &str,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        let table = self.table(table_name)?;
        let output_indexes = output_columns(table, projection)?;
        let filter = bind_optional_where(selection, table)?;
        let sort_keys = bind_order(table, order_by)?;

        let mut rows: Vec<&[Value]> = Vec::new();
        for row in table.rows() {
            if expr::row_matches(filter.as_ref(), row)? {
                rows.push(row.as_slice());
            }
        }
        if !sort_keys.is_empty() {
            rows.sort_by(|left, right| compare_rows(left, right, &sort_keys));
        }
        if let Some(limit) = limit {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            if rows.len() > limit {
                rows.truncate(limit);
            }
        }

        let mut projected = Vec::with_capacity(rows.len());
        for row in rows {
            projected.push(project_row(row, &output_indexes)?);
        }
        let columns = column_names(table, &output_indexes)?;
        Ok(StatementResult::Query(QueryResult {
            columns,
            rows: projected,
        }))
    }

    fn update(
        &mut self,
        table_name: &str,
        assignments: &[Assignment],
        selection: Option<&Expr>,
    ) -> Result<StatementResult, DbError> {
        let (new_rows, count) = {
            let table = self.table(table_name)?;
            let filter = bind_optional_where(selection, table)?;
            let assignments = bind_assignments(table, assignments)?;
            let mut new_rows = Vec::with_capacity(table.rows().len());
            let mut count = 0usize;
            for row in table.rows() {
                if !expr::row_matches(filter.as_ref(), row)? {
                    new_rows.push(row.clone());
                    continue;
                }
                new_rows.push(assign_row(row, &assignments)?);
                count += 1;
            }
            (new_rows, count)
        };
        self.table_mut(table_name)?.rewrite_all(new_rows)?;
        Ok(StatementResult::Update { count })
    }

    fn delete(
        &mut self,
        table_name: &str,
        selection: Option<&Expr>,
    ) -> Result<StatementResult, DbError> {
        let (kept_indexes, deleted) = {
            let table = self.table(table_name)?;
            let filter = bind_optional_where(selection, table)?;
            let mut kept_indexes = Vec::new();
            let mut deleted = 0usize;
            for (index, row) in table.rows().iter().enumerate() {
                if expr::row_matches(filter.as_ref(), row)? {
                    deleted += 1;
                } else {
                    kept_indexes.push(index);
                }
            }
            (kept_indexes, deleted)
        };
        self.table_mut(table_name)?.retain_indexes(&kept_indexes)?;
        Ok(StatementResult::Delete { count: deleted })
    }
}

fn durable_change(statement: &Statement) -> DurableChange {
    match statement {
        Statement::CreateTable { name, .. } => DurableChange::Create(name.clone()),
        Statement::DropTable { names } => DurableChange::Drop(names.clone()),
        Statement::Insert { table, .. }
        | Statement::Update { table, .. }
        | Statement::Delete { table, .. } => DurableChange::Write(table.clone()),
        Statement::CreateIndex { .. } | Statement::DropIndex { .. } => DurableChange::Index,
        Statement::Select { .. } => DurableChange::None,
    }
}

fn bind_optional_where(
    selection: Option<&Expr>,
    table: &Table,
) -> Result<Option<BoundExpr>, DbError> {
    selection
        .map(|expr| expr::bind_where(expr, table))
        .transpose()
}

fn output_columns(table: &Table, projection: &Projection) -> Result<Vec<usize>, DbError> {
    match projection {
        Projection::All => Ok((0..table.schema().len()).collect()),
        Projection::Columns(columns) => columns
            .iter()
            .map(|column| expr::resolve_column(table, column))
            .collect(),
    }
}

fn column_names(table: &Table, indexes: &[usize]) -> Result<Vec<String>, DbError> {
    let mut names = Vec::with_capacity(indexes.len());
    for index in indexes {
        let Some(column) = table.schema().column(*index) else {
            return Err(DbError::Internal(format!(
                "output column index {index} is outside table '{}'",
                table.name()
            )));
        };
        names.push(column.name().to_string());
    }
    Ok(names)
}

fn project_row(row: &[Value], indexes: &[usize]) -> Result<Vec<Value>, DbError> {
    let mut projected = Vec::with_capacity(indexes.len());
    for index in indexes {
        let Some(value) = row.get(*index) else {
            return Err(DbError::Internal(format!(
                "row is missing column index {index}"
            )));
        };
        projected.push(value.clone());
    }
    Ok(projected)
}

fn bind_assignments(
    table: &Table,
    assignments: &[Assignment],
) -> Result<Vec<BoundAssignment>, DbError> {
    if assignments.is_empty() {
        return Err(DbError::invalid("UPDATE requires at least one assignment"));
    }
    let mut bound = Vec::with_capacity(assignments.len());
    let mut seen = vec![false; table.schema().len()];
    for assignment in assignments {
        let index = expr::resolve_column(table, &assignment.column)?;
        if seen[index] {
            return Err(DbError::invalid(format!(
                "column '{}' is assigned more than once in table '{}'",
                assignment.column.name,
                table.name()
            )));
        }
        seen[index] = true;
        let Some(column) = table.schema().column(index) else {
            return Err(DbError::Internal(format!(
                "assignment index {index} is outside table '{}'",
                table.name()
            )));
        };
        let value =
            expr::bind_assignment(&assignment.value, table, column.name(), column.data_type())?;
        bound.push(BoundAssignment { index, value });
    }
    Ok(bound)
}

/// Evaluate every right-hand side against `row`, then write them.
fn assign_row(row: &[Value], assignments: &[BoundAssignment]) -> Result<Vec<Value>, DbError> {
    let mut values = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        values.push((assignment.index, expr::eval(&assignment.value, row)?));
    }
    let mut updated = row.to_vec();
    for (index, value) in values {
        let Some(slot) = updated.get_mut(index) else {
            return Err(DbError::Internal(format!(
                "assignment index {index} is outside the row"
            )));
        };
        *slot = value;
    }
    Ok(updated)
}

fn bind_order(table: &Table, keys: &[OrderKey]) -> Result<Vec<SortKey>, DbError> {
    let mut bound = Vec::with_capacity(keys.len());
    for key in keys {
        let index = expr::resolve_column(table, &key.column)?;
        bound.push(SortKey {
            index,
            ascending: key.ascending,
            nulls_first: key.nulls_first.unwrap_or(!key.ascending),
        });
    }
    Ok(bound)
}

fn compare_rows(left: &[Value], right: &[Value], keys: &[SortKey]) -> Ordering {
    for key in keys {
        let ordering = compare_key(
            left.get(key.index),
            right.get(key.index),
            key.ascending,
            key.nulls_first,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_key(
    left: Option<&Value>,
    right: Option<&Value>,
    ascending: bool,
    nulls_first: bool,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_values(left, right, ascending, nulls_first),
        _ => Ordering::Equal,
    }
}

fn compare_values(left: &Value, right: &Value, ascending: bool, nulls_first: bool) -> Ordering {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ordering = match (left, right) {
                (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
                (Value::Text(left), Value::Text(right)) => left.cmp(right),
                (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
                _ => Ordering::Equal,
            };
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Database {
        let mut db = Database::new();
        db.execute("CREATE TABLE nums (n INT, label TEXT, active BOOLEAN);")
            .unwrap();
        db.execute(
            "INSERT INTO nums VALUES
                (1, 'a', true),
                (2, 'b', false),
                (3, 'c', true);",
        )
        .unwrap();
        db
    }

    fn query(db: &mut Database, sql: &str) -> QueryResult {
        match db.execute(sql).unwrap().pop().unwrap() {
            StatementResult::Query(result) => result,
            other => panic!("expected a query, got {other:?}"),
        }
    }

    #[test]
    fn and_binds_tighter_than_or() {
        let mut db = setup();
        let result = query(
            &mut db,
            "SELECT n FROM nums WHERE n = 1 OR n = 2 AND n = 3;",
        );
        assert_eq!(result.rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn parentheses_override_precedence() {
        let mut db = setup();
        let result = query(
            &mut db,
            "SELECT n FROM nums WHERE (n = 1 OR n = 2) AND active = FALSE;",
        );
        assert_eq!(result.rows, vec![vec![Value::Integer(2)]]);
    }

    #[test]
    fn type_errors_are_reported_before_scanning() {
        let mut db = Database::new();
        db.execute("CREATE TABLE empty (age INT);").unwrap();
        let error = db
            .execute("SELECT * FROM empty WHERE age > 'x';")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "cannot apply operator '>' to INTEGER and TEXT"
        );
    }
}

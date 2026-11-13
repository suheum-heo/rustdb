//! Execute parsed statements against an in-memory database.
//!
//! Filters are bound to column indexes and type-checked before any row is
//! read. A query against an empty table still reports a missing column or a
//! comparison between different types.

use crate::ast::{BinOp, Expr, Projection, SelectColumn, Statement};
use crate::database::Database;
use crate::error::DbError;
use crate::parser;
use crate::schema::Schema;
use crate::table::Table;
use crate::value::{DataType, Value};

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
    /// `SELECT` produced these columns and rows.
    Query(QueryResult),
}

/// Rows returned by a `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult {
    /// Output column names, in schema order for `SELECT *`.
    pub columns: Vec<String>,
    /// Output rows, in insertion order.
    pub rows: Vec<Vec<Value>>,
}

/// An expression whose columns are schema indexes and whose types check out.
#[derive(Debug)]
enum BoundExpr {
    Literal(Value),
    Column(usize),
    Binary {
        left: Box<BoundExpr>,
        op: BinOp,
        right: Box<BoundExpr>,
    },
}

impl Database {
    /// Parse `sql` and run every statement in order.
    ///
    /// Statements are autocommitted one by one. If a later statement fails,
    /// earlier statements in the same string stay applied.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        let statements = parser::parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_statement(statement)?);
        }
        Ok(results)
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult, DbError> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.create_table(name, columns)?;
                Ok(StatementResult::Ok)
            }
            Statement::DropTable { names } => {
                self.drop_tables(&names)?;
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
            Statement::Select {
                table,
                projection,
                selection,
            } => self.query(&table, &projection, selection.as_ref()),
        }
    }

    fn query(
        &self,
        table_name: &str,
        projection: &Projection,
        selection: Option<&Expr>,
    ) -> Result<StatementResult, DbError> {
        let table = self.table(table_name)?;
        let output_indexes = output_columns(table, projection)?;
        let filter = selection.map(|expr| bind_expr(expr, table)).transpose()?;
        if let Some(expr) = &filter {
            let data_type = bound_type(expr, table.schema())?;
            if data_type != DataType::Boolean {
                return Err(DbError::TypeError(format!(
                    "WHERE clause must evaluate to BOOLEAN, got {data_type}"
                )));
            }
        }

        let mut rows = Vec::new();
        for row in table.rows() {
            let keep = match &filter {
                Some(expr) => eval_bool(expr, row)?,
                None => true,
            };
            if keep {
                rows.push(project_row(row, &output_indexes)?);
            }
        }

        let mut columns = Vec::with_capacity(output_indexes.len());
        for index in output_indexes {
            let Some(column) = table.schema().column(index) else {
                return Err(DbError::Internal(format!(
                    "output column index {index} is outside table '{}'",
                    table.name()
                )));
            };
            columns.push(column.name().to_string());
        }

        Ok(StatementResult::Query(QueryResult { columns, rows }))
    }
}

fn output_columns(table: &Table, projection: &Projection) -> Result<Vec<usize>, DbError> {
    match projection {
        Projection::All => Ok((0..table.schema().len()).collect()),
        Projection::Columns(columns) => columns
            .iter()
            .map(|column| resolve_column(table, column))
            .collect(),
    }
}

fn resolve_column(table: &Table, column: &SelectColumn) -> Result<usize, DbError> {
    if let Some(qualifier) = &column.qualifier
        && qualifier != table.name()
    {
        return Err(DbError::invalid(format!(
            "table qualifier '{qualifier}' does not match '{}'",
            table.name()
        )));
    }
    table
        .schema()
        .index_of(&column.name)
        .ok_or_else(|| DbError::ColumnNotFound {
            table: table.name().to_string(),
            column: column.name.clone(),
        })
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

fn bind_expr(expr: &Expr, table: &Table) -> Result<BoundExpr, DbError> {
    match expr {
        Expr::Literal(value) => Ok(BoundExpr::Literal(value.clone())),
        Expr::Column { name, qualifier } => {
            let index = resolve_column(
                table,
                &SelectColumn {
                    name: name.clone(),
                    qualifier: qualifier.clone(),
                },
            )?;
            Ok(BoundExpr::Column(index))
        }
        Expr::Binary { left, op, right } => {
            let left = bind_expr(left, table)?;
            let right = bind_expr(right, table)?;
            check_binary(*op, &left, &right, table.schema())?;
            Ok(BoundExpr::Binary {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            })
        }
    }
}

fn check_binary(
    op: BinOp,
    left: &BoundExpr,
    right: &BoundExpr,
    schema: &Schema,
) -> Result<(), DbError> {
    let left_type = bound_type(left, schema)?;
    let right_type = bound_type(right, schema)?;
    if op.is_logical() {
        if left_type != DataType::Boolean {
            return Err(logical_operand(op, left_type));
        }
        if right_type != DataType::Boolean {
            return Err(logical_operand(op, right_type));
        }
        return Ok(());
    }
    if left_type != right_type {
        return Err(DbError::TypeError(format!(
            "cannot apply operator '{op}' to {left_type} and {right_type}",
            op = op.as_str()
        )));
    }
    if op.is_ordering() && !matches!(left_type, DataType::Integer | DataType::Text) {
        return Err(DbError::TypeError(format!(
            "operator '{}' is not supported for {left_type}",
            op.as_str()
        )));
    }
    Ok(())
}

fn logical_operand(op: BinOp, found: DataType) -> DbError {
    DbError::TypeError(format!(
        "operator '{}' requires BOOLEAN operands, got {found}",
        op.as_str()
    ))
}

fn bound_type(expr: &BoundExpr, schema: &Schema) -> Result<DataType, DbError> {
    match expr {
        BoundExpr::Literal(value) => Ok(value.data_type()),
        BoundExpr::Column(index) => {
            let Some(column) = schema.column(*index) else {
                return Err(DbError::Internal(format!(
                    "bound column index {index} is outside the schema"
                )));
            };
            Ok(column.data_type())
        }
        BoundExpr::Binary { .. } => Ok(DataType::Boolean),
    }
}

fn eval(expr: &BoundExpr, row: &[Value]) -> Result<Value, DbError> {
    match expr {
        BoundExpr::Literal(value) => Ok(value.clone()),
        BoundExpr::Column(index) => row.get(*index).cloned().ok_or_else(|| {
            DbError::Internal(format!(
                "evaluation column index {index} is outside the row"
            ))
        }),
        BoundExpr::Binary { left, op, right } => {
            let left = eval(left, row)?;
            let right = eval(right, row)?;
            eval_binary(*op, &left, &right)
        }
    }
}

fn eval_bool(expr: &BoundExpr, row: &[Value]) -> Result<bool, DbError> {
    match eval(expr, row)? {
        Value::Boolean(value) => Ok(value),
        other => Err(DbError::TypeError(format!(
            "WHERE clause must evaluate to BOOLEAN, got {}",
            other.data_type()
        ))),
    }
}

fn eval_binary(op: BinOp, left: &Value, right: &Value) -> Result<Value, DbError> {
    if op.is_logical() {
        let (Value::Boolean(left), Value::Boolean(right)) = (left, right) else {
            return Err(DbError::Internal(
                "logical operator evaluated non-boolean values".to_string(),
            ));
        };
        let result = match op {
            BinOp::And => *left && *right,
            BinOp::Or => *left || *right,
            _ => {
                return Err(DbError::Internal(
                    "comparison operator reached logical evaluation".to_string(),
                ));
            }
        };
        return Ok(Value::Boolean(result));
    }

    if left.data_type() != right.data_type() {
        return Err(DbError::TypeError(format!(
            "cannot apply operator '{}' to {} and {}",
            op.as_str(),
            left.data_type(),
            right.data_type()
        )));
    }

    let ordering = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => {
            return Err(DbError::Internal(
                "comparison values changed type".to_string(),
            ));
        }
    };

    if op.is_ordering() && matches!(left, Value::Boolean(_)) {
        return Err(DbError::TypeError(format!(
            "operator '{}' is not supported for BOOLEAN",
            op.as_str()
        )));
    }

    let result = match op {
        BinOp::Eq => ordering.is_eq(),
        BinOp::NotEq => ordering.is_ne(),
        BinOp::Lt => ordering.is_lt(),
        BinOp::Gt => ordering.is_gt(),
        BinOp::LtEq => ordering.is_le(),
        BinOp::GtEq => ordering.is_ge(),
        BinOp::And | BinOp::Or => {
            return Err(DbError::Internal(
                "logical operator reached comparison evaluation".to_string(),
            ));
        }
    };
    Ok(Value::Boolean(result))
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

//! Bind and evaluate scalar expressions.
//!
//! `SELECT`, `UPDATE`, and `DELETE` all filter through [`row_matches`], so
//! three-valued logic lives in one place. Binding checks types before any row
//! is read. A null literal has no type and is compatible with every column;
//! a column reference keeps the column's declared type even when the stored
//! value is null.

use crate::ast::{BinOp, Expr, SelectColumn};
use crate::error::DbError;
use crate::schema::Schema;
use crate::table::Table;
use crate::value::{DataType, Value};

/// The type of a bound expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprType {
    /// An untyped `NULL` literal.
    Null,
    /// A column or a non-null literal.
    Value(DataType),
}

/// An expression whose columns are schema indexes.
#[derive(Debug)]
pub(crate) enum BoundExpr {
    Literal(Value),
    Column(usize),
    Not(Box<BoundExpr>),
    IsNull(Box<BoundExpr>),
    IsNotNull(Box<BoundExpr>),
    Binary {
        left: Box<BoundExpr>,
        op: BinOp,
        right: Box<BoundExpr>,
    },
}

/// Resolve `column` against `table`.
pub(crate) fn resolve_column(table: &Table, column: &SelectColumn) -> Result<usize, DbError> {
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

/// Bind a `WHERE` expression. It must be boolean, or an untyped null literal.
pub(crate) fn bind_where(expr: &Expr, table: &Table) -> Result<BoundExpr, DbError> {
    let bound = bind(expr, table)?;
    match expr_type(&bound, table.schema())? {
        ExprType::Null | ExprType::Value(DataType::Boolean) => Ok(bound),
        ExprType::Value(found) => Err(DbError::TypeError(format!(
            "WHERE clause must evaluate to BOOLEAN, got {found}"
        ))),
    }
}

/// Bind an assignment value. Null is compatible with every column type.
pub(crate) fn bind_assignment(
    expr: &Expr,
    table: &Table,
    column: &str,
    expected: DataType,
) -> Result<BoundExpr, DbError> {
    let bound = bind(expr, table)?;
    match expr_type(&bound, table.schema())? {
        ExprType::Null => Ok(bound),
        ExprType::Value(actual) if actual == expected => Ok(bound),
        ExprType::Value(actual) => Err(DbError::TypeMismatch {
            column: column.to_string(),
            expected,
            actual,
        }),
    }
}

/// Keep a row only when `filter` is absent or evaluates to TRUE.
///
/// FALSE and UNKNOWN both reject the row.
pub(crate) fn row_matches(filter: Option<&BoundExpr>, row: &[Value]) -> Result<bool, DbError> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    Ok(matches!(eval(filter, row)?, Value::Boolean(true)))
}

/// Evaluate `expr` for one row.
pub(crate) fn eval(expr: &BoundExpr, row: &[Value]) -> Result<Value, DbError> {
    match expr {
        BoundExpr::Literal(value) => Ok(value.clone()),
        BoundExpr::Column(index) => row.get(*index).cloned().ok_or_else(|| {
            DbError::Internal(format!(
                "evaluation column index {index} is outside the row"
            ))
        }),
        BoundExpr::Not(inner) => eval_not(eval(inner, row)?),
        BoundExpr::IsNull(inner) => Ok(Value::Boolean(eval(inner, row)?.is_null())),
        BoundExpr::IsNotNull(inner) => Ok(Value::Boolean(!eval(inner, row)?.is_null())),
        BoundExpr::Binary { left, op, right } => {
            let left = eval(left, row)?;
            let right = eval(right, row)?;
            eval_binary(*op, &left, &right)
        }
    }
}

fn bind(expr: &Expr, table: &Table) -> Result<BoundExpr, DbError> {
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
        Expr::Not(inner) => {
            let inner = bind(inner, table)?;
            expect_boolean("NOT", expr_type(&inner, table.schema())?)?;
            Ok(BoundExpr::Not(Box::new(inner)))
        }
        Expr::IsNull(inner) => Ok(BoundExpr::IsNull(Box::new(bind(inner, table)?))),
        Expr::IsNotNull(inner) => Ok(BoundExpr::IsNotNull(Box::new(bind(inner, table)?))),
        Expr::Binary { left, op, right } => {
            let left = bind(left, table)?;
            let right = bind(right, table)?;
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
    let left_type = expr_type(left, schema)?;
    let right_type = expr_type(right, schema)?;
    if op.is_logical() {
        expect_boolean(op.as_str(), left_type)?;
        expect_boolean(op.as_str(), right_type)?;
        return Ok(());
    }

    let resolved = match (left_type, right_type) {
        (ExprType::Null, ExprType::Null) => None,
        (ExprType::Null, ExprType::Value(data_type))
        | (ExprType::Value(data_type), ExprType::Null) => Some(data_type),
        (ExprType::Value(left), ExprType::Value(right)) if left == right => Some(left),
        (ExprType::Value(left), ExprType::Value(right)) => {
            return Err(DbError::TypeError(format!(
                "cannot apply operator '{}' to {left} and {right}",
                op.as_str()
            )));
        }
    };
    if op.is_ordering() && matches!(resolved, Some(DataType::Boolean)) {
        return Err(DbError::TypeError(format!(
            "operator '{}' is not supported for BOOLEAN",
            op.as_str()
        )));
    }
    Ok(())
}

fn expect_boolean(op: &str, found: ExprType) -> Result<(), DbError> {
    match found {
        ExprType::Null | ExprType::Value(DataType::Boolean) => Ok(()),
        ExprType::Value(found) => Err(DbError::TypeError(format!(
            "operator '{op}' requires BOOLEAN operands, got {found}"
        ))),
    }
}

fn expr_type(expr: &BoundExpr, schema: &Schema) -> Result<ExprType, DbError> {
    match expr {
        BoundExpr::Literal(Value::Null) => Ok(ExprType::Null),
        BoundExpr::Literal(value) => {
            Ok(ExprType::Value(value.data_type().ok_or_else(|| {
                DbError::Internal("non-null literal has no type".to_string())
            })?))
        }
        BoundExpr::Column(index) => {
            let Some(column) = schema.column(*index) else {
                return Err(DbError::Internal(format!(
                    "bound column index {index} is outside the schema"
                )));
            };
            Ok(ExprType::Value(column.data_type()))
        }
        BoundExpr::Not(_)
        | BoundExpr::IsNull(_)
        | BoundExpr::IsNotNull(_)
        | BoundExpr::Binary { .. } => Ok(ExprType::Value(DataType::Boolean)),
    }
}

fn eval_not(value: Value) -> Result<Value, DbError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Boolean(value) => Ok(Value::Boolean(!value)),
        other => Err(DbError::Internal(format!(
            "NOT evaluated a {} value",
            other
                .data_type()
                .map(|data_type| data_type.to_string())
                .unwrap_or_else(|| "NULL".to_string())
        ))),
    }
}

fn eval_binary(op: BinOp, left: &Value, right: &Value) -> Result<Value, DbError> {
    if op.is_logical() {
        return Ok(truth_value(match op {
            BinOp::And => and(as_truth(left)?, as_truth(right)?),
            BinOp::Or => or(as_truth(left)?, as_truth(right)?),
            _ => {
                return Err(DbError::Internal(
                    "comparison operator reached logical evaluation".to_string(),
                ));
            }
        }));
    }

    if left.is_null() || right.is_null() {
        return Ok(Value::Null);
    }

    let ordering = match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Text(left), Value::Text(right)) => left.cmp(right),
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        _ => {
            return Err(DbError::Internal(
                "comparison values have different types".to_string(),
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

/// `None` is UNKNOWN.
fn as_truth(value: &Value) -> Result<Option<bool>, DbError> {
    match value {
        Value::Null => Ok(None),
        Value::Boolean(value) => Ok(Some(*value)),
        other => Err(DbError::Internal(format!(
            "boolean operator evaluated a non-boolean {}",
            other
                .data_type()
                .map(|data_type| data_type.to_string())
                .unwrap_or_else(|| "NULL".to_string())
        ))),
    }
}

fn truth_value(value: Option<bool>) -> Value {
    match value {
        Some(value) => Value::Boolean(value),
        None => Value::Null,
    }
}

fn and(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(value: Value) -> BoundExpr {
        BoundExpr::Literal(value)
    }

    fn boolean(value: Option<bool>) -> Value {
        match value {
            Some(value) => Value::Boolean(value),
            None => Value::Null,
        }
    }

    fn binary(op: BinOp, left: Option<bool>, right: Option<bool>) -> Value {
        eval(
            &BoundExpr::Binary {
                left: Box::new(literal(boolean(left))),
                op,
                right: Box::new(literal(boolean(right))),
            },
            &[],
        )
        .unwrap()
    }

    #[test]
    fn and_uses_sql_truth_table() {
        let cases = [
            (Some(true), Some(true), Some(true)),
            (Some(true), Some(false), Some(false)),
            (Some(true), None, None),
            (Some(false), Some(true), Some(false)),
            (Some(false), Some(false), Some(false)),
            (Some(false), None, Some(false)),
            (None, Some(true), None),
            (None, Some(false), Some(false)),
            (None, None, None),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                binary(BinOp::And, left, right),
                boolean(expected),
                "AND {left:?} {right:?}"
            );
        }
    }

    #[test]
    fn or_uses_sql_truth_table() {
        let cases = [
            (Some(true), Some(true), Some(true)),
            (Some(true), Some(false), Some(true)),
            (Some(true), None, Some(true)),
            (Some(false), Some(true), Some(true)),
            (Some(false), Some(false), Some(false)),
            (Some(false), None, None),
            (None, Some(true), Some(true)),
            (None, Some(false), None),
            (None, None, None),
        ];
        for (left, right, expected) in cases {
            assert_eq!(
                binary(BinOp::Or, left, right),
                boolean(expected),
                "OR {left:?} {right:?}"
            );
        }
    }

    #[test]
    fn not_and_comparisons_propagate_unknown() {
        assert_eq!(
            eval_not(Value::Boolean(true)).unwrap(),
            Value::Boolean(false)
        );
        assert_eq!(
            eval_not(Value::Boolean(false)).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(eval_not(Value::Null).unwrap(), Value::Null);

        let unknown = eval(
            &BoundExpr::Binary {
                left: Box::new(literal(Value::Null)),
                op: BinOp::Eq,
                right: Box::new(literal(Value::Integer(1))),
            },
            &[],
        )
        .unwrap();
        assert_eq!(unknown, Value::Null);
        assert!(!row_matches(Some(&literal(Value::Null)), &[]).unwrap());
        assert!(row_matches(Some(&literal(Value::Boolean(true))), &[]).unwrap());
    }

    #[test]
    fn is_null_is_never_unknown() {
        assert_eq!(
            eval(&BoundExpr::IsNull(Box::new(literal(Value::Null))), &[]).unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval(
                &BoundExpr::IsNotNull(Box::new(literal(Value::Integer(1)))),
                &[]
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(
            eval(
                &BoundExpr::IsNull(Box::new(literal(Value::Text("a".to_string())))),
                &[]
            )
            .unwrap(),
            Value::Boolean(false)
        );
    }
}

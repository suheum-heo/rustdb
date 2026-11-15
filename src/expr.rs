//! Bind and evaluate scalar expressions.
//!
//! `SELECT`, `UPDATE`, and `DELETE` all filter through [`row_matches`], so
//! three-valued logic lives in one place. Binding checks types before any row
//! is read. A null literal has no type and is compatible with every column;
//! a column reference keeps the column's declared type even when the stored
//! value is null.

use std::fmt;

use crate::ast::{AggregateCall, BinOp, Expr, SelectColumn};
use crate::error::DbError;
use crate::table::Table;
use crate::value::{self, DataType, Value};

/// One input of a statement, in the order its columns occupy in a joined row.
struct Relation {
    alias: String,
    offset: usize,
    columns: Vec<(String, DataType)>,
}

/// Columns a statement can name, laid out outer-then-inner.
///
/// A qualifier matches an alias, or the catalog name when the query wrote no
/// alias. An unqualified name must match exactly one column. Binding stores
/// the flat index, so later evaluation does not look names up again.
pub(crate) struct Scope {
    relations: Vec<Relation>,
    types: Vec<DataType>,
}

impl Scope {
    /// One table. `alias` is the catalog name when the query has no alias.
    pub(crate) fn from_table(table: &Table, alias: &str) -> Self {
        Self::from_parts(vec![(alias.to_string(), column_list(table))])
    }

    /// Two tables. The same visible name on both sides is an error.
    ///
    /// Column lists are owned so the caller can read each table separately,
    /// including a self-join of one catalog table.
    pub(crate) fn try_join(
        outer_alias: &str,
        outer_columns: Vec<(String, DataType)>,
        inner_alias: &str,
        inner_columns: Vec<(String, DataType)>,
    ) -> Result<Self, DbError> {
        if outer_alias == inner_alias {
            return Err(DbError::invalid(format!(
                "duplicate table alias '{outer_alias}'"
            )));
        }
        Ok(Self::from_parts(vec![
            (outer_alias.to_string(), outer_columns),
            (inner_alias.to_string(), inner_columns),
        ]))
    }

    pub(crate) fn columns(table: &Table) -> Vec<(String, DataType)> {
        column_list(table)
    }

    fn from_parts(parts: Vec<(String, Vec<(String, DataType)>)>) -> Self {
        let mut relations = Vec::with_capacity(parts.len());
        let mut types = Vec::new();
        for (alias, columns) in parts {
            let offset = types.len();
            types.extend(columns.iter().map(|(_, data_type)| *data_type));
            relations.push(Relation {
                alias,
                offset,
                columns,
            });
        }
        Self { relations, types }
    }

    pub(crate) fn len(&self) -> usize {
        self.types.len()
    }

    pub(crate) fn type_at(&self, index: usize) -> Option<DataType> {
        self.types.get(index).copied()
    }

    pub(crate) fn name_at(&self, index: usize) -> Option<&str> {
        for relation in &self.relations {
            let Some(local) = index.checked_sub(relation.offset) else {
                continue;
            };
            if local < relation.columns.len() {
                return Some(relation.columns[local].0.as_str());
            }
        }
        None
    }

    /// Indexes of every column belonging to `qualifier`, in declared order.
    pub(crate) fn columns_of(&self, qualifier: &str) -> Result<Vec<usize>, DbError> {
        let relation = self
            .relations
            .iter()
            .find(|relation| relation.alias == qualifier)
            .ok_or_else(|| self.unknown_qualifier(qualifier))?;
        Ok((0..relation.columns.len())
            .map(|local| relation.offset + local)
            .collect())
    }

    /// Resolve `column` to an index in the flat row.
    pub(crate) fn resolve(&self, column: &SelectColumn) -> Result<usize, DbError> {
        if let Some(qualifier) = &column.qualifier {
            let relation = self
                .relations
                .iter()
                .find(|relation| relation.alias == *qualifier)
                .ok_or_else(|| self.unknown_qualifier(qualifier))?;
            let local = relation
                .columns
                .iter()
                .position(|(name, _)| name == &column.name)
                .ok_or_else(|| DbError::ColumnNotFound {
                    table: qualifier.clone(),
                    column: column.name.clone(),
                })?;
            return Ok(relation.offset + local);
        }

        let mut found = None;
        for relation in &self.relations {
            if let Some(local) = relation
                .columns
                .iter()
                .position(|(name, _)| name == &column.name)
            {
                if found.is_some() {
                    return Err(DbError::invalid(format!(
                        "column '{}' is ambiguous",
                        column.name
                    )));
                }
                found = Some(relation.offset + local);
            }
        }
        found.ok_or_else(|| {
            if self.relations.len() == 1 {
                DbError::ColumnNotFound {
                    table: self.relations[0].alias.clone(),
                    column: column.name.clone(),
                }
            } else {
                DbError::invalid(format!("column '{}' does not exist", column.name))
            }
        })
    }

    fn unknown_qualifier(&self, qualifier: &str) -> DbError {
        if self.relations.len() == 1 {
            DbError::invalid(format!(
                "table qualifier '{qualifier}' does not match '{}'",
                self.relations[0].alias
            ))
        } else {
            DbError::invalid(format!("table '{qualifier}' is not in scope"))
        }
    }
}

fn column_list(table: &Table) -> Vec<(String, DataType)> {
    table
        .schema()
        .columns()
        .iter()
        .map(|column| (column.name().to_string(), column.data_type()))
        .collect()
}

/// A type used while binding an expression.
///
/// [`DataType`] is what a table can store. [`ScalarType::Ratio`] exists only
/// for an `AVG` result. It is never a column type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScalarType {
    /// Signed 64-bit integer.
    Integer,
    /// Text.
    Text,
    /// Boolean.
    Boolean,
    /// Exact ratio produced by `AVG`.
    Ratio,
}

impl From<DataType> for ScalarType {
    fn from(data_type: DataType) -> Self {
        match data_type {
            DataType::Integer => Self::Integer,
            DataType::Text => Self::Text,
            DataType::Boolean => Self::Boolean,
        }
    }
}

impl fmt::Display for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Boolean => "BOOLEAN",
            Self::Ratio => "RATIO",
        })
    }
}

/// The type of a bound expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprType {
    /// An untyped `NULL` literal.
    Null,
    /// A column, a non-null literal, or an aggregate result.
    Value(ScalarType),
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

/// Bind a filter. It must be boolean, or an untyped null literal.
///
/// `ON` and `WHERE` both use this. The caller names the clause in the error
/// when the context is not `WHERE`.
pub(crate) fn bind_filter(expr: &Expr, scope: &Scope, clause: &str) -> Result<BoundExpr, DbError> {
    let bound = bind(expr, scope)?;
    match expr_type(&bound, TypeList::Schema(&scope.types))? {
        ExprType::Null | ExprType::Value(ScalarType::Boolean) => Ok(bound),
        ExprType::Value(found) => Err(DbError::TypeError(format!(
            "{clause} clause must evaluate to BOOLEAN, got {found}"
        ))),
    }
}

/// Bind a `WHERE` expression. It must be boolean, or an untyped null literal.
pub(crate) fn bind_where(expr: &Expr, scope: &Scope) -> Result<BoundExpr, DbError> {
    bind_filter(expr, scope, "WHERE")
}

/// Bind `HAVING` against one completed group.
///
/// `column` returns the group's slot and the column's declared type.
/// `aggregate` returns the slot of a shared accumulator and that slot's
/// result type. `COUNT` and `SUM` are integers. `AVG` is a ratio. `MIN` and
/// `MAX` keep the argument type. The clause must be boolean, or an untyped
/// null literal.
pub(crate) fn bind_having(
    expr: &Expr,
    mut column: impl FnMut(&SelectColumn) -> Result<(usize, DataType), DbError>,
    mut aggregate: impl FnMut(&AggregateCall) -> Result<(usize, ScalarType), DbError>,
) -> Result<BoundExpr, DbError> {
    let mut types = Vec::new();
    let bound = bind_having_expr(expr, &mut column, &mut aggregate, &mut types)?;
    match expr_type(&bound, TypeList::Scalar(&types))? {
        ExprType::Null | ExprType::Value(ScalarType::Boolean) => Ok(bound),
        ExprType::Value(found) => Err(DbError::TypeError(format!(
            "HAVING clause must evaluate to BOOLEAN, got {found}"
        ))),
    }
}

type GroupColumnFn<'a> = dyn FnMut(&SelectColumn) -> Result<(usize, DataType), DbError> + 'a;
type GroupAggregateFn<'a> = dyn FnMut(&AggregateCall) -> Result<(usize, ScalarType), DbError> + 'a;

fn bind_having_expr(
    expr: &Expr,
    column: &mut GroupColumnFn<'_>,
    aggregate: &mut GroupAggregateFn<'_>,
    types: &mut Vec<ScalarType>,
) -> Result<BoundExpr, DbError> {
    match expr {
        Expr::Literal(value) => Ok(BoundExpr::Literal(value.clone())),
        Expr::Column { name, qualifier } => {
            let (index, data_type) = column(&SelectColumn {
                name: name.clone(),
                qualifier: qualifier.clone(),
            })?;
            note_type(types, index, ScalarType::from(data_type));
            Ok(BoundExpr::Column(index))
        }
        Expr::Aggregate(call) => {
            let (index, scalar) = aggregate(call)?;
            note_type(types, index, scalar);
            Ok(BoundExpr::Column(index))
        }
        Expr::Not(inner) => {
            let inner = bind_having_expr(inner, column, aggregate, types)?;
            expect_boolean("NOT", expr_type(&inner, TypeList::Scalar(types))?)?;
            Ok(BoundExpr::Not(Box::new(inner)))
        }
        Expr::IsNull(inner) => Ok(BoundExpr::IsNull(Box::new(bind_having_expr(
            inner, column, aggregate, types,
        )?))),
        Expr::IsNotNull(inner) => Ok(BoundExpr::IsNotNull(Box::new(bind_having_expr(
            inner, column, aggregate, types,
        )?))),
        Expr::Binary { left, op, right } => {
            let left = bind_having_expr(left, column, aggregate, types)?;
            let right = bind_having_expr(right, column, aggregate, types)?;
            check_binary(*op, &left, &right, TypeList::Scalar(types))?;
            Ok(BoundExpr::Binary {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            })
        }
    }
}

fn note_type(types: &mut Vec<ScalarType>, index: usize, data_type: ScalarType) {
    if types.len() <= index {
        types.resize(index + 1, ScalarType::Integer);
    }
    types[index] = data_type;
}

/// Bind an assignment value. Null is compatible with every column type.
pub(crate) fn bind_assignment(
    expr: &Expr,
    scope: &Scope,
    column: &str,
    expected: DataType,
) -> Result<BoundExpr, DbError> {
    let bound = bind(expr, scope)?;
    match expr_type(&bound, TypeList::Schema(&scope.types))? {
        ExprType::Null => Ok(bound),
        ExprType::Value(ScalarType::Ratio) => {
            Err(DbError::Internal("ratio values are not stored".to_string()))
        }
        ExprType::Value(actual) if scalar_data_type(actual) == Some(expected) => Ok(bound),
        ExprType::Value(actual) => Err(DbError::TypeMismatch {
            column: column.to_string(),
            expected,
            actual: scalar_data_type(actual).unwrap_or(DataType::Integer),
        }),
    }
}

fn scalar_data_type(scalar: ScalarType) -> Option<DataType> {
    match scalar {
        ScalarType::Integer => Some(DataType::Integer),
        ScalarType::Text => Some(DataType::Text),
        ScalarType::Boolean => Some(DataType::Boolean),
        ScalarType::Ratio => None,
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
    eval_in(expr, &FlatRow::One(row))
}

/// Evaluate `expr` for one outer row joined to one inner row.
///
/// Indexes below `outer.len()` read the outer row. The rest read the inner row.
pub(crate) fn eval_pair(
    expr: &BoundExpr,
    outer: &[Value],
    inner: &[Value],
) -> Result<Value, DbError> {
    eval_in(expr, &FlatRow::Two(outer, inner))
}

/// Keep a joined pair only when `filter` evaluates to TRUE.
pub(crate) fn pair_matches(
    filter: Option<&BoundExpr>,
    outer: &[Value],
    inner: &[Value],
) -> Result<bool, DbError> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    Ok(matches!(
        eval_pair(filter, outer, inner)?,
        Value::Boolean(true)
    ))
}

enum FlatRow<'a> {
    One(&'a [Value]),
    Two(&'a [Value], &'a [Value]),
}

impl FlatRow<'_> {
    fn get(&self, index: usize) -> Option<&Value> {
        match self {
            Self::One(row) => row.get(index),
            Self::Two(outer, inner) => {
                if index < outer.len() {
                    outer.get(index)
                } else {
                    inner.get(index - outer.len())
                }
            }
        }
    }
}

fn eval_in(expr: &BoundExpr, row: &FlatRow<'_>) -> Result<Value, DbError> {
    match expr {
        BoundExpr::Literal(value) => Ok(value.clone()),
        BoundExpr::Column(index) => row.get(*index).cloned().ok_or_else(|| {
            DbError::Internal(format!(
                "evaluation column index {index} is outside the row"
            ))
        }),
        BoundExpr::Not(inner) => eval_not(eval_in(inner, row)?),
        BoundExpr::IsNull(inner) => Ok(Value::Boolean(eval_in(inner, row)?.is_null())),
        BoundExpr::IsNotNull(inner) => Ok(Value::Boolean(!eval_in(inner, row)?.is_null())),
        BoundExpr::Binary { left, op, right } => {
            let left = eval_in(left, row)?;
            let right = eval_in(right, row)?;
            eval_binary(*op, &left, &right)
        }
    }
}

fn bind(expr: &Expr, scope: &Scope) -> Result<BoundExpr, DbError> {
    match expr {
        Expr::Literal(value) => Ok(BoundExpr::Literal(value.clone())),
        Expr::Column { name, qualifier } => {
            let index = scope.resolve(&SelectColumn {
                name: name.clone(),
                qualifier: qualifier.clone(),
            })?;
            Ok(BoundExpr::Column(index))
        }
        Expr::Not(inner) => {
            let inner = bind(inner, scope)?;
            expect_boolean("NOT", expr_type(&inner, TypeList::Schema(&scope.types))?)?;
            Ok(BoundExpr::Not(Box::new(inner)))
        }
        Expr::IsNull(inner) => Ok(BoundExpr::IsNull(Box::new(bind(inner, scope)?))),
        Expr::IsNotNull(inner) => Ok(BoundExpr::IsNotNull(Box::new(bind(inner, scope)?))),
        Expr::Binary { left, op, right } => {
            let left = bind(left, scope)?;
            let right = bind(right, scope)?;
            check_binary(*op, &left, &right, TypeList::Schema(&scope.types))?;
            Ok(BoundExpr::Binary {
                left: Box::new(left),
                op: *op,
                right: Box::new(right),
            })
        }
        Expr::Aggregate(_) => Err(DbError::unsupported(
            "aggregates are not allowed in this expression",
        )),
    }
}

#[derive(Clone, Copy)]
enum TypeList<'a> {
    Schema(&'a [DataType]),
    Scalar(&'a [ScalarType]),
}

fn check_binary(
    op: BinOp,
    left: &BoundExpr,
    right: &BoundExpr,
    types: TypeList<'_>,
) -> Result<(), DbError> {
    let left_type = expr_type(left, types)?;
    let right_type = expr_type(right, types)?;
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
        (ExprType::Value(ScalarType::Integer), ExprType::Value(ScalarType::Ratio))
        | (ExprType::Value(ScalarType::Ratio), ExprType::Value(ScalarType::Integer)) => {
            Some(ScalarType::Ratio)
        }
        (ExprType::Value(left), ExprType::Value(right)) => {
            return Err(DbError::TypeError(format!(
                "cannot apply operator '{}' to {left} and {right}",
                op.as_str()
            )));
        }
    };
    if op.is_ordering() && matches!(resolved, Some(ScalarType::Boolean)) {
        return Err(DbError::TypeError(format!(
            "operator '{}' is not supported for BOOLEAN",
            op.as_str()
        )));
    }
    Ok(())
}

fn expect_boolean(op: &str, found: ExprType) -> Result<(), DbError> {
    match found {
        ExprType::Null | ExprType::Value(ScalarType::Boolean) => Ok(()),
        ExprType::Value(found) => Err(DbError::TypeError(format!(
            "operator '{op}' requires BOOLEAN operands, got {found}"
        ))),
    }
}

fn expr_type(expr: &BoundExpr, types: TypeList<'_>) -> Result<ExprType, DbError> {
    match expr {
        BoundExpr::Literal(Value::Null) => Ok(ExprType::Null),
        BoundExpr::Literal(Value::Ratio { .. }) => Ok(ExprType::Value(ScalarType::Ratio)),
        BoundExpr::Literal(value) => Ok(ExprType::Value(ScalarType::from(
            value
                .data_type()
                .ok_or_else(|| DbError::Internal("non-null literal has no type".to_string()))?,
        ))),
        BoundExpr::Column(index) => {
            let Some(data_type) = lookup_type(types, *index) else {
                return Err(DbError::Internal(format!(
                    "bound column index {index} is outside the schema"
                )));
            };
            Ok(ExprType::Value(data_type))
        }
        BoundExpr::Not(_)
        | BoundExpr::IsNull(_)
        | BoundExpr::IsNotNull(_)
        | BoundExpr::Binary { .. } => Ok(ExprType::Value(ScalarType::Boolean)),
    }
}

fn lookup_type(types: TypeList<'_>, index: usize) -> Option<ScalarType> {
    match types {
        TypeList::Schema(types) => types.get(index).copied().map(ScalarType::from),
        TypeList::Scalar(types) => types.get(index).copied(),
    }
}

fn eval_not(value: Value) -> Result<Value, DbError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Boolean(value) => Ok(Value::Boolean(!value)),
        other => Err(DbError::Internal(format!(
            "NOT evaluated a {} value",
            scalar_name(&other)
        ))),
    }
}

fn scalar_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Text(_) => "TEXT",
        Value::Boolean(_) => "BOOLEAN",
        Value::Ratio { .. } => "RATIO",
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

    let Some(ordering) = value::cmp_scalar(left, right) else {
        return Err(DbError::Internal(
            "comparison values have different types".to_string(),
        ));
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
            scalar_name(other)
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

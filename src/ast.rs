//! Statements and expressions the executor understands.
//!
//! `sqlparser` accepts far more SQL than this engine runs. The parser lowers
//! the supported subset into these types and rejects everything else, so the
//! executor never has to guess about an unhandled clause.

use crate::schema::Column;
use crate::value::Value;

/// One statement after parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Statement {
    /// `CREATE TABLE`.
    CreateTable {
        /// Normalized table name.
        name: String,
        /// Columns in declared order.
        columns: Vec<Column>,
    },
    /// `DROP TABLE`, possibly with several names.
    DropTable {
        /// Normalized table names, in source order.
        names: Vec<String>,
    },
    /// `CREATE INDEX` on one INTEGER column.
    CreateIndex {
        /// Normalized index name.
        name: String,
        /// Normalized table name.
        table: String,
        /// Normalized column name.
        column: String,
    },
    /// `DROP INDEX`, possibly with several names.
    DropIndex {
        /// Normalized index names, in source order.
        names: Vec<String>,
    },
    /// `INSERT INTO ... VALUES`.
    Insert {
        /// Normalized table name.
        table: String,
        /// Normalized target columns. Empty means every column, in order.
        columns: Vec<String>,
        /// One value list per inserted row.
        rows: Vec<Vec<Value>>,
    },
    /// `UPDATE`.
    Update {
        /// Normalized table name.
        table: String,
        /// Assignments, in source order. Evaluated against the original row.
        assignments: Vec<Assignment>,
        /// Optional filter. Absent means every row.
        selection: Option<Expr>,
    },
    /// `DELETE FROM`.
    Delete {
        /// Normalized table name.
        table: String,
        /// Optional filter. Absent means every row.
        selection: Option<Expr>,
    },
    /// A `SELECT` from one table, or an `INNER JOIN` of two tables.
    ///
    /// Boxed so the query tree does not inflate every other statement.
    Select(Box<SelectQuery>),
    /// `EXPLAIN` of one `SELECT`. The query is planned and not executed.
    Explain(Box<Statement>),
}

/// One `SELECT`, including grouping and `HAVING`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectQuery {
    /// Input tables.
    pub(crate) from: QueryFrom,
    /// Output columns.
    pub(crate) projection: Projection,
    /// Optional filter. Absent means every joined row. Applied after `ON`
    /// and before grouping.
    pub(crate) selection: Option<Expr>,
    /// Grouping columns. Empty means no `GROUP BY`.
    pub(crate) group_by: Vec<SelectColumn>,
    /// Filter applied to each completed group. Absent means every group.
    pub(crate) having: Option<Expr>,
    /// Sort keys. Empty means insertion order, or first-seen group order
    /// for an aggregate query.
    pub(crate) order_by: Vec<OrderKey>,
    /// Maximum rows after filtering and sorting.
    pub(crate) limit: Option<u64>,
}

/// The `FROM` clause of a `SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QueryFrom {
    /// One table.
    Table(TableRef),
    /// `INNER JOIN` of two tables. `JOIN` without `OUTER` is this form.
    Join {
        /// Left input. Nested-loop order walks this table first.
        outer: TableRef,
        /// Right input.
        inner: TableRef,
        /// Join condition. A pair is kept only when this is TRUE.
        on: Expr,
    },
}

/// One table in `FROM`, with the name queries use for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableRef {
    /// Catalog name.
    pub(crate) name: String,
    /// Name visible to column references. This is `name` when the query
    /// wrote no alias. A table that has an alias is not also visible under
    /// its catalog name.
    pub(crate) alias: String,
}

impl TableRef {
    /// Text for `EXPLAIN`: the alias, and the catalog name when they differ.
    pub(crate) fn explain_name(&self) -> String {
        if self.alias == self.name {
            self.name.clone()
        } else {
            format!("{} ({})", self.alias, self.name)
        }
    }
}

/// One `SET` assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    /// Column being written.
    pub(crate) column: SelectColumn,
    /// Expression evaluated against the original row.
    pub(crate) value: Expr,
}

/// One `ORDER BY` key.
///
/// `nulls_first` is `None` when the query did not say `NULLS FIRST` or
/// `NULLS LAST`. Execution then uses PostgreSQL's default: nulls sort after
/// every non-null value, so `ASC` is nulls last and `DESC` is nulls first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OrderKey {
    /// A source column, or an aggregate in an aggregate query.
    pub(crate) target: OrderTarget,
    /// `true` for `ASC` and for an omitted direction.
    pub(crate) ascending: bool,
    /// Explicit `NULLS FIRST` (`Some(true)`) or `NULLS LAST` (`Some(false)`).
    pub(crate) nulls_first: Option<bool>,
}

/// What an `ORDER BY` key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OrderTarget {
    /// A column of an input table.
    Column(SelectColumn),
    /// `COUNT` or `SUM`.
    Aggregate(AggregateCall),
}

/// The select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Projection {
    /// `SELECT *`. Columns come from each input, in `FROM` order.
    All,
    /// `SELECT alias.*` for one input.
    QualifiedAll {
        /// Table name or alias that owns the columns.
        qualifier: String,
    },
    /// Explicit columns and aggregates, in output order.
    Columns(Vec<SelectItem>),
}

/// One item in an explicit select list.
///
/// `alias` is the name written with `AS` or as an implicit alias. `None` means
/// the output name is the schema column name or the rendered aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectItem {
    /// A column of an input table.
    Column {
        /// The bound column. The alias does not replace it.
        column: SelectColumn,
        /// Explicit output name.
        alias: Option<String>,
    },
    /// `COUNT`, `SUM`, `AVG`, `MIN`, or `MAX`.
    Aggregate {
        /// The call that owns the accumulator.
        call: AggregateCall,
        /// Explicit output name.
        alias: Option<String>,
    },
}

/// `COUNT`, `SUM`, `AVG`, `MIN`, or `MAX` in the select list, `HAVING`, or `ORDER BY`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AggregateCall {
    /// Which function.
    pub(crate) function: AggregateFunction,
    /// `*` or one column.
    pub(crate) argument: AggregateArg,
}

/// Aggregate functions this engine evaluates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AggregateFunction {
    /// `COUNT`.
    Count,
    /// `SUM` of an integer column.
    Sum,
    /// `AVG` of an integer column. The result is a query-only ratio.
    Avg,
    /// `MIN` of an integer, text, or boolean column.
    Min,
    /// `MAX` of an integer, text, or boolean column.
    Max,
}

/// The argument of `COUNT` or `SUM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AggregateArg {
    /// `COUNT(*)`.
    Star,
    /// A column. `COUNT` ignores NULL. `SUM` requires an integer column.
    Column(SelectColumn),
}

impl AggregateCall {
    /// SQL text used as the output name and in `EXPLAIN`.
    pub(crate) fn render(&self) -> String {
        let name = match self.function {
            AggregateFunction::Count => "COUNT",
            AggregateFunction::Sum => "SUM",
            AggregateFunction::Avg => "AVG",
            AggregateFunction::Min => "MIN",
            AggregateFunction::Max => "MAX",
        };
        match &self.argument {
            AggregateArg::Star => format!("{name}(*)"),
            AggregateArg::Column(column) => format!("{name}({})", column.render()),
        }
    }
}

/// A column reference in a select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectColumn {
    /// Normalized column name.
    pub(crate) name: String,
    /// Normalized table qualifier, when the user wrote `table.column`.
    pub(crate) qualifier: Option<String>,
}

impl SelectColumn {
    pub(crate) fn render(&self) -> String {
        match &self.qualifier {
            Some(qualifier) => format!("{qualifier}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// A scalar expression that can appear in `WHERE`, `HAVING`, or `VALUES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expr {
    /// A constant.
    Literal(Value),
    /// A column of the table being scanned.
    Column {
        /// Normalized column name.
        name: String,
        /// Normalized table qualifier, if one was written.
        qualifier: Option<String>,
    },
    /// A comparison or boolean operator.
    Binary {
        /// Left operand.
        left: Box<Expr>,
        /// Operator.
        op: BinOp,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `NOT`.
    Not(Box<Expr>),
    /// `IS NULL`.
    IsNull(Box<Expr>),
    /// `IS NOT NULL`.
    IsNotNull(Box<Expr>),
    /// An aggregate produced for `HAVING` or `ORDER BY`. A select-list aggregate
    /// stays a [`SelectItem`].
    Aggregate(AggregateCall),
}

/// Operators supported in `WHERE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    /// `=`
    Eq,
    /// `!=` or `<>`
    NotEq,
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `<=`
    LtEq,
    /// `>=`
    GtEq,
    /// `AND`
    And,
    /// `OR`
    Or,
}

impl Expr {
    /// SQL text for `EXPLAIN`. Column names are the names the query used.
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        write_expr(self, 0, &mut out);
        out
    }
}

fn write_expr(expr: &Expr, parent: u8, out: &mut String) {
    let mine = expr_precedence(expr);
    if mine < parent {
        out.push('(');
    }
    match expr {
        Expr::Literal(Value::Null) => out.push_str("NULL"),
        Expr::Literal(Value::Integer(value)) => out.push_str(&value.to_string()),
        Expr::Literal(Value::Boolean(true)) => out.push_str("TRUE"),
        Expr::Literal(Value::Boolean(false)) => out.push_str("FALSE"),
        Expr::Literal(Value::Text(text)) => push_sql_text(out, text),
        Expr::Literal(value @ Value::Ratio { .. }) => out.push_str(&value.to_string()),
        Expr::Column { name, qualifier } => {
            if let Some(qualifier) = qualifier {
                out.push_str(qualifier);
                out.push('.');
            }
            out.push_str(name);
        }
        Expr::Not(inner) => {
            out.push_str("NOT ");
            write_expr(inner, mine, out);
        }
        Expr::IsNull(inner) => {
            write_expr(inner, mine, out);
            out.push_str(" IS NULL");
        }
        Expr::IsNotNull(inner) => {
            write_expr(inner, mine, out);
            out.push_str(" IS NOT NULL");
        }
        Expr::Binary { left, op, right } => {
            write_expr(left, mine, out);
            out.push(' ');
            out.push_str(op.as_str());
            out.push(' ');
            write_expr(right, mine.saturating_add(1), out);
        }
        Expr::Aggregate(call) => out.push_str(&call.render()),
    }
    if mine < parent {
        out.push(')');
    }
}

fn expr_precedence(expr: &Expr) -> u8 {
    match expr {
        Expr::Binary { op: BinOp::Or, .. } => 1,
        Expr::Binary { op: BinOp::And, .. } => 2,
        Expr::Not(_) => 3,
        Expr::Binary { .. } | Expr::IsNull(_) | Expr::IsNotNull(_) => 4,
        Expr::Literal(_) | Expr::Column { .. } | Expr::Aggregate(_) => 5,
    }
}

fn push_sql_text(out: &mut String, text: &str) {
    out.push('\'');
    for ch in text.chars() {
        if ch == '\'' {
            out.push('\'');
        }
        out.push(ch);
    }
    out.push('\'');
}

impl BinOp {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "=",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::Gt => ">",
            Self::LtEq => "<=",
            Self::GtEq => ">=",
            Self::And => "AND",
            Self::Or => "OR",
        }
    }

    pub(crate) fn is_logical(self) -> bool {
        matches!(self, Self::And | Self::Or)
    }

    pub(crate) fn is_ordering(self) -> bool {
        matches!(self, Self::Lt | Self::Gt | Self::LtEq | Self::GtEq)
    }
}

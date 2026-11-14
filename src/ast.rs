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
    Select {
        /// Input tables.
        from: QueryFrom,
        /// Output columns.
        projection: Projection,
        /// Optional filter. Absent means every joined row. Applied after `ON`.
        selection: Option<Expr>,
        /// Sort keys. Empty means nested-loop order, which is insertion order
        /// for a single table.
        order_by: Vec<OrderKey>,
        /// Maximum rows after filtering and sorting.
        limit: Option<u64>,
    },
    /// `EXPLAIN` of one `SELECT`. The query is planned and not executed.
    Explain(Box<Statement>),
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
    /// Source-table column, which does not have to appear in the select list.
    pub(crate) column: SelectColumn,
    /// `true` for `ASC` and for an omitted direction.
    pub(crate) ascending: bool,
    /// Explicit `NULLS FIRST` (`Some(true)`) or `NULLS LAST` (`Some(false)`).
    pub(crate) nulls_first: Option<bool>,
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
    /// Explicit columns, in output order.
    Columns(Vec<SelectColumn>),
}

/// A column reference in a select list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectColumn {
    /// Normalized column name.
    pub(crate) name: String,
    /// Normalized table qualifier, when the user wrote `table.column`.
    pub(crate) qualifier: Option<String>,
}

/// A scalar expression that can appear in `WHERE` or `VALUES`.
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
        Expr::Literal(_) | Expr::Column { .. } => 5,
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

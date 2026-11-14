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
    /// A single-table `SELECT`.
    Select {
        /// Normalized table name.
        table: String,
        /// Output columns.
        projection: Projection,
        /// Optional filter. Absent means every row.
        selection: Option<Expr>,
        /// Sort keys. Empty means insertion order.
        order_by: Vec<OrderKey>,
        /// Maximum rows after filtering and sorting.
        limit: Option<u64>,
    },
    /// `EXPLAIN` of one `SELECT`. The query is planned and not executed.
    Explain(Box<Statement>),
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
    /// `SELECT *`.
    All,
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

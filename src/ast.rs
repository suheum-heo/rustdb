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
    /// `INSERT INTO ... VALUES`.
    Insert {
        /// Normalized table name.
        table: String,
        /// Normalized target columns. Empty means every column, in order.
        columns: Vec<String>,
        /// One value list per inserted row.
        rows: Vec<Vec<Value>>,
    },
    /// A single-table `SELECT`.
    Select {
        /// Normalized table name.
        table: String,
        /// Output columns.
        projection: Projection,
        /// Optional filter. Absent means every row.
        selection: Option<Expr>,
    },
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

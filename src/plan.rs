//! Access paths for a single-table `SELECT`.
//!
//! This is not a cost-based optimizer. An integer comparison, or an `AND` of
//! comparisons on one indexed column, may become an index scan. The original
//! `WHERE` is still evaluated on every candidate. `OR`, `IS NULL`, `IS NOT
//! NULL`, and any predicate that is not an integer comparison stay a sequential
//! scan. A predicate that contains `OR` anywhere is a sequential scan.
//!
//! A range also stays a sequential scan when it covers every `i64`, or every
//! key currently stored in the chosen index. That decision uses the predicate
//! and, when needed, the index's minimum and maximum key. It does not assume
//! that integers are uniformly distributed, and it does not treat `INTEGER` as
//! nonnegative. Equality stays an index lookup.

use std::collections::HashMap;

use crate::ast::{BinOp, Expr};
use crate::value::Value;

/// One integer index the planner may use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexChoice {
    pub name: String,
    pub column: String,
    pub ordinal: usize,
}

/// How `SELECT` will find candidate rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AccessPath {
    /// Read the in-memory row vector from front to back.
    SeqScan,
    /// Read candidate row ids from one integer B+ tree.
    IndexScan {
        /// Index that supplies the candidates.
        name: String,
        /// Indexed column. The scan bounds are on this column.
        column: String,
        /// Schema position of `column`.
        ordinal: usize,
        /// Inclusive integer bounds after reversing literals and folding `<` / `>`.
        bounds: Bounds,
    },
}

/// Inclusive key bounds for one integer column.
///
/// `< n` is `<= n - 1` and `> n` is `>= n + 1`. Those rewrites are exact for
/// `i64`. A bound that falls off the end of `i64`, or a contradiction, is
/// [`Bounds::Empty`] and produces no candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bounds {
    /// No `i64` key can qualify.
    Empty,
    /// `column = value`.
    Equal(i64),
    /// Inclusive range. A missing side is unbounded.
    Range { low: Option<i64>, high: Option<i64> },
}

impl Bounds {
    fn describe(self, column: &str) -> String {
        match self {
            Self::Empty => "none".to_string(),
            Self::Equal(value) => format!("{column} = {value}"),
            Self::Range { low, high } => match (low, high) {
                (Some(low), Some(high)) => format!("{column} >= {low} AND {column} <= {high}"),
                (Some(low), None) => format!("{column} >= {low}"),
                (None, Some(high)) => format!("{column} <= {high}"),
                (None, None) => "none".to_string(),
            },
        }
    }
}

/// Columns of an `EXPLAIN` row: access, index, bounds.
pub(crate) fn describe(path: &AccessPath) -> (Value, Value, Value) {
    match path {
        AccessPath::SeqScan => (Value::Text("SeqScan".to_string()), Value::Null, Value::Null),
        AccessPath::IndexScan {
            name,
            column,
            bounds,
            ..
        } => (
            Value::Text("IndexScan".to_string()),
            Value::Text(name.clone()),
            Value::Text(bounds.describe(column)),
        ),
    }
}

/// `true` when `bounds` includes every `i64` value.
///
/// `column >= i64::MIN` and `column <= i64::MAX` are this range. A missing
/// side is unbounded. This does not look at stored rows.
pub(crate) fn range_covers_every_i64(bounds: Bounds) -> bool {
    let Bounds::Range { low, high } = bounds else {
        return false;
    };
    low.is_none_or(|value| value == i64::MIN) && high.is_none_or(|value| value == i64::MAX)
}

/// `true` when every key in `span` satisfies the inclusive range.
///
/// An empty index has no span and does not match. The caller supplies the
/// span from the tree's edge keys.
pub(crate) fn range_covers_stored_keys(bounds: Bounds, span: Option<(i64, i64)>) -> bool {
    let Bounds::Range { low, high } = bounds else {
        return false;
    };
    let Some((min, max)) = span else {
        return false;
    };
    low.is_none_or(|value| value <= min) && high.is_none_or(|value| value >= max)
}

/// Pick a scan for `selection`.
///
/// When several indexes apply, an equality wins over a range. Equal ranks keep
/// the earlier index in `indexes`, which is catalog order. There is no cost
/// comparison.
pub(crate) fn choose(table: &str, selection: Option<&Expr>, indexes: &[IndexChoice]) -> AccessPath {
    let Some(selection) = selection else {
        return AccessPath::SeqScan;
    };
    if indexes.is_empty() || contains_or(selection) {
        return AccessPath::SeqScan;
    }

    let mut atoms = Vec::new();
    flatten_and(selection, &mut atoms);
    let mut by_column: HashMap<&str, Accum> = HashMap::new();
    for atom in atoms {
        let Some((column, op, value)) = indexed_comparison(atom, table) else {
            continue;
        };
        if !indexes.iter().any(|index| index.column == column) {
            continue;
        }
        by_column.entry(column).or_default().apply(op, value);
    }

    let mut best: Option<(u8, &IndexChoice, Bounds)> = None;
    for index in indexes {
        let Some(accum) = by_column.get(index.column.as_str()) else {
            continue;
        };
        let bounds = accum.bounds();
        let rank = match bounds {
            Bounds::Equal(_) | Bounds::Empty => 0,
            Bounds::Range { .. } => 1,
        };
        if best
            .as_ref()
            .is_none_or(|(best_rank, _, _)| rank < *best_rank)
        {
            best = Some((rank, index, bounds));
        }
    }

    match best {
        Some((_, index, bounds)) => AccessPath::IndexScan {
            name: index.name.clone(),
            column: index.column.clone(),
            ordinal: index.ordinal,
            bounds,
        },
        None => AccessPath::SeqScan,
    }
}

#[derive(Default)]
struct Accum {
    empty: bool,
    equal: Option<i64>,
    low: Option<i64>,
    high: Option<i64>,
}

impl Accum {
    fn apply(&mut self, op: BinOp, value: i64) {
        if self.empty {
            return;
        }
        match op {
            BinOp::Eq => self.meet_equal(value),
            BinOp::Lt => match value.checked_sub(1) {
                Some(high) => self.meet_high(high),
                None => self.empty = true,
            },
            BinOp::LtEq => self.meet_high(value),
            BinOp::Gt => match value.checked_add(1) {
                Some(low) => self.meet_low(low),
                None => self.empty = true,
            },
            BinOp::GtEq => self.meet_low(value),
            BinOp::NotEq | BinOp::And | BinOp::Or => {}
        }
    }

    fn meet_equal(&mut self, value: i64) {
        if self.low.is_some_and(|low| value < low) || self.high.is_some_and(|high| value > high) {
            self.empty = true;
            return;
        }
        if let Some(existing) = self.equal {
            if existing != value {
                self.empty = true;
            }
            return;
        }
        self.equal = Some(value);
    }

    fn meet_low(&mut self, low: i64) {
        if let Some(equal) = self.equal {
            if equal < low {
                self.empty = true;
            }
            return;
        }
        if self.high.is_some_and(|high| low > high) {
            self.empty = true;
            return;
        }
        self.low = Some(self.low.map_or(low, |current| current.max(low)));
    }

    fn meet_high(&mut self, high: i64) {
        if let Some(equal) = self.equal {
            if equal > high {
                self.empty = true;
            }
            return;
        }
        if self.low.is_some_and(|low| high < low) {
            self.empty = true;
            return;
        }
        self.high = Some(self.high.map_or(high, |current| current.min(high)));
    }

    fn bounds(&self) -> Bounds {
        if self.empty {
            return Bounds::Empty;
        }
        if let Some(value) = self.equal {
            return Bounds::Equal(value);
        }
        Bounds::Range {
            low: self.low,
            high: self.high,
        }
    }
}

fn contains_or(expr: &Expr) -> bool {
    match expr {
        Expr::Binary { op: BinOp::Or, .. } => true,
        Expr::Binary { left, right, .. } => contains_or(left) || contains_or(right),
        Expr::Not(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => contains_or(inner),
        Expr::Literal(_) | Expr::Column { .. } => false,
    }
}

fn flatten_and<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let Expr::Binary {
        left,
        op: BinOp::And,
        right,
    } = expr
    {
        flatten_and(left, out);
        flatten_and(right, out);
    } else {
        out.push(expr);
    }
}

fn indexed_comparison<'a>(expr: &'a Expr, table: &str) -> Option<(&'a str, BinOp, i64)> {
    let Expr::Binary { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinOp::Eq | BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq
    ) {
        return None;
    }
    match (left.as_ref(), right.as_ref()) {
        (Expr::Column { name, qualifier }, Expr::Literal(Value::Integer(value)))
            if qualifier_matches(qualifier, table) =>
        {
            Some((name.as_str(), *op, *value))
        }
        (Expr::Literal(Value::Integer(value)), Expr::Column { name, qualifier })
            if qualifier_matches(qualifier, table) =>
        {
            Some((name.as_str(), flip(*op), *value))
        }
        _ => None,
    }
}

fn qualifier_matches(qualifier: &Option<String>, table: &str) -> bool {
    qualifier.as_deref().is_none_or(|name| name == table)
}

fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Eq => BinOp::Eq,
        BinOp::Lt => BinOp::Gt,
        BinOp::Gt => BinOp::Lt,
        BinOp::LtEq => BinOp::GtEq,
        BinOp::GtEq => BinOp::LtEq,
        BinOp::NotEq | BinOp::And | BinOp::Or => op,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> Expr {
        Expr::Column {
            name: name.to_string(),
            qualifier: None,
        }
    }

    fn integer(value: i64) -> Expr {
        Expr::Literal(Value::Integer(value))
    }

    fn binary(left: Expr, op: BinOp, right: Expr) -> Expr {
        Expr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    fn indexes() -> Vec<IndexChoice> {
        vec![
            IndexChoice {
                name: "idx_age".to_string(),
                column: "age".to_string(),
                ordinal: 1,
            },
            IndexChoice {
                name: "idx_id".to_string(),
                column: "id".to_string(),
                ordinal: 0,
            },
        ]
    }

    fn bounds_of(expr: &Expr) -> (String, Bounds) {
        match choose("users", Some(expr), &indexes()) {
            AccessPath::IndexScan { name, bounds, .. } => (name, bounds),
            AccessPath::SeqScan => panic!("expected an index scan"),
        }
    }

    #[test]
    fn normalizes_reversed_comparisons_and_intersects_one_column() {
        let expr = binary(integer(25), BinOp::Lt, column("age"));
        assert_eq!(
            bounds_of(&expr),
            (
                "idx_age".to_string(),
                Bounds::Range {
                    low: Some(26),
                    high: None
                }
            )
        );

        let expr = binary(
            binary(column("age"), BinOp::GtEq, integer(10)),
            BinOp::And,
            binary(column("age"), BinOp::Lt, integer(20)),
        );
        assert_eq!(
            bounds_of(&expr),
            (
                "idx_age".to_string(),
                Bounds::Range {
                    low: Some(10),
                    high: Some(19)
                }
            )
        );

        let expr = binary(
            binary(column("age"), BinOp::Gt, integer(10)),
            BinOp::And,
            binary(column("age"), BinOp::Lt, integer(5)),
        );
        assert_eq!(bounds_of(&expr).1, Bounds::Empty);

        let below_min = binary(column("age"), BinOp::Lt, integer(i64::MIN));
        assert_eq!(bounds_of(&below_min).1, Bounds::Empty);
        let above_max = binary(column("age"), BinOp::Gt, integer(i64::MAX));
        assert_eq!(bounds_of(&above_max).1, Bounds::Empty);
    }

    #[test]
    fn prefers_equality_and_leaves_or_unindexed() {
        let expr = binary(
            binary(column("id"), BinOp::Eq, integer(1)),
            BinOp::And,
            binary(column("age"), BinOp::Lt, integer(10)),
        );
        assert_eq!(bounds_of(&expr).0, "idx_id");

        let ranges = binary(
            binary(column("id"), BinOp::Lt, integer(10)),
            BinOp::And,
            binary(column("age"), BinOp::Lt, integer(10)),
        );
        assert_eq!(bounds_of(&ranges).0, "idx_age");

        let either = binary(
            binary(column("age"), BinOp::Eq, integer(1)),
            BinOp::Or,
            binary(column("id"), BinOp::Eq, integer(1)),
        );
        assert!(matches!(
            choose("users", Some(&either), &indexes()),
            AccessPath::SeqScan
        ));
        assert!(matches!(
            choose("users", Some(&either), &[]),
            AccessPath::SeqScan
        ));

        let (access, index, scan_bounds) = describe(&choose("users", Some(&either), &indexes()));
        assert_eq!(access, Value::Text("SeqScan".to_string()));
        assert_eq!(index, Value::Null);
        assert_eq!(scan_bounds, Value::Null);
    }

    #[test]
    fn full_domain_and_stored_span_are_sequential_candidates() {
        assert!(range_covers_every_i64(Bounds::Range {
            low: Some(i64::MIN),
            high: None,
        }));
        assert!(range_covers_every_i64(Bounds::Range {
            low: None,
            high: Some(i64::MAX),
        }));
        assert!(!range_covers_every_i64(Bounds::Range {
            low: Some(0),
            high: None,
        }));
        assert!(!range_covers_every_i64(Bounds::Equal(1)));

        let span = Some((0, 99));
        assert!(range_covers_stored_keys(
            Bounds::Range {
                low: Some(0),
                high: None,
            },
            span
        ));
        assert!(!range_covers_stored_keys(
            Bounds::Range {
                low: Some(50),
                high: None,
            },
            span
        ));
        assert!(!range_covers_stored_keys(
            Bounds::Range {
                low: Some(0),
                high: Some(9),
            },
            span
        ));
        assert!(!range_covers_stored_keys(
            Bounds::Range {
                low: Some(0),
                high: None,
            },
            None
        ));
    }
}

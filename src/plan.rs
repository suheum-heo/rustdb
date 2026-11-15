//! Access paths for `SELECT`.
//!
//! This is not a cost-based optimizer. An integer comparison, or an `AND` of
//! comparisons on one indexed column, may become an index scan. The original
//! `WHERE` is still evaluated on every candidate. `OR`, `IS NULL`, `IS NOT
//! NULL`, and any predicate that is not an integer comparison stay a sequential
//! scan. A predicate that contains `OR` anywhere is a sequential scan.
//!
//! A two-table `INNER JOIN` uses the same rule for `OR`. It probes an integer
//! index on the inner table only when `ON` contains an equality between an
//! outer integer column and that indexed column. The original `ON` stays the
//! residual predicate. Anything else is a nested loop.
//!
//! A range also stays a sequential scan when it covers every `i64`, or every
//! key currently stored in the chosen index. That decision uses the predicate
//! and, when needed, the index's minimum and maximum key. It does not assume
//! that integers are uniformly distributed, and it does not treat `INTEGER` as
//! nonnegative. Equality stays an index lookup.

use std::collections::HashMap;

use crate::ast::{BinOp, Expr};
use crate::value::{DataType, Value};

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

/// How an `INNER JOIN` finds candidate pairs.
///
/// [`JoinAccess::IndexNestedLoop`] still evaluates the original `ON`. `residual`
/// is that text when `ON` is wider than the equality used for the probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JoinAccess {
    /// Compare every outer row with every inner row.
    NestedLoop,
    /// For each outer row, look up one integer key in an inner index.
    IndexNestedLoop {
        /// Index that supplies the inner candidates.
        name: String,
        /// Schema position of the indexed inner column.
        inner_ordinal: usize,
        /// Schema position of the outer join key.
        outer_ordinal: usize,
        /// The equality that supplies the probe key.
        predicate: String,
        /// Original `ON` text when it is not exactly `predicate`.
        residual: Option<String>,
    },
}

/// Pick a join path. There is no cost comparison.
///
/// An equality wins only when one side is an outer integer column and the
/// other is an inner column that has an index. Reversed operands are the same
/// equality. `AND` may hide that equality among other predicates. The earliest
/// matching index in `indexes` wins, and `indexes` is catalog order. `OR`
/// anywhere, a missing index, a non-equality, or a comparison that is not
/// between those two sides stays a nested loop.
pub(crate) fn choose_join(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
) -> JoinAccess {
    if indexes.is_empty() || contains_or(on) {
        return JoinAccess::NestedLoop;
    }
    let mut atoms = Vec::new();
    flatten_and(on, &mut atoms);
    let mut probes = Vec::new();
    for atom in atoms {
        let Some((outer_ordinal, inner_ordinal)) =
            equijoin_ordinals(atom, outer_alias, outer_columns, inner_alias, inner_columns)
        else {
            continue;
        };
        let Some(index_pos) = indexes
            .iter()
            .position(|index| index.ordinal == inner_ordinal)
        else {
            continue;
        };
        probes.push((index_pos, outer_ordinal, atom));
    }
    let Some((index_pos, outer_ordinal, atom)) =
        probes.iter().min_by_key(|(index_pos, _, _)| *index_pos)
    else {
        return JoinAccess::NestedLoop;
    };
    let index = &indexes[*index_pos];
    let predicate = atom.render();
    let residual = if predicate == on.render() {
        None
    } else {
        Some(on.render())
    };
    JoinAccess::IndexNestedLoop {
        name: index.name.clone(),
        inner_ordinal: index.ordinal,
        outer_ordinal: *outer_ordinal,
        predicate,
        residual,
    }
}

fn equijoin_ordinals(
    expr: &Expr,
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
) -> Option<(usize, usize)> {
    let Expr::Binary {
        left,
        op: BinOp::Eq,
        right,
    } = expr
    else {
        return None;
    };
    let left = column_side(left, outer_alias, outer_columns, inner_alias, inner_columns)?;
    let right = column_side(
        right,
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
    )?;
    match (left, right) {
        (ColumnSide::Outer(outer), ColumnSide::Inner(inner))
        | (ColumnSide::Inner(inner), ColumnSide::Outer(outer)) => Some((outer, inner)),
        (ColumnSide::Outer(_), ColumnSide::Outer(_))
        | (ColumnSide::Inner(_), ColumnSide::Inner(_)) => None,
    }
}

enum ColumnSide {
    Outer(usize),
    Inner(usize),
}

fn column_side(
    expr: &Expr,
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
) -> Option<ColumnSide> {
    let Expr::Column { name, qualifier } = expr else {
        return None;
    };
    if let Some(qualifier) = qualifier {
        if qualifier == outer_alias {
            return integer_ordinal(outer_columns, name).map(ColumnSide::Outer);
        }
        if qualifier == inner_alias {
            return integer_ordinal(inner_columns, name).map(ColumnSide::Inner);
        }
        return None;
    }
    match (
        integer_ordinal(outer_columns, name),
        integer_ordinal(inner_columns, name),
    ) {
        (Some(outer), None) => Some(ColumnSide::Outer(outer)),
        (None, Some(inner)) => Some(ColumnSide::Inner(inner)),
        (Some(_), Some(_)) | (None, None) => None,
    }
}

fn integer_ordinal(columns: &[(String, DataType)], name: &str) -> Option<usize> {
    let index = columns.iter().position(|(column, _)| column == name)?;
    (columns[index].1 == DataType::Integer).then_some(index)
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

    fn qualified(table: &str, name: &str) -> Expr {
        Expr::Column {
            name: name.to_string(),
            qualifier: Some(table.to_string()),
        }
    }

    struct JoinTables {
        outer: Vec<(String, DataType)>,
        inner: Vec<(String, DataType)>,
    }

    fn users_and_orders() -> JoinTables {
        JoinTables {
            outer: vec![
                ("id".to_string(), DataType::Integer),
                ("name".to_string(), DataType::Text),
            ],
            inner: vec![
                ("id".to_string(), DataType::Integer),
                ("user_id".to_string(), DataType::Integer),
                ("total".to_string(), DataType::Integer),
            ],
        }
    }

    fn order_indexes() -> Vec<IndexChoice> {
        vec![
            IndexChoice {
                name: "idx_orders_id".to_string(),
                column: "id".to_string(),
                ordinal: 0,
            },
            IndexChoice {
                name: "idx_orders_user".to_string(),
                column: "user_id".to_string(),
                ordinal: 1,
            },
        ]
    }

    fn join_path(on: &Expr, indexes: &[IndexChoice]) -> JoinAccess {
        let tables = users_and_orders();
        choose_join("users", &tables.outer, "orders", &tables.inner, on, indexes)
    }

    #[test]
    fn join_equality_uses_the_inner_index_in_catalog_order() {
        let on = binary(
            qualified("users", "id"),
            BinOp::Eq,
            qualified("orders", "user_id"),
        );
        match join_path(&on, &order_indexes()) {
            JoinAccess::IndexNestedLoop {
                name,
                inner_ordinal,
                outer_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(name, "idx_orders_user");
                assert_eq!(inner_ordinal, 1);
                assert_eq!(outer_ordinal, 0);
                assert_eq!(predicate, "users.id = orders.user_id");
                assert_eq!(residual, None);
            }
            JoinAccess::NestedLoop => panic!("expected an index nested loop"),
        }

        let reversed = binary(
            qualified("orders", "user_id"),
            BinOp::Eq,
            qualified("users", "id"),
        );
        assert!(matches!(
            join_path(&reversed, &order_indexes()),
            JoinAccess::IndexNestedLoop { name, .. } if name == "idx_orders_user"
        ));

        let both = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "id"),
            ),
            BinOp::And,
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
        );
        match join_path(&both, &order_indexes()) {
            JoinAccess::IndexNestedLoop { name, residual, .. } => {
                assert_eq!(name, "idx_orders_id");
                assert_eq!(
                    residual.as_deref(),
                    Some("users.id = orders.id AND users.id = orders.user_id")
                );
            }
            JoinAccess::NestedLoop => panic!("expected the earlier index"),
        }
    }

    #[test]
    fn join_without_a_safe_equality_scans_both_tables() {
        let tables = users_and_orders();
        let indexes = order_indexes();
        let or = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
            BinOp::Or,
            binary(qualified("orders", "total"), BinOp::Gt, integer(10)),
        );
        assert!(matches!(
            choose_join(
                "users",
                &tables.outer,
                "orders",
                &tables.inner,
                &or,
                &indexes
            ),
            JoinAccess::NestedLoop
        ));

        let range = binary(
            qualified("orders", "user_id"),
            BinOp::Gt,
            qualified("users", "id"),
        );
        assert!(matches!(
            join_path(&range, &indexes),
            JoinAccess::NestedLoop
        ));
        let nulls = Expr::IsNull(Box::new(qualified("orders", "user_id")));
        assert!(matches!(
            join_path(&nulls, &indexes),
            JoinAccess::NestedLoop
        ));
        let equality = binary(
            qualified("users", "id"),
            BinOp::Eq,
            qualified("orders", "user_id"),
        );
        assert!(matches!(join_path(&equality, &[]), JoinAccess::NestedLoop));
        let id_only = [IndexChoice {
            name: "idx_orders_id".to_string(),
            column: "id".to_string(),
            ordinal: 0,
        }];
        assert!(matches!(
            join_path(&equality, &id_only),
            JoinAccess::NestedLoop
        ));
    }
}

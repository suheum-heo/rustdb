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
//! A two-table `LEFT JOIN` probes an integer index on the right table when
//! `ON` contains an equality between a left integer column and that indexed
//! column. The executor keeps that probe only when the left table is empty or
//! the right table has more than [`INDEX_PROBE_LEFT_FACTOR`] times as many
//! rows. Otherwise that same integer equality hashes the right table, because
//! one hash build is cheaper than a tree probe per left row. This is a size
//! rule, not a cost model. It does not change the inner join's 64× rule. A
//! `TEXT` or `BOOLEAN` equality is not an index probe. It hashes the right
//! table even when an integer index exists on another column. An indexed
//! integer equality is preferred to an earlier text equality in the same
//! `AND`; the text comparison stays in the residual. `OR`, a range, and
//! `IS NULL` stay a nested loop.
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
/// [`JoinAccess::IndexNestedLoop`] and [`JoinAccess::HashJoin`] still evaluate
/// the original `ON`. `residual` is that text when `ON` is wider than the
/// equality used to find candidates.
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
    /// Probe an in-memory hash table built from one equality.
    ///
    /// The executor builds the smaller input. Equal sizes build the inner
    /// table, so matches can be emitted in outer insertion order.
    HashJoin {
        /// Schema position of the outer join key.
        outer_ordinal: usize,
        /// Schema position of the inner join key.
        inner_ordinal: usize,
        /// The equality that supplies the hash key.
        predicate: String,
        /// Original `ON` text when it is not exactly `predicate`.
        residual: Option<String>,
    },
}

/// How a two-table `LEFT JOIN` will be executed.
///
/// An integer equality on an indexed right column is an index probe.
/// [`finish_left_access`] turns that probe back into a hash of the right table
/// unless the right table is empty-probed or more than
/// [`INDEX_PROBE_LEFT_FACTOR`] times the left table. Any other equality of
/// `INTEGER`, `TEXT`, or `BOOLEAN` columns hashes the right table. `OR`, a
/// range, `IS NULL`, or any comparison that is not a column equality on both
/// sides stays a nested loop. `WHERE` is not part of this choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeftAccess {
    /// Compare every left row with every right row.
    NestedLoop,
    /// Probe a hash table built from the right table.
    ///
    /// The build is always the right input, so left rows can be emitted in
    /// insertion order without a pair buffer. `predicate` is the equality used
    /// as the hash key. `residual` is the original `ON` when it is wider.
    Hash {
        /// Schema position of the left join key.
        outer_ordinal: usize,
        /// Schema position of the right join key.
        inner_ordinal: usize,
        /// The equality that supplies the hash key.
        predicate: String,
        /// Original `ON` text when it is not exactly `predicate`.
        residual: Option<String>,
    },
    /// For each left row, look up one integer key in a right-table index.
    ///
    /// The original `ON` still decides a match. `residual` is that text when
    /// `ON` is wider than `predicate`.
    Index {
        /// Index that supplies the right candidates.
        name: String,
        /// Schema position of the indexed right column.
        inner_ordinal: usize,
        /// Schema position of the left join key.
        outer_ordinal: usize,
        /// The equality that supplies the probe key.
        predicate: String,
        /// Original `ON` text when it is not exactly `predicate`.
        residual: Option<String>,
    },
}

/// Right rows per left row before a left join keeps an index probe.
///
/// Hashing the whole right table is one pass. An index probe is one tree
/// lookup per left row. The inner join measured the hash scan faster at 20×
/// and the index probe faster at 100× and 400×, so 64 sits between those
/// measurements. This constant is not the inner join's factor: changing one
/// does not change the other. It is a size rule, not a cost model.
pub(crate) const INDEX_PROBE_LEFT_FACTOR: usize = 64;

/// Pick a left-join path from the shape of `ON`. Row counts are applied later.
///
/// An indexed integer equality is the first candidate. Reversed operands are
/// the same equality. The earliest matching index in `indexes` wins, and
/// `indexes` is catalog order. Otherwise the first column-to-column equality
/// in `AND` order hashes the right table. A later equality is not a second
/// hash key. `OR` anywhere stays a nested loop.
pub(crate) fn choose_left_join(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
) -> LeftAccess {
    if contains_or(on) {
        return LeftAccess::NestedLoop;
    }
    let mut atoms = Vec::new();
    flatten_and(on, &mut atoms);
    if let Some(path) = left_index_join(
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        on,
        indexes,
        &atoms,
    ) {
        return path;
    }
    for atom in atoms {
        let Some((outer_ordinal, inner_ordinal)) =
            hash_ordinals(atom, outer_alias, outer_columns, inner_alias, inner_columns)
        else {
            continue;
        };
        let predicate = atom.render();
        return LeftAccess::Hash {
            outer_ordinal,
            inner_ordinal,
            predicate: predicate.clone(),
            residual: residual(on, &predicate),
        };
    }
    LeftAccess::NestedLoop
}

/// Keep an index probe when the right table is the one the size rule says is
/// cheaper to probe. Every other indexed equality hashes those same columns.
pub(crate) fn finish_left_access(
    access: LeftAccess,
    left_len: usize,
    right_len: usize,
) -> LeftAccess {
    let LeftAccess::Index {
        name,
        inner_ordinal,
        outer_ordinal,
        predicate,
        residual,
    } = access
    else {
        return access;
    };
    if prefer_left_index_probe(left_len, right_len) {
        return LeftAccess::Index {
            name,
            inner_ordinal,
            outer_ordinal,
            predicate,
            residual,
        };
    }
    LeftAccess::Hash {
        outer_ordinal,
        inner_ordinal,
        predicate,
        residual,
    }
}

/// `true` when probing the right index is the chosen size rule.
pub(crate) fn prefer_left_index_probe(left_len: usize, right_len: usize) -> bool {
    if left_len == 0 {
        return true;
    }
    right_len > left_len.saturating_mul(INDEX_PROBE_LEFT_FACTOR)
}

fn left_index_join(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
    atoms: &[&Expr],
) -> Option<LeftAccess> {
    let (name, inner_ordinal, outer_ordinal, predicate, residual) = indexed_equality(
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        on,
        indexes,
        atoms,
    )?;
    Some(LeftAccess::Index {
        name,
        inner_ordinal,
        outer_ordinal,
        predicate,
        residual,
    })
}

/// Pick a join path from the shape of `ON`. Row counts are applied later.
///
/// An indexed integer equality is the first candidate when one side is an
/// outer integer column and the other is an inner column that has an index.
/// Reversed operands are the same equality. `AND` may hide that equality
/// among other predicates. The earliest matching index in `indexes` wins, and
/// `indexes` is catalog order. Otherwise a column-to-column equality of the
/// same scalar type is a hash join, using the first such atom in `AND` order.
/// `OR` anywhere, a non-equality, or a comparison that is not between the two
/// sides stays a nested loop. The executor keeps the index only when the inner
/// table is empty-probed or much larger than the outer table. Every other
/// indexed equality becomes a hash join on the same columns.
pub(crate) fn choose_join(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
) -> JoinAccess {
    if contains_or(on) {
        return JoinAccess::NestedLoop;
    }
    let mut atoms = Vec::new();
    flatten_and(on, &mut atoms);
    if let Some(path) = index_join(
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        on,
        indexes,
        &atoms,
    ) {
        return path;
    }
    for atom in atoms {
        let Some((outer_ordinal, inner_ordinal)) =
            hash_ordinals(atom, outer_alias, outer_columns, inner_alias, inner_columns)
        else {
            continue;
        };
        return hash_join(on, atom, outer_ordinal, inner_ordinal);
    }
    JoinAccess::NestedLoop
}

fn index_join(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
    atoms: &[&Expr],
) -> Option<JoinAccess> {
    let (name, inner_ordinal, outer_ordinal, predicate, residual) = indexed_equality(
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        on,
        indexes,
        atoms,
    )?;
    Some(JoinAccess::IndexNestedLoop {
        name,
        inner_ordinal,
        outer_ordinal,
        predicate,
        residual,
    })
}

/// The earliest catalog index on an integer equality, if `ON` has one.
fn indexed_equality(
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    on: &Expr,
    indexes: &[IndexChoice],
    atoms: &[&Expr],
) -> Option<(String, usize, usize, String, Option<String>)> {
    if indexes.is_empty() {
        return None;
    }
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
        probes.push((index_pos, outer_ordinal, *atom));
    }
    let (index_pos, outer_ordinal, atom) =
        probes.iter().min_by_key(|(index_pos, _, _)| *index_pos)?;
    let index = &indexes[*index_pos];
    let predicate = atom.render();
    let residual = residual(on, &predicate);
    Some((
        index.name.clone(),
        index.ordinal,
        *outer_ordinal,
        predicate,
        residual,
    ))
}

fn hash_join(on: &Expr, atom: &Expr, outer_ordinal: usize, inner_ordinal: usize) -> JoinAccess {
    let predicate = atom.render();
    JoinAccess::HashJoin {
        outer_ordinal,
        inner_ordinal,
        predicate: predicate.clone(),
        residual: residual(on, &predicate),
    }
}

fn residual(on: &Expr, predicate: &str) -> Option<String> {
    let rendered = on.render();
    (rendered != predicate).then_some(rendered)
}

fn equijoin_ordinals(
    expr: &Expr,
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
) -> Option<(usize, usize)> {
    typed_equijoin(
        expr,
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        true,
    )
    .map(|(outer, inner, _)| (outer, inner))
}

fn hash_ordinals(
    expr: &Expr,
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
) -> Option<(usize, usize)> {
    typed_equijoin(
        expr,
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        false,
    )
    .map(|(outer, inner, _)| (outer, inner))
}

fn typed_equijoin(
    expr: &Expr,
    outer_alias: &str,
    outer_columns: &[(String, DataType)],
    inner_alias: &str,
    inner_columns: &[(String, DataType)],
    integers_only: bool,
) -> Option<(usize, usize, DataType)> {
    let Expr::Binary {
        left,
        op: BinOp::Eq,
        right,
    } = expr
    else {
        return None;
    };
    let left = column_side(
        left,
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        integers_only,
    )?;
    let right = column_side(
        right,
        outer_alias,
        outer_columns,
        inner_alias,
        inner_columns,
        integers_only,
    )?;
    if left.1 != right.1 {
        return None;
    }
    match (left.0, right.0) {
        (ColumnSide::Outer(outer), ColumnSide::Inner(inner))
        | (ColumnSide::Inner(inner), ColumnSide::Outer(outer)) => Some((outer, inner, left.1)),
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
    integers_only: bool,
) -> Option<(ColumnSide, DataType)> {
    let Expr::Column { name, qualifier } = expr else {
        return None;
    };
    let resolve = |columns: &[(String, DataType)]| -> Option<(usize, DataType)> {
        let index = columns.iter().position(|(column, _)| column == name)?;
        let data_type = columns[index].1;
        if integers_only && data_type != DataType::Integer {
            return None;
        }
        Some((index, data_type))
    };
    if let Some(qualifier) = qualifier {
        if qualifier == outer_alias {
            let (ordinal, data_type) = resolve(outer_columns)?;
            return Some((ColumnSide::Outer(ordinal), data_type));
        }
        if qualifier == inner_alias {
            let (ordinal, data_type) = resolve(inner_columns)?;
            return Some((ColumnSide::Inner(ordinal), data_type));
        }
        return None;
    }
    match (resolve(outer_columns), resolve(inner_columns)) {
        (Some((outer, data_type)), None) => Some((ColumnSide::Outer(outer), data_type)),
        (None, Some((inner, data_type))) => Some((ColumnSide::Inner(inner), data_type)),
        (Some(_), Some(_)) | (None, None) => None,
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
        Expr::Literal(_) | Expr::Column { .. } | Expr::Aggregate(_) => false,
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

    fn boolean(value: bool) -> Expr {
        Expr::Literal(Value::Boolean(value))
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
            JoinAccess::NestedLoop | JoinAccess::HashJoin { .. } => {
                panic!("expected an index nested loop")
            }
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
            JoinAccess::NestedLoop | JoinAccess::HashJoin { .. } => {
                panic!("expected the earlier index")
            }
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
        match join_path(&equality, &[]) {
            JoinAccess::HashJoin {
                outer_ordinal,
                inner_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(outer_ordinal, 0);
                assert_eq!(inner_ordinal, 1);
                assert_eq!(predicate, "users.id = orders.user_id");
                assert_eq!(residual, None);
            }
            JoinAccess::NestedLoop | JoinAccess::IndexNestedLoop { .. } => {
                panic!("expected a hash join")
            }
        }
        let id_only = [IndexChoice {
            name: "idx_orders_id".to_string(),
            column: "id".to_string(),
            ordinal: 0,
        }];
        assert!(matches!(
            join_path(&equality, &id_only),
            JoinAccess::HashJoin {
                inner_ordinal: 1,
                ..
            }
        ));
    }

    #[test]
    fn hash_join_accepts_each_scalar_equality_and_keeps_a_residual() {
        let outer = vec![
            ("id".to_string(), DataType::Integer),
            ("name".to_string(), DataType::Text),
            ("active".to_string(), DataType::Boolean),
        ];
        let inner = vec![
            ("user_id".to_string(), DataType::Integer),
            ("label".to_string(), DataType::Text),
            ("paid".to_string(), DataType::Boolean),
        ];
        let text_eq = binary(
            qualified("users", "name"),
            BinOp::Eq,
            qualified("orders", "label"),
        );
        match choose_join("users", &outer, "orders", &inner, &text_eq, &[]) {
            JoinAccess::HashJoin {
                outer_ordinal,
                inner_ordinal,
                ..
            } => {
                assert_eq!(outer_ordinal, 1);
                assert_eq!(inner_ordinal, 1);
            }
            JoinAccess::NestedLoop | JoinAccess::IndexNestedLoop { .. } => {
                panic!("expected a text hash join")
            }
        }
        let flags = binary(
            qualified("orders", "paid"),
            BinOp::Eq,
            qualified("users", "active"),
        );
        assert!(matches!(
            choose_join("users", &outer, "orders", &inner, &flags, &[]),
            JoinAccess::HashJoin {
                outer_ordinal: 2,
                inner_ordinal: 2,
                ..
            }
        ));
        let wider = binary(
            binary(
                qualified("users", "name"),
                BinOp::Eq,
                qualified("orders", "label"),
            ),
            BinOp::And,
            binary(qualified("orders", "paid"), BinOp::Eq, boolean(true)),
        );
        match choose_join("users", &outer, "orders", &inner, &wider, &[]) {
            JoinAccess::HashJoin {
                predicate,
                residual,
                ..
            } => {
                assert_eq!(predicate, "users.name = orders.label");
                assert_eq!(
                    residual.as_deref(),
                    Some("users.name = orders.label AND orders.paid = TRUE")
                );
            }
            JoinAccess::NestedLoop | JoinAccess::IndexNestedLoop { .. } => {
                panic!("expected a residual hash join")
            }
        }
    }

    fn left_path(on: &Expr) -> LeftAccess {
        let tables = users_and_orders();
        choose_left_join("users", &tables.outer, "orders", &tables.inner, on, &[])
    }

    #[test]
    fn left_join_hashes_a_typed_equality_and_falls_back_otherwise() {
        let equality = binary(
            qualified("orders", "user_id"),
            BinOp::Eq,
            qualified("users", "id"),
        );
        match left_path(&equality) {
            LeftAccess::Hash {
                outer_ordinal,
                inner_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(outer_ordinal, 0);
                assert_eq!(inner_ordinal, 1);
                assert_eq!(predicate, "orders.user_id = users.id");
                assert_eq!(residual, None);
            }
            LeftAccess::NestedLoop | LeftAccess::Index { .. } => {
                panic!("expected a hash left join")
            }
        }
        let residual_on = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
            BinOp::And,
            binary(
                qualified("orders", "total"),
                BinOp::Gt,
                Expr::Literal(Value::Integer(100)),
            ),
        );
        match left_path(&residual_on) {
            LeftAccess::Hash {
                predicate,
                residual,
                ..
            } => {
                assert_eq!(predicate, "users.id = orders.user_id");
                assert_eq!(
                    residual.as_deref(),
                    Some("users.id = orders.user_id AND orders.total > 100")
                );
            }
            LeftAccess::NestedLoop | LeftAccess::Index { .. } => {
                panic!("expected a residual hash left join")
            }
        }
        let or_on = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
            BinOp::Or,
            binary(
                qualified("orders", "total"),
                BinOp::Eq,
                Expr::Literal(Value::Integer(5)),
            ),
        );
        assert!(matches!(left_path(&or_on), LeftAccess::NestedLoop));
        let range = binary(
            qualified("users", "id"),
            BinOp::Gt,
            qualified("orders", "user_id"),
        );
        assert!(matches!(left_path(&range), LeftAccess::NestedLoop));
        let nulls = Expr::IsNull(Box::new(qualified("orders", "user_id")));
        assert!(matches!(left_path(&nulls), LeftAccess::NestedLoop));
        let two = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
            BinOp::And,
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "id"),
            ),
        );
        match left_path(&two) {
            LeftAccess::Hash {
                outer_ordinal,
                inner_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(outer_ordinal, 0);
                assert_eq!(inner_ordinal, 1);
                assert_eq!(predicate, "users.id = orders.user_id");
                assert!(residual.is_some());
            }
            LeftAccess::NestedLoop | LeftAccess::Index { .. } => {
                panic!("the first equality is the hash key")
            }
        }
        let not_eq = binary(
            qualified("users", "id"),
            BinOp::NotEq,
            qualified("orders", "user_id"),
        );
        assert!(matches!(left_path(&not_eq), LeftAccess::NestedLoop));
        let or_inside = binary(
            binary(
                qualified("users", "id"),
                BinOp::Eq,
                qualified("orders", "user_id"),
            ),
            BinOp::And,
            binary(
                binary(
                    qualified("orders", "total"),
                    BinOp::Eq,
                    Expr::Literal(Value::Integer(1)),
                ),
                BinOp::Or,
                binary(
                    qualified("orders", "total"),
                    BinOp::Gt,
                    Expr::Literal(Value::Integer(2)),
                ),
            ),
        );
        assert!(matches!(left_path(&or_inside), LeftAccess::NestedLoop));
    }

    #[test]
    fn left_join_uses_the_earliest_right_index_and_the_size_rule() {
        let tables = users_and_orders();
        let indexes = order_indexes();
        let equality = binary(
            qualified("orders", "user_id"),
            BinOp::Eq,
            qualified("users", "id"),
        );
        match choose_left_join(
            "users",
            &tables.outer,
            "orders",
            &tables.inner,
            &equality,
            &indexes,
        ) {
            LeftAccess::Index {
                name,
                outer_ordinal,
                inner_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(name, "idx_orders_user");
                assert_eq!(outer_ordinal, 0);
                assert_eq!(inner_ordinal, 1);
                assert_eq!(predicate, "orders.user_id = users.id");
                assert_eq!(residual, None);
            }
            other => panic!("expected an index left join, got {other:?}"),
        }
        let kept = finish_left_access(
            choose_left_join(
                "users",
                &tables.outer,
                "orders",
                &tables.inner,
                &equality,
                &indexes,
            ),
            1,
            INDEX_PROBE_LEFT_FACTOR + 1,
        );
        assert!(matches!(kept, LeftAccess::Index { .. }));
        let hashed = finish_left_access(
            choose_left_join(
                "users",
                &tables.outer,
                "orders",
                &tables.inner,
                &equality,
                &indexes,
            ),
            1,
            INDEX_PROBE_LEFT_FACTOR,
        );
        assert!(matches!(hashed, LeftAccess::Hash { .. }));
        assert!(prefer_left_index_probe(0, 4));
        assert!(!prefer_left_index_probe(100, 100));
        assert!(!prefer_left_index_probe(10_000, 100));
        assert!(prefer_left_index_probe(100, 6_401));
        // `name` is TEXT and `user_id` is INTEGER. Different types are not a
        // hash key. A same-type TEXT or BOOLEAN equality still hashes.
        let mixed = binary(
            qualified("users", "name"),
            BinOp::Eq,
            qualified("orders", "user_id"),
        );
        assert!(matches!(
            choose_left_join(
                "users",
                &tables.outer,
                "orders",
                &tables.inner,
                &mixed,
                &indexes,
            ),
            LeftAccess::NestedLoop
        ));
        let or_on = binary(equality, BinOp::Or, Expr::Literal(Value::Boolean(true)));
        assert!(matches!(
            choose_left_join(
                "users",
                &tables.outer,
                "orders",
                &tables.inner,
                &or_on,
                &indexes,
            ),
            LeftAccess::NestedLoop
        ));
    }

    #[test]
    fn left_join_keeps_text_and_boolean_hashes_beside_an_integer_index() {
        let outer = vec![
            ("id".to_string(), DataType::Integer),
            ("name".to_string(), DataType::Text),
            ("active".to_string(), DataType::Boolean),
        ];
        let inner = vec![
            ("user_id".to_string(), DataType::Integer),
            ("label".to_string(), DataType::Text),
            ("paid".to_string(), DataType::Boolean),
            ("amount".to_string(), DataType::Integer),
        ];
        let user_index = [IndexChoice {
            name: "idx_orders_user".to_string(),
            column: "user_id".to_string(),
            ordinal: 0,
        }];
        let amount_index = [IndexChoice {
            name: "idx_orders_amount".to_string(),
            column: "amount".to_string(),
            ordinal: 3,
        }];
        let text_eq = binary(
            qualified("users", "name"),
            BinOp::Eq,
            qualified("orders", "label"),
        );
        let flags = binary(
            qualified("users", "active"),
            BinOp::Eq,
            qualified("orders", "paid"),
        );
        let ids = binary(
            qualified("users", "id"),
            BinOp::Eq,
            qualified("orders", "user_id"),
        );
        match choose_left_join("users", &outer, "orders", &inner, &text_eq, &user_index) {
            LeftAccess::Hash {
                outer_ordinal,
                inner_ordinal,
                predicate,
                residual,
            } => {
                assert_eq!(outer_ordinal, 1);
                assert_eq!(inner_ordinal, 1);
                assert_eq!(predicate, "users.name = orders.label");
                assert_eq!(residual, None);
            }
            other => panic!("text equality stays a hash, got {other:?}"),
        }
        match choose_left_join("users", &outer, "orders", &inner, &flags, &user_index) {
            LeftAccess::Hash {
                outer_ordinal,
                inner_ordinal,
                ..
            } => {
                assert_eq!(outer_ordinal, 2);
                assert_eq!(inner_ordinal, 2);
            }
            other => panic!("boolean equality stays a hash, got {other:?}"),
        }
        assert!(matches!(
            choose_left_join("users", &outer, "orders", &inner, &ids, &[]),
            LeftAccess::Hash {
                outer_ordinal: 0,
                inner_ordinal: 0,
                ..
            }
        ));
        assert!(matches!(
            choose_left_join("users", &outer, "orders", &inner, &ids, &amount_index),
            LeftAccess::Hash {
                inner_ordinal: 0,
                ..
            }
        ));
        match choose_left_join("users", &outer, "orders", &inner, &ids, &user_index) {
            LeftAccess::Index {
                name,
                inner_ordinal,
                outer_ordinal,
                ..
            } => {
                assert_eq!(name, "idx_orders_user");
                assert_eq!(outer_ordinal, 0);
                assert_eq!(inner_ordinal, 0);
            }
            other => panic!("indexed integer equality is a probe candidate, got {other:?}"),
        }
        let small = finish_left_access(
            choose_left_join("users", &outer, "orders", &inner, &ids, &user_index),
            100,
            100,
        );
        assert!(matches!(
            small,
            LeftAccess::Hash {
                outer_ordinal: 0,
                inner_ordinal: 0,
                ..
            }
        ));
        let wide = finish_left_access(
            choose_left_join("users", &outer, "orders", &inner, &ids, &user_index),
            100,
            10_000,
        );
        assert!(matches!(wide, LeftAccess::Index { name, .. } if name == "idx_orders_user"));
        let narrow = finish_left_access(
            choose_left_join("users", &outer, "orders", &inner, &ids, &user_index),
            10_000,
            100,
        );
        assert!(matches!(narrow, LeftAccess::Hash { .. }));
        let both = binary(text_eq.clone(), BinOp::And, ids.clone());
        let earlier_amount = [
            IndexChoice {
                name: "idx_orders_amount".to_string(),
                column: "amount".to_string(),
                ordinal: 3,
            },
            IndexChoice {
                name: "idx_orders_user".to_string(),
                column: "user_id".to_string(),
                ordinal: 0,
            },
        ];
        let amount_eq = binary(
            qualified("users", "id"),
            BinOp::Eq,
            qualified("orders", "amount"),
        );
        let two_integers = binary(ids, BinOp::And, amount_eq);
        match choose_left_join(
            "users",
            &outer,
            "orders",
            &inner,
            &two_integers,
            &earlier_amount,
        ) {
            LeftAccess::Index {
                name,
                inner_ordinal,
                predicate,
                residual,
                ..
            } => {
                assert_eq!(name, "idx_orders_amount");
                assert_eq!(inner_ordinal, 3);
                assert_eq!(predicate, "users.id = orders.amount");
                assert!(residual.is_some());
            }
            other => panic!("catalog order picks the amount index, got {other:?}"),
        }
        match choose_left_join("users", &outer, "orders", &inner, &both, &user_index) {
            LeftAccess::Index {
                name,
                inner_ordinal,
                predicate,
                residual,
                ..
            } => {
                assert_eq!(name, "idx_orders_user");
                assert_eq!(inner_ordinal, 0);
                assert_eq!(predicate, "users.id = orders.user_id");
                let residual = residual.expect("text equality remains");
                assert!(residual.contains("users.name = orders.label"), "{residual}");
            }
            other => panic!("integer index wins over an earlier text equality, got {other:?}"),
        }
        let hashed_text = finish_left_access(
            choose_left_join("users", &outer, "orders", &inner, &both, &user_index),
            2,
            2,
        );
        match hashed_text {
            LeftAccess::Hash {
                inner_ordinal,
                predicate,
                residual,
                ..
            } => {
                assert_eq!(inner_ordinal, 0);
                assert_eq!(predicate, "users.id = orders.user_id");
                assert!(residual.is_some());
            }
            other => panic!("a small indexed equality hashes those columns, got {other:?}"),
        }
    }
}

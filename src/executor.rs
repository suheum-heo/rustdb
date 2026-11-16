//! Execute parsed statements against the catalog.
//!
//! Filters are bound and type-checked before any row is read. `UPDATE` and
//! `DELETE` build the replacement rows and install them only after every row
//! has been planned, so a failure leaves the table unchanged. A durable
//! database publishes that result only after the statement succeeds.
//!
//! A plain `SELECT` applies `WHERE`, then `ORDER BY`, then `LIMIT`, then
//! projection. Its sort keys may name columns that are not in the select list.
//! `SELECT DISTINCT` is a non-aggregate query on one table or on one
//! `INNER JOIN`. It projects the chosen columns, then keeps the first copy of
//! each output row. NULL equals NULL in that comparison. A fingerprint
//! collision still compares every cell. `ORDER BY` names a selected output
//! column or its alias, and an unselected column is rejected because the
//! hidden value would be ambiguous after deduplication. `ORDER BY` and
//! `LIMIT` run after deduplication, and `LIMIT` does not stop the scan. The
//! distinct rows stay in memory until the query finishes. A distinct join
//! visits pairs through the same nested, hash, or index path as a plain join
//! and admits each projected pair immediately. It does not store the joined
//! rows beside the distinct set. An aggregate query groups after `WHERE` and applies `HAVING`
//! before the sort. An indexed integer comparison on one table may supply
//! candidate rows first. The original `WHERE` is still applied to those
//! candidates. Equal keys keep the order produced above because the sort is
//! stable. NULL is greater than every non-NULL value unless the query
//! overrides that with `NULLS FIRST` or `NULLS LAST`.
//! `UPDATE` and `DELETE` still read every row of one table.
//!
//! A `SELECT` may group rows from one table, one `INNER JOIN`, or one
//! `LEFT JOIN`. `WHERE`
//! runs first. `COUNT(*)` counts every remaining row, `COUNT(column)` skips
//! NULL, and `SUM` adds non-NULL integers with checked arithmetic. An empty
//! input with no `GROUP BY` returns one row. `GROUP BY` keeps NULL keys in one
//! group and emits groups in first-seen order. A group hash mixes a type tag
//! with each key, and a shared fingerprint still compares the stored cells, so
//! unequal tuples stay apart. The same aggregate call shares one accumulator
//! across the select list, `HAVING`, and `ORDER BY`. An unqualified `HAVING`
//! name uses a unique explicit select alias before an implicit output name,
//! then a grouped source column. A qualified name ignores aliases. `HAVING`
//! then keeps a group only when it evaluates to TRUE. `COUNT(DISTINCT column)`,
//! `SUM(DISTINCT column)`, and `AVG(DISTINCT column)` keep one set of values
//! per group. A new member is recorded only after every aggregate of that
//! input row succeeds. `ORDER BY` and `LIMIT` run after that. A joined
//! aggregate consumes each kept pair as it is produced and does not store the
//! pair. When a hash join builds the outer table, it still
//! buffers match positions so that first-seen order stays outer-then-inner.
//!
//! An `INNER JOIN` walks the outer table in insertion order. For each outer
//! row it reads inner candidates, then keeps a pair only when `ON` is TRUE.
//! `WHERE` then runs on that pair. A `LEFT JOIN` hashes the right table when
//! `ON` has a same-type column equality. The hash key skips NULL. Each exact
//! key still runs the original `ON`, so a residual predicate can reject a
//! candidate. A stored cell that is not NULL and not the column's declared
//! type is joined with the nested loop instead, because that comparison
//! raises a type error on pairs the hash key would otherwise skip. Any other
//! `ON` compares every left row with every right row.
//! Either way, left rows are visited in insertion order and matching right
//! rows are emitted in insertion order. `WHERE` runs on that pair. A left row
//! with no TRUE `ON` match is emitted once, with NULL for every right column,
//! and `WHERE` then sees that extended row. An aggregate left join feeds each
//! of those logical rows into the same group accumulator as an inner join.
//! `COUNT(*)` counts a null-extended row. `COUNT`, `SUM`, `AVG`, `MIN`, and
//! `MAX` of a right column ignore its NULLs. `SELECT DISTINCT` and `DISTINCT`
//! aggregates stay rejected. A `WHERE` that removes the matches
//! does not add the NULL row. `ON` FALSE and `ON` UNKNOWN do not match,
//! including `NULL = NULL`. `LIMIT` does not stop that scan. The left hash
//! table stores positions into the right rows, not copied keys, and it does
//! not buffer pairs. The right table is always the build side.
//! An indexed integer equality on an `INNER JOIN` uses the index
//! when the outer table is empty or the inner table has more than 64 times as
//! many rows. Otherwise that equality builds a hash table. Each bucket keeps
//! one group per distinct key, so a fingerprint collision does not mix keys.
//! NULL keys are left out. The original `ON` still runs on each matching key.
//! A stored `ON` cell that is not NULL and not the column's declared type
//! uses the nested loop for that execution, on both the hash path and the
//! index path, so a comparison error is not skipped. `EXPLAIN` still names
//! the plan chosen from the schema. Equal sizes build the inner table.
//! Matches are emitted in outer insertion order, then inner insertion order.
//! Any other shape reads the inner table from front to back. The catalog stores tables in one map, so a path that
//! needs both inputs copies one of them and borrows the other. A self-join
//! copies one side for the same reason. The index path copies the outer rows,
//! because each probe needs to borrow the database mutably. Pairs that fail
//! are not concatenated. With no `ORDER BY`, a match is projected immediately
//! when the inner table was built, and `LIMIT` stores only the rows it will
//! return; later pairs are still evaluated. Building the outer table buffers
//! the matching positions so they can be ordered before projection. `ORDER BY`
//! keeps the joined row until the sort. The join does not write the database.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::ast::{
    AggregateArg, AggregateCall, AggregateFunction, Assignment, Expr, JoinKind, OrderKey,
    OrderTarget, Projection, QueryFrom, SelectColumn, SelectItem, Statement, TableRef,
};
use crate::database::{Database, DurableChange};
use crate::error::DbError;
use crate::expr::{self, BoundExpr, Scope};
use crate::parser;
use crate::plan::{self, AccessPath, JoinAccess};
use crate::storage::IndexImage;
use crate::table::Table;
use crate::value::{self, DataType, Value};

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
    /// `UPDATE` matched this many rows.
    ///
    /// A row counts even when the assigned values equal the old ones.
    Update {
        /// Number of rows matched by `WHERE`.
        count: usize,
    },
    /// `DELETE` removed this many rows.
    Delete {
        /// Number of rows removed.
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
    /// Output rows, in the order produced by the query.
    pub rows: Vec<Vec<Value>>,
}

struct BoundAssignment {
    index: usize,
    value: BoundExpr,
}

struct SortKey {
    index: usize,
    ascending: bool,
    nulls_first: bool,
}

struct PreparedSelect {
    output_indexes: Vec<usize>,
    output_names: Vec<String>,
    filter: Option<BoundExpr>,
    sort_keys: Vec<SortKey>,
    path: AccessPath,
}

#[derive(Clone, Copy)]
struct SelectTail<'a> {
    projection: &'a Projection,
    selection: Option<&'a Expr>,
    group_by: &'a [SelectColumn],
    having: Option<&'a Expr>,
    order_by: &'a [OrderKey],
    limit: Option<u64>,
    distinct: bool,
}

struct PreparedJoin {
    on: BoundExpr,
    filter: Option<BoundExpr>,
    sort_keys: Vec<SortKey>,
    output_indexes: Vec<usize>,
    output_names: Vec<String>,
    path: JoinAccess,
}

struct IndexProbe {
    index: String,
    inner_ordinal: usize,
    outer_ordinal: usize,
}

struct HashKeys {
    outer_ordinal: usize,
    inner_ordinal: usize,
}

impl Database {
    /// Parse `sql` and run every statement in order.
    ///
    /// Statements are autocommitted one by one. If a later statement fails,
    /// earlier statements in the same string stay applied. Each individual
    /// `INSERT`, `UPDATE`, or `DELETE` is atomic. A durable database writes a
    /// redo log and checkpoints it before reporting success. A failure before
    /// the commit record is synced restores memory when the log is still
    /// incomplete. A failed sync of that record does not prove the record is
    /// absent: memory stays, the log stays, and further statements are refused
    /// until the file is reopened. A failure after a successful log sync does
    /// the same.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<StatementResult>, DbError> {
        if let Some(error) = self.fenced() {
            return Err(error);
        }
        let statements = parser::parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_statement(statement)?);
        }
        Ok(results)
    }

    fn execute_statement(&mut self, statement: Statement) -> Result<StatementResult, DbError> {
        if !self.stores_on_disk() {
            return self.apply(statement);
        }
        let change = durable_change(&statement);
        let undo = self.prepare_durable(&change);
        let result = self.apply(statement)?;
        if matches!(change, DurableChange::None) {
            return Ok(result);
        }
        if let Err(error) = self.publish() {
            if let crate::storage::StepError::BeforeCommit(_) = &error {
                self.rollback(undo);
            }
            return Err(error.into_db());
        }
        Ok(result)
    }

    fn apply(&mut self, statement: Statement) -> Result<StatementResult, DbError> {
        match statement {
            Statement::CreateTable { name, columns } => {
                self.create_table(name, columns)?;
                Ok(StatementResult::Ok)
            }
            Statement::DropTable { names } => {
                self.drop_tables(&names)?;
                Ok(StatementResult::Ok)
            }
            Statement::CreateIndex {
                name,
                table,
                column,
            } => {
                self.create_index(name, table, column)?;
                Ok(StatementResult::Ok)
            }
            Statement::DropIndex { names } => {
                self.drop_indexes(&names)?;
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
            Statement::Update {
                table,
                assignments,
                selection,
            } => self.update(&table, &assignments, selection.as_ref()),
            Statement::Delete { table, selection } => self.delete(&table, selection.as_ref()),
            Statement::Select(query) => self.query(
                &query.from,
                SelectTail {
                    projection: &query.projection,
                    selection: query.selection.as_ref(),
                    group_by: &query.group_by,
                    having: query.having.as_ref(),
                    order_by: &query.order_by,
                    limit: query.limit,
                    distinct: query.distinct,
                },
            ),
            Statement::Explain(inner) => self.explain(inner.as_ref()),
        }
    }

    fn query(
        &mut self,
        from: &QueryFrom,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        reject_runtime_distinct(from, tail)?;
        reject_left_join(from, tail)?;
        if tail.distinct {
            return match from {
                QueryFrom::Table(source) => self.query_table(source, tail),
                QueryFrom::Join {
                    kind: JoinKind::Inner,
                    outer,
                    inner,
                    on,
                } => self.query_distinct_join(outer, inner, on, tail),
                QueryFrom::Join {
                    kind: JoinKind::Left,
                    ..
                } => Err(DbError::unsupported(
                    "LEFT JOIN is not supported with DISTINCT",
                )),
            };
        }
        if is_aggregate_query(tail.projection, tail.group_by, tail.having, tail.order_by) {
            return match from {
                QueryFrom::Table(source) => self.aggregate_table(source, tail),
                QueryFrom::Join {
                    kind: JoinKind::Inner,
                    outer,
                    inner,
                    on,
                } => self.aggregate_join(outer, inner, on, tail),
                QueryFrom::Join {
                    kind: JoinKind::Left,
                    outer,
                    inner,
                    on,
                } => self.aggregate_left_join(outer, inner, on, tail),
            };
        }
        match from {
            QueryFrom::Table(source) => self.query_table(source, tail),
            QueryFrom::Join {
                kind: JoinKind::Inner,
                outer,
                inner,
                on,
            } => self.query_join(outer, inner, on, tail),
            QueryFrom::Join {
                kind: JoinKind::Left,
                outer,
                inner,
                on,
            } => self.query_left_join(outer, inner, on, tail),
        }
    }

    fn query_table(
        &mut self,
        source: &TableRef,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let mut prepared = self.prepare_select(
            source,
            tail.projection,
            tail.selection,
            tail.order_by,
            tail.distinct,
        )?;
        prepared.path = self.settle_path(prepared.path)?;
        let positions = match &prepared.path {
            AccessPath::SeqScan => None,
            AccessPath::IndexScan { .. } => {
                Some(self.index_positions(&source.name, &prepared.path)?)
            }
        };

        let table = self.table(&source.name)?;
        if tail.distinct {
            return finish_distinct(
                &source.name,
                table.rows(),
                positions.as_deref(),
                &prepared,
                tail.limit,
            );
        }
        let mut rows: Vec<&[Value]> = Vec::new();
        if let Some(positions) = positions {
            for position in positions {
                let Some(row) = table.rows().get(position) else {
                    return Err(DbError::Internal(format!(
                        "index position {position} is outside table '{}'",
                        source.name
                    )));
                };
                if expr::row_matches(prepared.filter.as_ref(), row)? {
                    rows.push(row.as_slice());
                }
            }
        } else {
            for row in table.rows() {
                if expr::row_matches(prepared.filter.as_ref(), row)? {
                    rows.push(row.as_slice());
                }
            }
        }
        if !prepared.sort_keys.is_empty() {
            rows.sort_by(|left, right| compare_rows(left, right, &prepared.sort_keys));
        }
        if let Some(limit) = tail.limit {
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            if rows.len() > limit {
                rows.truncate(limit);
            }
        }

        let mut projected = Vec::with_capacity(rows.len());
        for row in rows {
            projected.push(project_row(row, &prepared.output_indexes)?);
        }
        Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }))
    }

    fn aggregate_table(
        &mut self,
        source: &TableRef,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let mut plan = self.prepare_aggregate(source, tail)?;
        plan.path = self.settle_path(plan.path)?;
        let positions = match &plan.path {
            AccessPath::SeqScan => None,
            AccessPath::IndexScan { .. } => Some(self.index_positions(&source.name, &plan.path)?),
        };
        let table = self.table(&source.name)?;
        let grouped = collect_groups(table.rows(), positions.as_deref(), &plan)?;
        finish_aggregate(grouped, plan, tail.limit)
    }

    fn explain_aggregate(
        &mut self,
        source: &TableRef,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let mut plan = self.prepare_aggregate(source, tail)?;
        plan.path = self.settle_path(plan.path)?;
        let (scan, index, bounds) = plan::describe(&plan.path);
        Ok(StatementResult::Query(aggregate_explain_row(
            &plan,
            scan,
            index,
            bounds,
            tail.selection,
            tail.having,
        )))
    }

    fn prepare_aggregate(
        &self,
        source: &TableRef,
        tail: SelectTail<'_>,
    ) -> Result<AggPlan, DbError> {
        let indexes = self.index_images().to_vec();
        let table = self.table(&source.name)?;
        let scope = Scope::from_table(table, &source.alias);
        let choices = index_choices(table, &indexes)?;
        let path = plan::choose(&source.alias, tail.selection, &choices);
        prepare_groups(&scope, tail, path)
    }

    fn query_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let prepared = self.prepare_join(
            outer,
            inner,
            on,
            tail.projection,
            tail.selection,
            tail.order_by,
            false,
        )?;
        let mut prepared = prepared;
        #[cfg(test)]
        if force_nested_join() {
            prepared.path = JoinAccess::NestedLoop;
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        prepared.path = finish_join_path(prepared.path, outer_len, inner_len);
        match &prepared.path {
            JoinAccess::IndexNestedLoop {
                name,
                inner_ordinal,
                outer_ordinal,
                ..
            } => {
                let probe = IndexProbe {
                    index: name.clone(),
                    inner_ordinal: *inner_ordinal,
                    outer_ordinal: *outer_ordinal,
                };
                // Probes borrow the database, so the outer rows cannot stay borrowed
                // from the catalog. The copy is not updated.
                let outer_rows = self.table(&outer.name)?.rows().to_vec();
                if !self.on_inputs_allow_fast_join(outer, inner, &outer_rows, &prepared.on)? {
                    let inner_rows = self.copied_inner(outer, inner, &outer_rows)?;
                    return scan_join(&outer_rows, &inner_rows, prepared, tail.limit);
                }
                self.index_nested_loop(&inner.name, &probe, &outer_rows, prepared, tail.limit)
            }
            JoinAccess::HashJoin {
                outer_ordinal,
                inner_ordinal,
                ..
            } => {
                let keys = HashKeys {
                    outer_ordinal: *outer_ordinal,
                    inner_ordinal: *inner_ordinal,
                };
                self.hash_join(outer, inner, keys, prepared, tail.limit)
            }
            JoinAccess::NestedLoop => {
                self.with_copied_side(outer, inner, |outer_rows, inner_rows, _| {
                    scan_join(outer_rows, inner_rows, prepared, tail.limit)
                })
            }
        }
    }

    /// `LEFT JOIN`. An equality hashes the right table. Anything else is a nested loop.
    fn query_left_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let prepared = self.prepare_join(
            outer,
            inner,
            on,
            tail.projection,
            tail.selection,
            tail.order_by,
            false,
        )?;
        let path = self.left_access(outer, inner, on)?;
        let outer_types = self
            .table(&outer.name)?
            .schema()
            .columns()
            .iter()
            .map(crate::schema::Column::data_type)
            .collect::<Vec<_>>();
        let inner_types = self
            .table(&inner.name)?
            .schema()
            .columns()
            .iter()
            .map(crate::schema::Column::data_type)
            .collect::<Vec<_>>();
        let inner_width = inner_types.len();
        self.with_copied_side(outer, inner, |outer_rows, inner_rows, _| match path {
            plan::LeftAccess::NestedLoop => {
                scan_left_join(outer_rows, inner_rows, inner_width, prepared, tail.limit)
            }
            plan::LeftAccess::Hash {
                outer_ordinal,
                inner_ordinal,
                ..
            } => scan_hash_left_join(
                outer_rows,
                inner_rows,
                &outer_types,
                &inner_types,
                HashKeys {
                    outer_ordinal,
                    inner_ordinal,
                },
                prepared,
                tail.limit,
            ),
        })
    }

    /// The left-join strategy `EXPLAIN` and execution share.
    ///
    /// This reads schemas only. It does not hash rows or visit pairs.
    fn left_access(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
    ) -> Result<plan::LeftAccess, DbError> {
        #[cfg(test)]
        if force_nested_join() {
            return Ok(plan::LeftAccess::NestedLoop);
        }
        let outer_columns = Scope::columns(self.table(&outer.name)?);
        let inner_columns = Scope::columns(self.table(&inner.name)?);
        Ok(plan::choose_left_join(
            &outer.alias,
            &outer_columns,
            &inner.alias,
            &inner_columns,
            on,
        ))
    }

    fn explain_left_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        query: &crate::ast::SelectQuery,
    ) -> Result<StatementResult, DbError> {
        let prepared = self.prepare_join(
            outer,
            inner,
            on,
            &query.projection,
            query.selection.as_ref(),
            &query.order_by,
            false,
        )?;
        let path = self.left_access(outer, inner, on)?;
        let filter = query
            .selection
            .as_ref()
            .map(|expr| Value::Text(expr.render()))
            .unwrap_or(Value::Null);
        let order = if query.order_by.is_empty() {
            Value::Null
        } else {
            Value::Text(
                query
                    .order_by
                    .iter()
                    .map(render_order_key)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        let (access, build, predicate, residual) = match path {
            plan::LeftAccess::NestedLoop => (
                "LeftNestedLoopJoin",
                Value::Null,
                Value::Null,
                Value::Text(on.render()),
            ),
            plan::LeftAccess::Hash {
                predicate,
                residual,
                ..
            } => (
                "HashLeftJoin",
                Value::Text(inner.explain_name()),
                Value::Text(predicate),
                residual.map(Value::Text).unwrap_or(Value::Null),
            ),
        };
        Ok(StatementResult::Query(QueryResult {
            columns: vec![
                "access".to_string(),
                "outer".to_string(),
                "inner".to_string(),
                "build".to_string(),
                "predicate".to_string(),
                "residual".to_string(),
                "filter".to_string(),
                "projection".to_string(),
                "order_by".to_string(),
                "limit".to_string(),
            ],
            rows: vec![vec![
                Value::Text(access.to_string()),
                Value::Text(outer.explain_name()),
                Value::Text(inner.explain_name()),
                build,
                predicate,
                residual,
                filter,
                Value::Text(prepared.output_names.join(", ")),
                order,
                limit_cell(query.limit),
            ]],
        }))
    }

    fn query_distinct_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let mut prepared = self.prepare_join(
            outer,
            inner,
            on,
            tail.projection,
            tail.selection,
            tail.order_by,
            true,
        )?;
        #[cfg(test)]
        if force_nested_join() {
            prepared.path = JoinAccess::NestedLoop;
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        prepared.path = finish_join_path(prepared.path, outer_len, inner_len);
        let outputs = prepared.output_indexes.clone();
        let mut distinct = DistinctRows::new();
        self.visit_join_pairs(outer, inner, &prepared, |outer_row, inner_row| {
            distinct.admit(project_pair(outer_row, inner_row, &outputs)?)
        })?;
        Ok(finish_distinct_rows(
            distinct,
            &prepared.sort_keys,
            tail.limit,
            prepared.output_names,
        ))
    }

    fn aggregate_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let limit = tail.limit;
        let (plan, prepared) = self.plan_join_aggregate(outer, inner, on, tail)?;
        let mut sink = GroupSink::new(&plan);
        self.feed_join(outer, inner, &prepared, &mut sink)?;
        finish_aggregate(sink.finish()?, plan, limit)
    }

    /// Aggregate the logical rows of a `LEFT JOIN`.
    ///
    /// Each `ON`-true pair is one input. A left row with no such pair is one
    /// null-extended input when `WHERE` keeps it. `WHERE` does not invent a
    /// null row for a match it rejects. The rows are not stored. `LIMIT` runs
    /// after `HAVING` and the sort, so a later aggregate error is not hidden.
    fn aggregate_left_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let limit = tail.limit;
        let (plan, prepared, access) = self.plan_left_aggregate(outer, inner, on, tail)?;
        let mut sink = GroupSink::new(&plan);
        self.feed_left_aggregate(outer, inner, &prepared, &access, &mut sink)?;
        finish_aggregate(sink.finish()?, plan, limit)
    }

    /// Bind a left-join aggregate without reading joined rows.
    fn plan_left_aggregate(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<(AggPlan, PreparedJoin, plan::LeftAccess), DbError> {
        let outer_columns = Scope::columns(self.table(&outer.name)?);
        let inner_columns = Scope::columns(self.table(&inner.name)?);
        let scope = Scope::try_join(&outer.alias, outer_columns, &inner.alias, inner_columns)?;
        let bound_on = expr::bind_filter(on, &scope, "ON")?;
        let access = self.left_access(outer, inner, on)?;
        let mut plan = prepare_groups(&scope, tail, AccessPath::SeqScan)?;
        let filter = plan.filter.take();
        let prepared = PreparedJoin {
            on: bound_on,
            filter,
            sort_keys: Vec::new(),
            output_indexes: Vec::new(),
            output_names: Vec::new(),
            path: JoinAccess::NestedLoop,
        };
        Ok((plan, prepared, access))
    }

    fn feed_left_aggregate(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        prepared: &PreparedJoin,
        access: &plan::LeftAccess,
        sink: &mut GroupSink<'_>,
    ) -> Result<(), DbError> {
        let outer_types = self
            .table(&outer.name)?
            .schema()
            .columns()
            .iter()
            .map(crate::schema::Column::data_type)
            .collect::<Vec<_>>();
        let inner_types = self
            .table(&inner.name)?
            .schema()
            .columns()
            .iter()
            .map(crate::schema::Column::data_type)
            .collect::<Vec<_>>();
        self.with_copied_side(outer, inner, |outer_rows, inner_rows, _| {
            consume_left_join(
                outer_rows,
                inner_rows,
                &outer_types,
                &inner_types,
                access,
                prepared,
                |outer_row, inner_row| sink.push(InputRow::Pair(outer_row, inner_row)),
            )
        })
    }

    fn plan_join_aggregate(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<(AggPlan, PreparedJoin), DbError> {
        let outer_columns = Scope::columns(self.table(&outer.name)?);
        let inner_columns = Scope::columns(self.table(&inner.name)?);
        let scope = Scope::try_join(
            &outer.alias,
            outer_columns.clone(),
            &inner.alias,
            inner_columns.clone(),
        )?;
        let bound_on = expr::bind_filter(on, &scope, "ON")?;
        let indexes = self.index_images().to_vec();
        let choices = index_choices(self.table(&inner.name)?, &indexes)?;
        let mut path = plan::choose_join(
            &outer.alias,
            &outer_columns,
            &inner.alias,
            &inner_columns,
            on,
            &choices,
        );
        #[cfg(test)]
        if force_nested_join() {
            path = JoinAccess::NestedLoop;
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        path = finish_join_path(path, outer_len, inner_len);
        let mut plan = prepare_groups(&scope, tail, AccessPath::SeqScan)?;
        let filter = plan.filter.take();
        let prepared = PreparedJoin {
            on: bound_on,
            filter,
            sort_keys: Vec::new(),
            output_indexes: Vec::new(),
            output_names: Vec::new(),
            path,
        };
        Ok((plan, prepared))
    }

    fn feed_join(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        prepared: &PreparedJoin,
        sink: &mut GroupSink<'_>,
    ) -> Result<(), DbError> {
        #[cfg(test)]
        if materialize_aggregate() {
            let mut pairs = Vec::new();
            self.visit_join_pairs(outer, inner, prepared, |outer_row, inner_row| {
                pairs.push(concat_pair(outer_row, inner_row));
                Ok(())
            })?;
            for pair in &pairs {
                sink.push(InputRow::One(pair))?;
            }
            return Ok(());
        }
        self.visit_join_pairs(outer, inner, prepared, |outer_row, inner_row| {
            sink.push(InputRow::Pair(outer_row, inner_row))
        })
    }

    fn visit_join_pairs(
        &mut self,
        outer: &TableRef,
        inner: &TableRef,
        prepared: &PreparedJoin,
        mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        match feed_kind(&prepared.path) {
            FeedKind::Index {
                name,
                inner_ordinal,
                outer_ordinal,
            } => {
                let probe = IndexProbe {
                    index: name,
                    inner_ordinal,
                    outer_ordinal,
                };
                let outer_rows = self.table(&outer.name)?.rows().to_vec();
                if !self.on_inputs_allow_fast_join(outer, inner, &outer_rows, &prepared.on)? {
                    let inner_rows = self.copied_inner(outer, inner, &outer_rows)?;
                    return visit_nested_pairs(&outer_rows, &inner_rows, prepared, visit);
                }
                self.visit_index_pairs(&inner.name, &probe, &outer_rows, prepared, visit)
            }
            FeedKind::Hash {
                outer_ordinal,
                inner_ordinal,
            } => {
                let keys = HashKeys {
                    outer_ordinal,
                    inner_ordinal,
                };
                let outer_types = declared_types(self.table(&outer.name)?);
                let inner_types = declared_types(self.table(&inner.name)?);
                self.with_copied_side(outer, inner, |outer_rows, inner_rows, build_inner| {
                    if on_cells_allow_fast_join(
                        &outer_types,
                        &inner_types,
                        outer_rows,
                        inner_rows,
                        &prepared.on,
                    ) {
                        visit_hash_pairs(
                            outer_rows,
                            inner_rows,
                            keys,
                            build_inner,
                            prepared,
                            |outer_row, inner_row| visit(outer_row, inner_row),
                        )
                    } else {
                        visit_nested_pairs(outer_rows, inner_rows, prepared, visit)
                    }
                })
            }
            FeedKind::Nested => self.with_copied_side(outer, inner, |outer_rows, inner_rows, _| {
                visit_nested_pairs(outer_rows, inner_rows, prepared, |outer_row, inner_row| {
                    visit(outer_row, inner_row)
                })
            }),
        }
    }

    /// Whether every `ON` cell is NULL or its declared type.
    ///
    /// The check reads cells already loaded for the join. It does not copy
    /// them again and it does not write the database. A mismatch means this
    /// execution should use the nested loop.
    fn on_inputs_allow_fast_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        outer_rows: &[Vec<Value>],
        on: &BoundExpr,
    ) -> Result<bool, DbError> {
        let outer_types = declared_types(self.table(&outer.name)?);
        let inner_types = declared_types(self.table(&inner.name)?);
        let inner_rows = if outer.name == inner.name {
            outer_rows
        } else {
            self.table(&inner.name)?.rows()
        };
        Ok(on_cells_allow_fast_join(
            &outer_types,
            &inner_types,
            outer_rows,
            inner_rows,
            on,
        ))
    }

    fn copied_inner(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        outer_rows: &[Vec<Value>],
    ) -> Result<Vec<Vec<Value>>, DbError> {
        if outer.name == inner.name {
            Ok(outer_rows.to_vec())
        } else {
            Ok(self.table(&inner.name)?.rows().to_vec())
        }
    }

    /// Copy the smaller input so the other can be borrowed from the catalog.
    ///
    /// A self-join copies the only table and uses that copy for both sides.
    /// The bool is true when the copy is the inner table, which is also the
    /// hash-join build side for equal sizes.
    fn with_copied_side<T>(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        body: impl FnOnce(&[Vec<Value>], &[Vec<Value>], bool) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        if outer.name == inner.name {
            let rows = self.table(&outer.name)?.rows().to_vec();
            return body(&rows, &rows, true);
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        if inner_len <= outer_len {
            let inner_rows = self.table(&inner.name)?.rows().to_vec();
            let outer_rows = self.table(&outer.name)?.rows();
            body(outer_rows, &inner_rows, true)
        } else {
            let outer_rows = self.table(&outer.name)?.rows().to_vec();
            let inner_rows = self.table(&inner.name)?.rows();
            body(&outer_rows, inner_rows, false)
        }
    }

    fn explain_aggregate_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let (plan, prepared) = self.plan_join_aggregate(outer, inner, on, tail)?;
        let filter = tail
            .selection
            .map(Expr::render)
            .map(Value::Text)
            .unwrap_or(Value::Null);
        let having = tail
            .having
            .map(Expr::render)
            .map(Value::Text)
            .unwrap_or(Value::Null);
        let groups = text_list(&plan.group_labels);
        let aggregates = text_list(&plan.aggregate_labels);
        let (join, build, index, predicate, residual) = match &prepared.path {
            JoinAccess::NestedLoop => (
                "NestedLoopJoin",
                Value::Null,
                Value::Null,
                Value::Text(on.render()),
                Value::Null,
            ),
            JoinAccess::HashJoin {
                predicate,
                residual,
                ..
            } => {
                let outer_len = self.table(&outer.name)?.rows().len();
                let inner_len = self.table(&inner.name)?.rows().len();
                let build = if inner_len <= outer_len {
                    inner.explain_name()
                } else {
                    outer.explain_name()
                };
                (
                    "HashJoin",
                    Value::Text(build),
                    Value::Null,
                    Value::Text(predicate.clone()),
                    residual
                        .as_ref()
                        .map(|text| Value::Text(text.clone()))
                        .unwrap_or(Value::Null),
                )
            }
            JoinAccess::IndexNestedLoop {
                name,
                predicate,
                residual,
                ..
            } => (
                "IndexNestedLoopJoin",
                Value::Null,
                Value::Text(name.clone()),
                Value::Text(predicate.clone()),
                residual
                    .as_ref()
                    .map(|text| Value::Text(text.clone()))
                    .unwrap_or(Value::Null),
            ),
        };
        Ok(StatementResult::Query(QueryResult {
            columns: vec![
                "access".to_string(),
                "groups".to_string(),
                "aggregates".to_string(),
                "having".to_string(),
                "join".to_string(),
                "outer".to_string(),
                "inner".to_string(),
                "build".to_string(),
                "index".to_string(),
                "predicate".to_string(),
                "residual".to_string(),
                "filter".to_string(),
                "slots".to_string(),
                "aliases".to_string(),
            ],
            rows: vec![vec![
                Value::Text(plan.access),
                groups,
                aggregates,
                having,
                Value::Text(join.to_string()),
                Value::Text(outer.explain_name()),
                Value::Text(inner.explain_name()),
                build,
                index,
                predicate,
                residual,
                filter,
                slot_text(&plan.slot_labels),
                text_list(&plan.alias_labels),
            ]],
        }))
    }

    /// Plan an aggregate `LEFT JOIN` without visiting rows or writing storage.
    fn explain_left_aggregate(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        tail: SelectTail<'_>,
        query: &crate::ast::SelectQuery,
    ) -> Result<StatementResult, DbError> {
        let (plan, _, access) = self.plan_left_aggregate(outer, inner, on, tail)?;
        let filter = tail
            .selection
            .map(Expr::render)
            .map(Value::Text)
            .unwrap_or(Value::Null);
        let having = tail
            .having
            .map(Expr::render)
            .map(Value::Text)
            .unwrap_or(Value::Null);
        let order = if query.order_by.is_empty() {
            Value::Null
        } else {
            Value::Text(
                query
                    .order_by
                    .iter()
                    .map(render_order_key)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        let (strategy, build, predicate, residual) = match access {
            plan::LeftAccess::NestedLoop => (
                "LeftNestedLoopJoin",
                Value::Null,
                Value::Null,
                Value::Text(on.render()),
            ),
            plan::LeftAccess::Hash {
                predicate,
                residual,
                ..
            } => (
                "HashLeftJoin",
                Value::Text(inner.explain_name()),
                Value::Text(predicate),
                residual.map(Value::Text).unwrap_or(Value::Null),
            ),
        };
        Ok(StatementResult::Query(QueryResult {
            columns: vec![
                "access".to_string(),
                "outer".to_string(),
                "inner".to_string(),
                "build".to_string(),
                "predicate".to_string(),
                "residual".to_string(),
                "filter".to_string(),
                "groups".to_string(),
                "aggregates".to_string(),
                "having".to_string(),
                "projection".to_string(),
                "order_by".to_string(),
                "limit".to_string(),
            ],
            rows: vec![vec![
                Value::Text(strategy.to_string()),
                Value::Text(outer.explain_name()),
                Value::Text(inner.explain_name()),
                build,
                predicate,
                residual,
                filter,
                text_list(&plan.group_labels),
                text_list(&plan.aggregate_labels),
                having,
                Value::Text(plan.output_names.join(", ")),
                order,
                limit_cell(query.limit),
            ]],
        }))
    }

    fn explain(&mut self, statement: &Statement) -> Result<StatementResult, DbError> {
        let Statement::Select(query) = statement else {
            return Err(DbError::unsupported("EXPLAIN is only supported for SELECT"));
        };
        let tail = SelectTail {
            projection: &query.projection,
            selection: query.selection.as_ref(),
            group_by: &query.group_by,
            having: query.having.as_ref(),
            order_by: &query.order_by,
            limit: None,
            distinct: query.distinct,
        };
        reject_runtime_distinct(&query.from, tail)?;
        reject_left_join(&query.from, tail)?;
        if query.distinct {
            return match &query.from {
                QueryFrom::Table(source) => self.explain_distinct(source, query),
                QueryFrom::Join {
                    kind: JoinKind::Inner,
                    outer,
                    inner,
                    on,
                } => self.explain_distinct_join(outer, inner, on, query),
                QueryFrom::Join {
                    kind: JoinKind::Left,
                    ..
                } => Err(DbError::unsupported(
                    "LEFT JOIN is not supported with DISTINCT",
                )),
            };
        }
        if is_aggregate_query(
            &query.projection,
            &query.group_by,
            query.having.as_ref(),
            &query.order_by,
        ) {
            return match &query.from {
                QueryFrom::Table(source) => self.explain_aggregate(source, tail),
                QueryFrom::Join {
                    kind: JoinKind::Inner,
                    outer,
                    inner,
                    on,
                } => self.explain_aggregate_join(outer, inner, on, tail),
                QueryFrom::Join {
                    kind: JoinKind::Left,
                    outer,
                    inner,
                    on,
                } => self.explain_left_aggregate(outer, inner, on, tail, query),
            };
        }
        match &query.from {
            QueryFrom::Table(source) => {
                let mut prepared = self.prepare_select(
                    source,
                    &query.projection,
                    query.selection.as_ref(),
                    &query.order_by,
                    false,
                )?;
                prepared.path = self.settle_path(prepared.path)?;
                let (access, index, bounds) = plan::describe(&prepared.path);
                Ok(StatementResult::Query(QueryResult {
                    columns: vec![
                        "access".to_string(),
                        "index".to_string(),
                        "bounds".to_string(),
                    ],
                    rows: vec![vec![access, index, bounds]],
                }))
            }
            QueryFrom::Join {
                kind: JoinKind::Left,
                outer,
                inner,
                on,
            } => self.explain_left_join(outer, inner, on, query),
            QueryFrom::Join {
                kind: JoinKind::Inner,
                outer,
                inner,
                on,
            } => {
                let mut prepared = self.prepare_join(
                    outer,
                    inner,
                    on,
                    &query.projection,
                    query.selection.as_ref(),
                    &query.order_by,
                    false,
                )?;
                #[cfg(test)]
                if force_nested_join() {
                    prepared.path = JoinAccess::NestedLoop;
                }
                let outer_len = self.table(&outer.name)?.rows().len();
                let inner_len = self.table(&inner.name)?.rows().len();
                prepared.path = finish_join_path(prepared.path, outer_len, inner_len);
                let filter = query
                    .selection
                    .as_ref()
                    .map(|expr| Value::Text(expr.render()))
                    .unwrap_or(Value::Null);
                let (columns, row) = match &prepared.path {
                    JoinAccess::NestedLoop => (
                        vec![
                            "access".to_string(),
                            "outer".to_string(),
                            "inner".to_string(),
                            "condition".to_string(),
                            "filter".to_string(),
                        ],
                        vec![
                            Value::Text("NestedLoopJoin".to_string()),
                            Value::Text(outer.explain_name()),
                            Value::Text(inner.explain_name()),
                            Value::Text(on.render()),
                            filter,
                        ],
                    ),
                    JoinAccess::HashJoin {
                        predicate,
                        residual,
                        ..
                    } => {
                        let outer_len = self.table(&outer.name)?.rows().len();
                        let inner_len = self.table(&inner.name)?.rows().len();
                        let build = if inner_len <= outer_len {
                            inner.explain_name()
                        } else {
                            outer.explain_name()
                        };
                        (
                            vec![
                                "access".to_string(),
                                "outer".to_string(),
                                "inner".to_string(),
                                "build".to_string(),
                                "predicate".to_string(),
                                "residual".to_string(),
                                "filter".to_string(),
                            ],
                            vec![
                                Value::Text("HashJoin".to_string()),
                                Value::Text(outer.explain_name()),
                                Value::Text(inner.explain_name()),
                                Value::Text(build),
                                Value::Text(predicate.clone()),
                                residual
                                    .as_ref()
                                    .map(|text| Value::Text(text.clone()))
                                    .unwrap_or(Value::Null),
                                filter,
                            ],
                        )
                    }
                    JoinAccess::IndexNestedLoop {
                        name,
                        predicate,
                        residual,
                        ..
                    } => (
                        vec![
                            "access".to_string(),
                            "outer".to_string(),
                            "inner".to_string(),
                            "index".to_string(),
                            "predicate".to_string(),
                            "residual".to_string(),
                            "filter".to_string(),
                        ],
                        vec![
                            Value::Text("IndexNestedLoopJoin".to_string()),
                            Value::Text(outer.explain_name()),
                            Value::Text(inner.explain_name()),
                            Value::Text(name.clone()),
                            Value::Text(predicate.clone()),
                            residual
                                .as_ref()
                                .map(|text| Value::Text(text.clone()))
                                .unwrap_or(Value::Null),
                            filter,
                        ],
                    ),
                };
                Ok(StatementResult::Query(QueryResult {
                    columns,
                    rows: vec![row],
                }))
            }
        }
    }

    fn explain_distinct(
        &mut self,
        source: &TableRef,
        query: &crate::ast::SelectQuery,
    ) -> Result<StatementResult, DbError> {
        let mut prepared = self.prepare_select(
            source,
            &query.projection,
            query.selection.as_ref(),
            &query.order_by,
            true,
        )?;
        prepared.path = self.settle_path(prepared.path)?;
        let (access, index, bounds) = plan::describe(&prepared.path);
        let filter = query
            .selection
            .as_ref()
            .map(|expr| Value::Text(expr.render()))
            .unwrap_or(Value::Null);
        let order = if query.order_by.is_empty() {
            Value::Null
        } else {
            Value::Text(
                query
                    .order_by
                    .iter()
                    .map(render_order_key)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        Ok(StatementResult::Query(QueryResult {
            columns: vec![
                "access".to_string(),
                "index".to_string(),
                "bounds".to_string(),
                "distinct".to_string(),
                "projection".to_string(),
                "filter".to_string(),
                "order_by".to_string(),
                "limit".to_string(),
            ],
            rows: vec![vec![
                access,
                index,
                bounds,
                Value::Text("HashDistinct".to_string()),
                Value::Text(projection_label(&query.projection)),
                filter,
                order,
                limit_cell(query.limit),
            ]],
        }))
    }

    fn explain_distinct_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        query: &crate::ast::SelectQuery,
    ) -> Result<StatementResult, DbError> {
        let mut prepared = self.prepare_join(
            outer,
            inner,
            on,
            &query.projection,
            query.selection.as_ref(),
            &query.order_by,
            true,
        )?;
        #[cfg(test)]
        if force_nested_join() {
            prepared.path = JoinAccess::NestedLoop;
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        prepared.path = finish_join_path(prepared.path, outer_len, inner_len);
        let filter = query
            .selection
            .as_ref()
            .map(|expr| Value::Text(expr.render()))
            .unwrap_or(Value::Null);
        let order = if query.order_by.is_empty() {
            Value::Null
        } else {
            Value::Text(
                query
                    .order_by
                    .iter()
                    .map(render_order_key)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        let (access, build, index, predicate, residual) = match &prepared.path {
            JoinAccess::NestedLoop => (
                "NestedLoopJoin",
                Value::Null,
                Value::Null,
                Value::Text(on.render()),
                Value::Null,
            ),
            JoinAccess::HashJoin {
                predicate,
                residual,
                ..
            } => {
                let build = if inner_len <= outer_len {
                    inner.explain_name()
                } else {
                    outer.explain_name()
                };
                (
                    "HashJoin",
                    Value::Text(build),
                    Value::Null,
                    Value::Text(predicate.clone()),
                    residual
                        .as_ref()
                        .map(|text| Value::Text(text.clone()))
                        .unwrap_or(Value::Null),
                )
            }
            JoinAccess::IndexNestedLoop {
                name,
                predicate,
                residual,
                ..
            } => (
                "IndexNestedLoopJoin",
                Value::Null,
                Value::Text(name.clone()),
                Value::Text(predicate.clone()),
                residual
                    .as_ref()
                    .map(|text| Value::Text(text.clone()))
                    .unwrap_or(Value::Null),
            ),
        };
        Ok(StatementResult::Query(QueryResult {
            columns: vec![
                "access".to_string(),
                "outer".to_string(),
                "inner".to_string(),
                "build".to_string(),
                "index".to_string(),
                "predicate".to_string(),
                "residual".to_string(),
                "filter".to_string(),
                "distinct".to_string(),
                "projection".to_string(),
                "order_by".to_string(),
                "limit".to_string(),
            ],
            rows: vec![vec![
                Value::Text(access.to_string()),
                Value::Text(outer.explain_name()),
                Value::Text(inner.explain_name()),
                build,
                index,
                predicate,
                residual,
                filter,
                Value::Text("HashDistinct".to_string()),
                Value::Text(projection_label(&query.projection)),
                order,
                limit_cell(query.limit),
            ]],
        }))
    }

    fn prepare_select(
        &self,
        source: &TableRef,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
        distinct: bool,
    ) -> Result<PreparedSelect, DbError> {
        let indexes = self.index_images().to_vec();
        let table = self.table(&source.name)?;
        let scope = Scope::from_table(table, &source.alias);
        let outputs = output_columns(&scope, projection)?;
        let filter = bind_optional_where(selection, &scope)?;
        let sort_keys = if distinct {
            bind_distinct_order(&scope, order_by, &outputs)?
        } else {
            bind_order(&scope, order_by, &outputs)?
        };
        let choices = index_choices(table, &indexes)?;
        let path = plan::choose(&source.alias, selection, &choices);
        Ok(PreparedSelect {
            output_indexes: outputs.indexes,
            output_names: outputs.names,
            filter,
            sort_keys,
            path,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
        distinct: bool,
    ) -> Result<PreparedJoin, DbError> {
        let outer_columns = Scope::columns(self.table(&outer.name)?);
        let inner_columns = Scope::columns(self.table(&inner.name)?);
        let scope = Scope::try_join(
            &outer.alias,
            outer_columns.clone(),
            &inner.alias,
            inner_columns.clone(),
        )?;
        let bound_on = expr::bind_filter(on, &scope, "ON")?;
        let filter = bind_optional_where(selection, &scope)?;
        let outputs = output_columns(&scope, projection)?;
        let sort_keys = if distinct {
            bind_distinct_order(&scope, order_by, &outputs)?
        } else {
            bind_order(&scope, order_by, &outputs)?
        };
        let indexes = self.index_images().to_vec();
        let choices = index_choices(self.table(&inner.name)?, &indexes)?;
        let path = plan::choose_join(
            &outer.alias,
            &outer_columns,
            &inner.alias,
            &inner_columns,
            on,
            &choices,
        );
        Ok(PreparedJoin {
            on: bound_on,
            filter,
            sort_keys,
            output_indexes: outputs.indexes,
            output_names: outputs.names,
            path,
        })
    }

    /// Keep an index range only when it might skip stored keys.
    ///
    /// Equality stays an index lookup. A range that includes every `i64`, or
    /// every key stored in that index, is a sequential scan: the tree would
    /// return every indexed row and the executor would sort those rows back
    /// into insertion order. A corrupt edge page is returned as an error.
    /// The sequential choice is not a fallback after a failed lookup.
    fn settle_path(&mut self, path: AccessPath) -> Result<AccessPath, DbError> {
        let AccessPath::IndexScan {
            name,
            column,
            ordinal,
            bounds,
        } = path
        else {
            return Ok(AccessPath::SeqScan);
        };
        if plan::range_covers_every_i64(bounds) {
            return Ok(AccessPath::SeqScan);
        }
        if matches!(bounds, plan::Bounds::Range { .. }) {
            let span = self.index_key_span(&name)?;
            if plan::range_covers_stored_keys(bounds, span) {
                return Ok(AccessPath::SeqScan);
            }
        }
        Ok(AccessPath::IndexScan {
            name,
            column,
            ordinal,
            bounds,
        })
    }

    fn index_positions(
        &mut self,
        table_name: &str,
        path: &AccessPath,
    ) -> Result<Vec<usize>, DbError> {
        let AccessPath::IndexScan {
            name,
            ordinal,
            bounds,
            ..
        } = path
        else {
            return Err(DbError::Internal(
                "sequential scan has no index positions".to_string(),
            ));
        };
        if matches!(bounds, plan::Bounds::Empty) {
            return Ok(Vec::new());
        }
        let ordinal = *ordinal;
        let index_name = name.clone();
        let (equal, low, high) = match *bounds {
            plan::Bounds::Empty => (None, None, None),
            plan::Bounds::Equal(key) => (Some(key), None, None),
            plan::Bounds::Range { low, high } => (None, low, high),
        };
        self.indexed_positions(table_name, &index_name, ordinal, equal, low, high)
    }

    /// Row positions for one index lookup, in insertion order.
    ///
    /// Each returned row id must exist, must sit at that position, and must
    /// store `key` in the indexed column. A repeated row id is corrupt. An
    /// empty tree is corrupt when the table still stores an integer in that
    /// column. A broken leaf link fails the lookup. Positions are sorted so
    /// duplicate keys keep insertion order. The caller does not replace a
    /// failed lookup with a scan of the table.
    fn indexed_positions(
        &mut self,
        table_name: &str,
        index_name: &str,
        ordinal: usize,
        equal: Option<i64>,
        low: Option<i64>,
        high: Option<i64>,
    ) -> Result<Vec<usize>, DbError> {
        let (root, entries) = self.read_indexed_keys(index_name, equal, low, high)?;

        let table = self.table(table_name)?;
        if root == 0 {
            let stored = table
                .rows()
                .iter()
                .any(|row| matches!(row.get(ordinal), Some(Value::Integer(_))));
            if stored {
                return Err(corrupt_index(index_name));
            }
            return Ok(Vec::new());
        }

        let mut positions = Vec::with_capacity(entries.len());
        let mut seen = HashSet::with_capacity(entries.len());
        for (key, row_id) in entries {
            if !seen.insert(row_id) {
                return Err(corrupt_index(index_name));
            }
            let Some(position) = table.position(row_id) else {
                return Err(corrupt_index(index_name));
            };
            if table.row_id_at(position) != Some(row_id) {
                return Err(corrupt_index(index_name));
            }
            match table.rows().get(position).and_then(|row| row.get(ordinal)) {
                Some(Value::Integer(stored)) if *stored == key => positions.push(position),
                _ => return Err(corrupt_index(index_name)),
            }
        }
        positions.sort_unstable();
        Ok(positions)
    }

    fn index_nested_loop(
        &mut self,
        inner_name: &str,
        probe: &IndexProbe,
        outer_rows: &[Vec<Value>],
        prepared: PreparedJoin,
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        let limit_n = limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
        if prepared.sort_keys.is_empty() {
            let mut projected = Vec::new();
            let outputs = prepared.output_indexes.clone();
            self.visit_index_pairs(
                inner_name,
                probe,
                outer_rows,
                &prepared,
                |outer_row, inner_row| {
                    if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                        return Ok(());
                    }
                    projected.push(project_pair(outer_row, inner_row, &outputs)?);
                    Ok(())
                },
            )?;
            return Ok(StatementResult::Query(QueryResult {
                columns: prepared.output_names,
                rows: projected,
            }));
        }

        let mut joined = Vec::new();
        self.visit_index_pairs(
            inner_name,
            probe,
            outer_rows,
            &prepared,
            |outer_row, inner_row| {
                joined.push(concat_pair(outer_row, inner_row));
                Ok(())
            },
        )?;
        finish_rows(
            joined,
            &prepared.sort_keys,
            limit,
            &prepared.output_indexes,
            prepared.output_names,
        )
    }

    fn visit_index_pairs(
        &mut self,
        inner_name: &str,
        probe: &IndexProbe,
        outer_rows: &[Vec<Value>],
        prepared: &PreparedJoin,
        mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        for outer_row in outer_rows {
            self.visit_inner_matches(inner_name, probe, outer_row, prepared, |inner_row| {
                visit(outer_row, inner_row)
            })?;
        }
        Ok(())
    }

    /// Probe `probe` for one outer row and visit each inner match in insertion order.
    ///
    /// A NULL outer key produces no candidates. The full `ON` and `WHERE` still
    /// run on every candidate the index returns.
    fn visit_inner_matches(
        &mut self,
        inner_name: &str,
        probe: &IndexProbe,
        outer_row: &[Value],
        prepared: &PreparedJoin,
        mut visit: impl FnMut(&[Value]) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        let key = match outer_row.get(probe.outer_ordinal) {
            Some(Value::Integer(key)) => *key,
            Some(Value::Null) => return Ok(()),
            Some(other) => {
                return Err(DbError::Internal(format!(
                    "indexed join key is {other}, not an integer"
                )));
            }
            None => {
                return Err(DbError::Internal(
                    "indexed join key is outside the outer row".to_string(),
                ));
            }
        };
        let positions = self.indexed_positions(
            inner_name,
            &probe.index,
            probe.inner_ordinal,
            Some(key),
            None,
            None,
        )?;
        let table = self.table(inner_name)?;
        for position in positions {
            let Some(inner_row) = table.rows().get(position) else {
                return Err(DbError::Internal(format!(
                    "index position {position} is outside table '{inner_name}'"
                )));
            };
            if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
                continue;
            }
            if !expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)? {
                continue;
            }
            visit(inner_row)?;
        }
        Ok(())
    }

    fn hash_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        keys: HashKeys,
        prepared: PreparedJoin,
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        let outer_types = declared_types(self.table(&outer.name)?);
        let inner_types = declared_types(self.table(&inner.name)?);
        self.with_copied_side(outer, inner, |outer_rows, inner_rows, build_inner| {
            if on_cells_allow_fast_join(
                &outer_types,
                &inner_types,
                outer_rows,
                inner_rows,
                &prepared.on,
            ) {
                run_hash_join(outer_rows, inner_rows, keys, build_inner, prepared, limit)
            } else {
                scan_join(outer_rows, inner_rows, prepared, limit)
            }
        })
    }

    fn update(
        &mut self,
        table_name: &str,
        assignments: &[Assignment],
        selection: Option<&Expr>,
    ) -> Result<StatementResult, DbError> {
        let (new_rows, count) = {
            let table = self.table(table_name)?;
            let scope = Scope::from_table(table, table.name());
            let filter = bind_optional_where(selection, &scope)?;
            let assignments = bind_assignments(&scope, table_name, assignments)?;
            let mut new_rows = Vec::with_capacity(table.rows().len());
            let mut count = 0usize;
            for row in table.rows() {
                if !expr::row_matches(filter.as_ref(), row)? {
                    new_rows.push(row.clone());
                    continue;
                }
                new_rows.push(assign_row(row, &assignments)?);
                count += 1;
            }
            (new_rows, count)
        };
        self.table_mut(table_name)?.rewrite_all(new_rows)?;
        Ok(StatementResult::Update { count })
    }

    fn delete(
        &mut self,
        table_name: &str,
        selection: Option<&Expr>,
    ) -> Result<StatementResult, DbError> {
        let (kept_indexes, deleted) = {
            let table = self.table(table_name)?;
            let scope = Scope::from_table(table, table.name());
            let filter = bind_optional_where(selection, &scope)?;
            let mut kept_indexes = Vec::new();
            let mut deleted = 0usize;
            for (index, row) in table.rows().iter().enumerate() {
                if expr::row_matches(filter.as_ref(), row)? {
                    deleted += 1;
                } else {
                    kept_indexes.push(index);
                }
            }
            (kept_indexes, deleted)
        };
        self.table_mut(table_name)?.retain_indexes(&kept_indexes)?;
        Ok(StatementResult::Delete { count: deleted })
    }
}

fn durable_change(statement: &Statement) -> DurableChange {
    match statement {
        Statement::CreateTable { name, .. } => DurableChange::Create(name.clone()),
        Statement::DropTable { names } => DurableChange::Drop(names.clone()),
        Statement::Insert { table, .. }
        | Statement::Update { table, .. }
        | Statement::Delete { table, .. } => DurableChange::Write(table.clone()),
        Statement::CreateIndex { .. } | Statement::DropIndex { .. } => DurableChange::Index,
        Statement::Select(_) | Statement::Explain(_) => DurableChange::None,
    }
}

fn index_choices(table: &Table, indexes: &[IndexImage]) -> Result<Vec<plan::IndexChoice>, DbError> {
    let mut choices = Vec::new();
    for index in indexes {
        if index.table != table.name() {
            continue;
        }
        let Some(ordinal) = table.schema().index_of(&index.column) else {
            return Err(corrupt_index(&index.name));
        };
        let Some(column) = table.schema().column(ordinal) else {
            return Err(corrupt_index(&index.name));
        };
        if column.data_type() != DataType::Integer {
            return Err(corrupt_index(&index.name));
        }
        choices.push(plan::IndexChoice {
            name: index.name.clone(),
            column: index.column.clone(),
            ordinal,
        });
    }
    Ok(choices)
}

fn corrupt_index(name: &str) -> DbError {
    DbError::Storage(format!(
        "database file is corrupt: index '{name}' does not match its table"
    ))
}

struct AggPlan {
    group_ordinals: Vec<usize>,
    group_labels: Vec<String>,
    aggs: Vec<AggFn>,
    /// Indexes into a row laid out as group keys, then aggregate values.
    outputs: Vec<usize>,
    output_names: Vec<String>,
    /// One label per accumulator, in slot order. A shared call appears once.
    slot_labels: Vec<String>,
    /// Explicit select aliases and the group or aggregate slot they name.
    alias_labels: Vec<String>,
    aggregate_labels: Vec<String>,
    sort_keys: Vec<SortKey>,
    filter: Option<BoundExpr>,
    having: Option<BoundExpr>,
    path: AccessPath,
    access: String,
}

enum AggFn {
    CountStar,
    CountColumn(usize),
    CountDistinct(usize),
    Sum(usize),
    SumDistinct(usize),
    Avg(usize),
    AvgDistinct(usize),
    Min(usize),
    Max(usize),
}

impl AggFn {
    fn is_distinct(&self) -> bool {
        matches!(
            self,
            AggFn::CountDistinct(_) | AggFn::SumDistinct(_) | AggFn::AvgDistinct(_)
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
enum AccValue {
    Count(i64),
    Sum(Option<i64>),
    Avg { sum: i128, count: i64 },
    Extreme(Option<Value>),
}

struct AggGroup {
    key: Vec<Value>,
    acc: Vec<AccValue>,
    seen: Vec<Option<DistinctSet>>,
}

/// Exact values already admitted to one `DISTINCT` accumulator in one group.
///
/// The map key is a fingerprint. Each bucket stores the values so a hash
/// collision cannot merge unequal cells. A text value is cloned only while a
/// new admission is prepared, after every aggregate of that row has accepted
/// it, and the clone is dropped if the set cannot reserve room. The set is
/// not inside [`AccValue`], so a plain `COUNT` does not carry a map. There is
/// no disk spill: every distinct value of every group stays in memory until
/// the query finishes.
struct DistinctSet {
    buckets: HashMap<u64, Vec<Value>>,
}

impl DistinctSet {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.values().map(Vec::len).sum()
    }

    fn contains(&self, value: &Value) -> bool {
        self.buckets
            .get(&fingerprint(value))
            .is_some_and(|bucket| bucket.iter().any(|stored| same_cell(stored, value)))
    }

    /// Reserve the map and the collision bucket without storing `value`.
    ///
    /// `Some` is a new bucket that the caller owns until commit. `None` means
    /// the existing bucket has room for one more cell. A reservation failure
    /// leaves membership unchanged.
    fn prepare_admit(&mut self, value: &Value) -> Result<Option<Vec<Value>>, DbError> {
        let hash = fingerprint(value);
        if self.buckets.contains_key(&hash) {
            take_reserve_budget()?;
            let Some(bucket) = self.buckets.get_mut(&hash) else {
                return Err(DbError::Internal("DISTINCT bucket disappeared".to_string()));
            };
            bucket.try_reserve(1).map_err(|_| distinct_capacity())?;
            return Ok(None);
        }
        take_reserve_budget()?;
        self.buckets
            .try_reserve(1)
            .map_err(|_| distinct_capacity())?;
        let mut bucket = Vec::new();
        take_reserve_budget()?;
        bucket.try_reserve(1).map_err(|_| distinct_capacity())?;
        Ok(Some(bucket))
    }

    /// Store a value whose bucket was reserved by [`Self::prepare_admit`].
    ///
    /// `try_reserve` has already made the map and the bucket able to hold one
    /// more element. Push and insert must not grow those buffers: this runs
    /// after earlier slots in the same row may already have been published.
    fn commit_admit(&mut self, value: Value, fresh: Option<Vec<Value>>) {
        let hash = fingerprint(&value);
        if let Some(mut bucket) = fresh {
            debug_assert!(!self.buckets.contains_key(&hash));
            debug_assert!(bucket.capacity() > bucket.len());
            debug_assert!(self.buckets.capacity() > self.buckets.len());
            let bucket_capacity = bucket.capacity();
            let map_capacity = self.buckets.capacity();
            bucket.push(value);
            debug_assert_eq!(bucket.capacity(), bucket_capacity);
            self.buckets.insert(hash, bucket);
            debug_assert_eq!(self.buckets.capacity(), map_capacity);
            return;
        }
        let Some(bucket) = self.buckets.get_mut(&hash) else {
            unreachable!("DISTINCT bucket was not reserved");
        };
        debug_assert!(bucket.capacity() > bucket.len());
        debug_assert!(!bucket.iter().any(|stored| same_cell(stored, &value)));
        let bucket_capacity = bucket.capacity();
        bucket.push(value);
        debug_assert_eq!(bucket.capacity(), bucket_capacity);
    }
}

fn distinct_capacity() -> DbError {
    DbError::Internal("DISTINCT set is too large".to_string())
}

/// Test runs can fail the Nth set reservation before any group is modified.
fn take_reserve_budget() -> Result<(), DbError> {
    charge_reserve(distinct_capacity())
}

fn charge_reserve(error: DbError) -> Result<(), DbError> {
    #[cfg(test)]
    {
        let blocked = DISTINCT_RESERVE_BUDGET.with(|budget| {
            let Some(left) = budget.get() else {
                return false;
            };
            if left == 0 {
                return true;
            }
            budget.set(Some(left - 1));
            false
        });
        if blocked {
            return Err(error);
        }
    }
    #[cfg(not(test))]
    let _ = error;
    Ok(())
}

fn prepare_groups(
    scope: &Scope,
    tail: SelectTail<'_>,
    path: AccessPath,
) -> Result<AggPlan, DbError> {
    let filter = bind_optional_where(tail.selection, scope)?;
    let mut group_ordinals = Vec::with_capacity(tail.group_by.len());
    let mut group_labels = Vec::with_capacity(tail.group_by.len());
    for column in tail.group_by {
        group_ordinals.push(scope.resolve(column)?);
        group_labels.push(column.render());
    }
    let Projection::Columns(items) = tail.projection else {
        return Err(DbError::unsupported(
            "SELECT * is not supported with GROUP BY or aggregates",
        ));
    };
    let mut aggs = Vec::new();
    let mut slot_labels = Vec::new();
    let mut outputs = OutputPlan::new();
    let mut aggregate_labels = Vec::new();
    let mut explicit_aliases = HashSet::new();
    for item in items {
        match item {
            SelectItem::Column { column, alias } => {
                note_explicit_alias(&mut explicit_aliases, alias.as_deref())?;
                let ordinal = scope.resolve(column)?;
                let Some(slot) = group_ordinals.iter().position(|group| *group == ordinal) else {
                    return Err(DbError::invalid(format!(
                        "column '{}' must appear in GROUP BY",
                        column.name
                    )));
                };
                let Some(implicit) = scope.name_at(ordinal) else {
                    return Err(DbError::Internal(
                        "group column is outside the table".to_string(),
                    ));
                };
                outputs.push(
                    slot,
                    alias.clone().unwrap_or_else(|| implicit.to_string()),
                    alias.is_some(),
                    stored_scalar(scope, ordinal)?,
                );
            }
            SelectItem::Aggregate { call, alias } => {
                note_explicit_alias(&mut explicit_aliases, alias.as_deref())?;
                let (slot, scalar) = bind_shared(call, scope, &mut aggs, &mut slot_labels)?;
                remember_aggregate(call, &mut aggregate_labels);
                let index = group_ordinals.len() + slot;
                outputs.push(
                    index,
                    alias.clone().unwrap_or_else(|| call.render()),
                    alias.is_some(),
                    scalar,
                );
            }
        }
    }
    let group_width = group_ordinals.len();
    let having = tail
        .having
        .map(|expr| {
            expr::bind_having(
                expr,
                |column| {
                    if let Some(resolved) = having_output(column, &outputs)? {
                        return Ok(resolved);
                    }
                    let (slot, data_type) = grouped_column(scope, &group_ordinals, column)?;
                    Ok((slot, expr::ScalarType::from(data_type)))
                },
                |call| {
                    let (slot, scalar) = bind_shared(call, scope, &mut aggs, &mut slot_labels)?;
                    remember_aggregate(call, &mut aggregate_labels);
                    Ok((group_width + slot, scalar))
                },
            )
        })
        .transpose()?;
    let mut sort_keys = Vec::with_capacity(tail.order_by.len());
    for key in tail.order_by {
        let index = match &key.target {
            OrderTarget::Column(column) => order_output_index(column, &outputs, |column| {
                Ok(grouped_column(scope, &group_ordinals, column)?.0)
            })?,
            OrderTarget::Aggregate(call) => {
                let (slot, _) = bind_shared(call, scope, &mut aggs, &mut slot_labels)?;
                remember_aggregate(call, &mut aggregate_labels);
                group_width + slot
            }
        };
        sort_keys.push(SortKey {
            index,
            ascending: key.ascending,
            nulls_first: key.nulls_first.unwrap_or(!key.ascending),
        });
    }
    let access = if group_ordinals.is_empty() {
        "Aggregate"
    } else {
        "HashAggregate"
    };
    let alias_labels = alias_report(&outputs, &group_labels, &slot_labels);
    Ok(AggPlan {
        group_ordinals,
        group_labels,
        aggs,
        outputs: outputs.indexes,
        output_names: outputs.names,
        slot_labels,
        alias_labels,
        aggregate_labels,
        sort_keys,
        filter,
        having,
        path,
        access: access.to_string(),
    })
}

fn bind_shared(
    call: &AggregateCall,
    scope: &Scope,
    aggs: &mut Vec<AggFn>,
    slot_labels: &mut Vec<String>,
) -> Result<(usize, expr::ScalarType), DbError> {
    let before = aggs.len();
    let bound = bind_aggregate(call, scope, aggs)?;
    if aggs.len() != before {
        slot_labels.push(call.render());
    }
    Ok(bound)
}

fn alias_report(
    outputs: &OutputPlan,
    group_labels: &[String],
    slot_labels: &[String],
) -> Vec<String> {
    let mut labels = Vec::new();
    for (index, name) in outputs.names.iter().enumerate() {
        if !outputs.explicit[index] {
            continue;
        }
        let slot = outputs.indexes[index];
        if slot < group_labels.len() {
            labels.push(format!("{name} -> group {slot} {}", group_labels[slot]));
        } else {
            let aggregate = slot - group_labels.len();
            let rendered = slot_labels
                .get(aggregate)
                .map(String::as_str)
                .unwrap_or("aggregate");
            labels.push(format!("{name} -> aggregate {aggregate} {rendered}"));
        }
    }
    labels
}

fn grouped_column(
    scope: &Scope,
    group_ordinals: &[usize],
    column: &SelectColumn,
) -> Result<(usize, DataType), DbError> {
    let ordinal = scope.resolve(column)?;
    let Some(slot) = group_ordinals.iter().position(|group| *group == ordinal) else {
        return Err(DbError::invalid(format!(
            "column '{}' must appear in GROUP BY",
            column.name
        )));
    };
    let Some(data_type) = scope.type_at(ordinal) else {
        return Err(DbError::Internal(
            "group column is outside the table".to_string(),
        ));
    };
    Ok((slot, data_type))
}

fn remember_aggregate(call: &AggregateCall, labels: &mut Vec<String>) {
    let label = call.render();
    if !labels.iter().any(|existing| existing == &label) {
        labels.push(label);
    }
}

fn aggregate_explain_row(
    plan: &AggPlan,
    scan: Value,
    index: Value,
    bounds: Value,
    selection: Option<&Expr>,
    having: Option<&Expr>,
) -> QueryResult {
    let filter = selection
        .map(Expr::render)
        .map(Value::Text)
        .unwrap_or(Value::Null);
    let having = having
        .map(Expr::render)
        .map(Value::Text)
        .unwrap_or(Value::Null);
    QueryResult {
        columns: vec![
            "access".to_string(),
            "groups".to_string(),
            "aggregates".to_string(),
            "scan".to_string(),
            "index".to_string(),
            "bounds".to_string(),
            "filter".to_string(),
            "having".to_string(),
            "slots".to_string(),
            "aliases".to_string(),
        ],
        rows: vec![vec![
            Value::Text(plan.access.clone()),
            text_list(&plan.group_labels),
            text_list(&plan.aggregate_labels),
            scan,
            index,
            bounds,
            filter,
            having,
            slot_text(&plan.slot_labels),
            text_list(&plan.alias_labels),
        ]],
    }
}

fn slot_text(labels: &[String]) -> Value {
    if labels.is_empty() {
        return Value::Null;
    }
    Value::Text(
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| format!("{index}:{label}"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn text_list(labels: &[String]) -> Value {
    if labels.is_empty() {
        Value::Null
    } else {
        Value::Text(labels.join(", "))
    }
}

fn reject_left_join(from: &QueryFrom, tail: SelectTail<'_>) -> Result<(), DbError> {
    let QueryFrom::Join {
        kind: JoinKind::Left,
        ..
    } = from
    else {
        return Ok(());
    };
    if tail.distinct {
        return Err(DbError::unsupported(
            "LEFT JOIN is not supported with DISTINCT",
        ));
    }
    if crate::ast::has_distinct_aggregate(tail.projection, tail.having, tail.order_by) {
        return Err(DbError::unsupported(
            "LEFT JOIN is not supported with DISTINCT aggregates",
        ));
    }
    Ok(())
}

fn reject_runtime_distinct(_from: &QueryFrom, tail: SelectTail<'_>) -> Result<(), DbError> {
    if !tail.distinct {
        return Ok(());
    }
    if !tail.group_by.is_empty() {
        return Err(DbError::unsupported(
            "DISTINCT is not supported with GROUP BY",
        ));
    }
    if tail.having.is_some() {
        return Err(DbError::unsupported(
            "DISTINCT is not supported with HAVING",
        ));
    }
    if is_aggregate_query(tail.projection, tail.group_by, tail.having, tail.order_by) {
        return Err(DbError::unsupported(
            "DISTINCT is not supported with aggregate functions",
        ));
    }
    Ok(())
}

fn is_aggregate_query(
    projection: &Projection,
    group_by: &[SelectColumn],
    having: Option<&Expr>,
    order_by: &[OrderKey],
) -> bool {
    if having.is_some() || !group_by.is_empty() {
        return true;
    }
    if order_by
        .iter()
        .any(|key| matches!(key.target, OrderTarget::Aggregate(_)))
    {
        return true;
    }
    match projection {
        Projection::All | Projection::QualifiedAll { .. } | Projection::QualifiedStars { .. } => {
            false
        }
        Projection::Columns(items) => items
            .iter()
            .any(|item| matches!(item, SelectItem::Aggregate { .. })),
    }
}

fn bind_aggregate(
    call: &AggregateCall,
    scope: &Scope,
    aggs: &mut Vec<AggFn>,
) -> Result<(usize, expr::ScalarType), DbError> {
    let (function, scalar) = match call.function {
        AggregateFunction::Count => match &call.argument {
            AggregateArg::Star if call.distinct => {
                return Err(DbError::unsupported("DISTINCT requires a column"));
            }
            AggregateArg::Star => (AggFn::CountStar, expr::ScalarType::Integer),
            AggregateArg::Column(column) => {
                let ordinal = scope.resolve(column)?;
                let function = if call.distinct {
                    AggFn::CountDistinct(ordinal)
                } else {
                    AggFn::CountColumn(ordinal)
                };
                (function, expr::ScalarType::Integer)
            }
        },
        AggregateFunction::Sum => {
            let make = if call.distinct {
                AggFn::SumDistinct
            } else {
                AggFn::Sum
            };
            (
                integer_aggregate("SUM", call, scope, make)?,
                expr::ScalarType::Integer,
            )
        }
        AggregateFunction::Avg => {
            let make = if call.distinct {
                AggFn::AvgDistinct
            } else {
                AggFn::Avg
            };
            (
                integer_aggregate("AVG", call, scope, make)?,
                expr::ScalarType::Ratio,
            )
        }
        AggregateFunction::Min => extreme_aggregate("MIN", call, scope, true)?,
        AggregateFunction::Max => extreme_aggregate("MAX", call, scope, false)?,
    };
    if let Some(slot) = aggs
        .iter()
        .position(|existing| agg_same(existing, &function))
    {
        return Ok((slot, scalar));
    }
    aggs.push(function);
    Ok((aggs.len() - 1, scalar))
}

fn integer_aggregate(
    name: &str,
    call: &AggregateCall,
    scope: &Scope,
    make: fn(usize) -> AggFn,
) -> Result<AggFn, DbError> {
    let ordinal = aggregate_column(&format!("{name} requires an INTEGER column"), call, scope)?;
    let Some(data_type) = scope.type_at(ordinal) else {
        return Err(DbError::Internal(
            "aggregate column is outside the table".to_string(),
        ));
    };
    if data_type != DataType::Integer {
        return Err(DbError::TypeError(format!(
            "{name} requires INTEGER, not {data_type}"
        )));
    }
    Ok(make(ordinal))
}

fn extreme_aggregate(
    name: &str,
    call: &AggregateCall,
    scope: &Scope,
    min: bool,
) -> Result<(AggFn, expr::ScalarType), DbError> {
    if call.distinct {
        return Err(DbError::unsupported(format!(
            "DISTINCT is not supported for {name}"
        )));
    }
    let ordinal = aggregate_column(&format!("{name} requires a column"), call, scope)?;
    let Some(data_type) = scope.type_at(ordinal) else {
        return Err(DbError::Internal(
            "aggregate column is outside the table".to_string(),
        ));
    };
    let function = if min {
        AggFn::Min(ordinal)
    } else {
        AggFn::Max(ordinal)
    };
    Ok((function, expr::ScalarType::from(data_type)))
}

fn aggregate_column(message: &str, call: &AggregateCall, scope: &Scope) -> Result<usize, DbError> {
    let AggregateArg::Column(column) = &call.argument else {
        return Err(DbError::unsupported(message));
    };
    scope.resolve(column)
}

fn agg_same(left: &AggFn, right: &AggFn) -> bool {
    match (left, right) {
        (AggFn::CountStar, AggFn::CountStar) => true,
        (AggFn::CountColumn(left), AggFn::CountColumn(right)) => left == right,
        (AggFn::CountDistinct(left), AggFn::CountDistinct(right)) => left == right,
        (AggFn::Sum(left), AggFn::Sum(right)) => left == right,
        (AggFn::SumDistinct(left), AggFn::SumDistinct(right)) => left == right,
        (AggFn::Avg(left), AggFn::Avg(right)) => left == right,
        (AggFn::AvgDistinct(left), AggFn::AvgDistinct(right)) => left == right,
        (AggFn::Min(left), AggFn::Min(right)) => left == right,
        (AggFn::Max(left), AggFn::Max(right)) => left == right,
        _ => false,
    }
}

fn collect_groups(
    rows: &[Vec<Value>],
    positions: Option<&[usize]>,
    plan: &AggPlan,
) -> Result<Vec<Vec<Value>>, DbError> {
    let mut sink = GroupSink::new(plan);
    let mut visit = |row: &[Value]| sink.push(InputRow::One(row));
    visit_filtered(rows, positions, plan.filter.as_ref(), &mut visit)?;
    sink.finish()
}

/// One input row, or one joined pair, without concatenating the pair.
#[derive(Clone, Copy)]
enum InputRow<'a> {
    One(&'a [Value]),
    Pair(&'a [Value], &'a [Value]),
}

impl<'a> InputRow<'a> {
    fn get(self, index: usize) -> Option<&'a Value> {
        match self {
            Self::One(row) => row.get(index),
            Self::Pair(outer, inner) => {
                if index < outer.len() {
                    outer.get(index)
                } else {
                    inner.get(index - outer.len())
                }
            }
        }
    }
}

struct GroupSink<'a> {
    plan: &'a AggPlan,
    scalar: Option<Vec<AccValue>>,
    scalar_seen: Vec<Option<DistinctSet>>,
    groups: Vec<AggGroup>,
    buckets: HashMap<u64, Vec<usize>>,
}

impl<'a> GroupSink<'a> {
    fn new(plan: &'a AggPlan) -> Self {
        let scalar = if plan.group_ordinals.is_empty() {
            Some(fresh_acc(&plan.aggs))
        } else {
            None
        };
        let scalar_seen = if scalar.is_some() {
            fresh_seen(&plan.aggs)
        } else {
            Vec::new()
        };
        Self {
            plan,
            scalar,
            scalar_seen,
            groups: Vec::new(),
            buckets: HashMap::new(),
        }
    }

    fn push(&mut self, row: InputRow<'_>) -> Result<(), DbError> {
        if let Some(acc) = &mut self.scalar {
            return update_acc(acc, &mut self.scalar_seen, row, &self.plan.aggs);
        }
        absorb_row(row, self.plan, &mut self.groups, &mut self.buckets)
    }

    fn finish(self) -> Result<Vec<Vec<Value>>, DbError> {
        if let Some(acc) = self.scalar {
            return Ok(vec![materialize_group(&[], &acc)]);
        }
        Ok(self
            .groups
            .into_iter()
            .map(|group| materialize_group(&group.key, &group.acc))
            .collect())
    }
}

fn absorb_row(
    row: InputRow<'_>,
    plan: &AggPlan,
    groups: &mut Vec<AggGroup>,
    buckets: &mut HashMap<u64, Vec<usize>>,
) -> Result<(), DbError> {
    let hash = hash_group(row, &plan.group_ordinals)?;
    let mut matched = None;
    if let Some(slots) = buckets.get(&hash) {
        for &slot in slots {
            if group_equals(&groups[slot].key, row, &plan.group_ordinals)? {
                matched = Some(slot);
                break;
            }
        }
    }
    if let Some(slot) = matched {
        let group = &mut groups[slot];
        return update_acc(&mut group.acc, &mut group.seen, row, &plan.aggs);
    }
    let mut key = Vec::with_capacity(plan.group_ordinals.len());
    for &ordinal in &plan.group_ordinals {
        key.push(cell(row, ordinal)?.clone());
    }
    let mut acc = fresh_acc(&plan.aggs);
    let mut seen = fresh_seen(&plan.aggs);
    update_acc(&mut acc, &mut seen, row, &plan.aggs)?;
    let slot = groups.len();
    groups.push(AggGroup { key, acc, seen });
    buckets.entry(hash).or_default().push(slot);
    Ok(())
}

fn visit_filtered(
    rows: &[Vec<Value>],
    positions: Option<&[usize]>,
    filter: Option<&BoundExpr>,
    visit: &mut impl FnMut(&[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    if let Some(positions) = positions {
        for &position in positions {
            let Some(row) = rows.get(position) else {
                return Err(DbError::Internal(format!(
                    "index position {position} is outside the table"
                )));
            };
            if expr::row_matches(filter, row)? {
                visit(row)?;
            }
        }
        return Ok(());
    }
    for row in rows {
        if expr::row_matches(filter, row)? {
            visit(row)?;
        }
    }
    Ok(())
}

fn fresh_acc(aggs: &[AggFn]) -> Vec<AccValue> {
    aggs.iter()
        .map(|agg| match agg {
            AggFn::CountStar | AggFn::CountColumn(_) | AggFn::CountDistinct(_) => {
                AccValue::Count(0)
            }
            AggFn::Sum(_) | AggFn::SumDistinct(_) => AccValue::Sum(None),
            AggFn::Avg(_) | AggFn::AvgDistinct(_) => AccValue::Avg { sum: 0, count: 0 },
            AggFn::Min(_) | AggFn::Max(_) => AccValue::Extreme(None),
        })
        .collect()
}

fn fresh_seen(aggs: &[AggFn]) -> Vec<Option<DistinctSet>> {
    aggs.iter()
        .map(|agg| agg.is_distinct().then(DistinctSet::new))
        .collect()
}

/// A numeric replacement, or a distinct column held until the whole row succeeds.
enum Staged {
    Replace(AccValue),
    Admit { ordinal: usize, numbers: AccValue },
}

/// One aggregate update whose allocations have already been reserved.
enum Ready {
    Replace {
        slot: usize,
        numbers: AccValue,
    },
    Admit {
        slot: usize,
        value: Value,
        numbers: AccValue,
        fresh: Option<Vec<Value>>,
    },
}

fn update_acc(
    acc: &mut [AccValue],
    seen: &mut [Option<DistinctSet>],
    row: InputRow<'_>,
    aggs: &[AggFn],
) -> Result<(), DbError> {
    if acc.len() != aggs.len() || seen.len() != aggs.len() {
        return Err(DbError::Internal(
            "aggregate accumulator has the wrong width".to_string(),
        ));
    }
    // Checked arithmetic finishes before any slot changes. Distinct clones and
    // bucket reservations finish next. Only then are numbers and membership
    // written, so a later error drops the prepared row.
    let mut pending = Vec::new();
    for (slot, agg) in aggs.iter().enumerate() {
        if let Some(next) = next_acc(&acc[slot], seen[slot].as_ref(), row, agg)? {
            pending.push((slot, next));
        }
    }
    if pending
        .iter()
        .all(|(_, staged)| matches!(staged, Staged::Replace(_)))
    {
        for (slot, staged) in pending {
            let Staged::Replace(numbers) = staged else {
                unreachable!("pending row was filtered to replacements");
            };
            acc[slot] = numbers;
        }
        return Ok(());
    }
    if pending.len() == 1 && matches!(pending[0].1, Staged::Admit { .. }) {
        let Some((slot, staged)) = pending.pop() else {
            unreachable!("the pending row has one update");
        };
        let Staged::Admit { ordinal, numbers } = staged else {
            unreachable!("pending row is one admission");
        };
        let value = cell(row, ordinal)?.clone();
        let fresh = {
            let Some(set) = seen[slot].as_mut() else {
                return Err(DbError::Internal(
                    "DISTINCT aggregate is missing its set".to_string(),
                ));
            };
            set.prepare_admit(&value)?
        };
        let Some(set) = seen[slot].as_mut() else {
            unreachable!("DISTINCT aggregate is missing its set");
        };
        set.commit_admit(value, fresh);
        acc[slot] = numbers;
        return Ok(());
    }
    let mut ready = Vec::with_capacity(pending.len());
    for (slot, staged) in pending {
        match staged {
            Staged::Replace(numbers) => ready.push(Ready::Replace { slot, numbers }),
            Staged::Admit { ordinal, numbers } => {
                let value = cell(row, ordinal)?.clone();
                let fresh = {
                    let Some(set) = seen[slot].as_mut() else {
                        return Err(DbError::Internal(
                            "DISTINCT aggregate is missing its set".to_string(),
                        ));
                    };
                    set.prepare_admit(&value)?
                };
                ready.push(Ready::Admit {
                    slot,
                    value,
                    numbers,
                    fresh,
                });
            }
        }
    }
    for item in ready {
        match item {
            Ready::Replace { slot, numbers } => acc[slot] = numbers,
            Ready::Admit {
                slot,
                value,
                numbers,
                fresh,
            } => {
                let Some(set) = seen[slot].as_mut() else {
                    unreachable!("DISTINCT aggregate is missing its set");
                };
                set.commit_admit(value, fresh);
                acc[slot] = numbers;
            }
        }
    }
    Ok(())
}

fn next_acc(
    current: &AccValue,
    seen: Option<&DistinctSet>,
    row: InputRow<'_>,
    agg: &AggFn,
) -> Result<Option<Staged>, DbError> {
    match agg {
        AggFn::CountStar => Ok(Some(Staged::Replace(bumped_count(current, "COUNT")?))),
        AggFn::CountColumn(ordinal) => {
            if cell(row, *ordinal)?.is_null() {
                Ok(None)
            } else {
                Ok(Some(Staged::Replace(bumped_count(current, "COUNT")?)))
            }
        }
        AggFn::CountDistinct(ordinal) => stage_count_distinct(current, seen, row, *ordinal),
        AggFn::Sum(ordinal) => Ok(next_sum(current, row, *ordinal)?.map(Staged::Replace)),
        AggFn::SumDistinct(ordinal) => stage_sum_distinct(current, seen, row, *ordinal),
        AggFn::Avg(ordinal) => Ok(next_avg(current, row, *ordinal)?.map(Staged::Replace)),
        AggFn::AvgDistinct(ordinal) => stage_avg_distinct(current, seen, row, *ordinal),
        AggFn::Min(ordinal) => Ok(next_extreme(current, row, *ordinal, true)?.map(Staged::Replace)),
        AggFn::Max(ordinal) => {
            Ok(next_extreme(current, row, *ordinal, false)?.map(Staged::Replace))
        }
    }
}

fn stage_count_distinct(
    current: &AccValue,
    seen: Option<&DistinctSet>,
    row: InputRow<'_>,
    ordinal: usize,
) -> Result<Option<Staged>, DbError> {
    if new_distinct(seen, row, ordinal)?.is_none() {
        return Ok(None);
    }
    Ok(Some(Staged::Admit {
        ordinal,
        numbers: bumped_count(current, "COUNT")?,
    }))
}

fn stage_sum_distinct(
    current: &AccValue,
    seen: Option<&DistinctSet>,
    row: InputRow<'_>,
    ordinal: usize,
) -> Result<Option<Staged>, DbError> {
    let Some(value) = new_distinct(seen, row, ordinal)? else {
        return Ok(None);
    };
    let Value::Integer(amount) = value else {
        return Err(DbError::Internal(
            "SUM received a non-integer value".to_string(),
        ));
    };
    let AccValue::Sum(total) = current else {
        return Err(DbError::Internal(
            "SUM accumulator has the wrong shape".to_string(),
        ));
    };
    let numbers = AccValue::Sum(Some(
        total
            .unwrap_or(0)
            .checked_add(*amount)
            .ok_or_else(|| DbError::invalid("SUM overflows INTEGER"))?,
    ));
    Ok(Some(Staged::Admit { ordinal, numbers }))
}

fn stage_avg_distinct(
    current: &AccValue,
    seen: Option<&DistinctSet>,
    row: InputRow<'_>,
    ordinal: usize,
) -> Result<Option<Staged>, DbError> {
    let Some(value) = new_distinct(seen, row, ordinal)? else {
        return Ok(None);
    };
    let Value::Integer(amount) = value else {
        return Err(DbError::Internal(
            "AVG received a non-integer value".to_string(),
        ));
    };
    let AccValue::Avg { sum, count } = current else {
        return Err(DbError::Internal(
            "AVG accumulator has the wrong shape".to_string(),
        ));
    };
    let sum = sum
        .checked_add(i128::from(*amount))
        .ok_or_else(|| DbError::invalid("AVG overflows"))?;
    let count = count
        .checked_add(1)
        .ok_or_else(|| DbError::invalid("AVG overflows INTEGER"))?;
    Ok(Some(Staged::Admit {
        ordinal,
        numbers: AccValue::Avg { sum, count },
    }))
}

/// A NULL input and a value already in the set are not candidates.
fn new_distinct<'a>(
    seen: Option<&DistinctSet>,
    row: InputRow<'a>,
    ordinal: usize,
) -> Result<Option<&'a Value>, DbError> {
    let value = cell(row, ordinal)?;
    if value.is_null() {
        return Ok(None);
    }
    let Some(set) = seen else {
        return Err(DbError::Internal(
            "DISTINCT aggregate is missing its set".to_string(),
        ));
    };
    if set.contains(value) {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn next_sum(
    current: &AccValue,
    row: InputRow<'_>,
    ordinal: usize,
) -> Result<Option<AccValue>, DbError> {
    let value = match cell(row, ordinal)? {
        Value::Null => return Ok(None),
        Value::Integer(value) => *value,
        _ => {
            return Err(DbError::Internal(
                "SUM received a non-integer value".to_string(),
            ));
        }
    };
    let AccValue::Sum(total) = current else {
        return Err(DbError::Internal(
            "SUM accumulator has the wrong shape".to_string(),
        ));
    };
    let base = total.unwrap_or(0);
    Ok(Some(AccValue::Sum(Some(
        base.checked_add(value)
            .ok_or_else(|| DbError::invalid("SUM overflows INTEGER"))?,
    ))))
}

fn next_avg(
    current: &AccValue,
    row: InputRow<'_>,
    ordinal: usize,
) -> Result<Option<AccValue>, DbError> {
    let value = match cell(row, ordinal)? {
        Value::Null => return Ok(None),
        Value::Integer(value) => *value,
        _ => {
            return Err(DbError::Internal(
                "AVG received a non-integer value".to_string(),
            ));
        }
    };
    let AccValue::Avg { sum, count } = current else {
        return Err(DbError::Internal(
            "AVG accumulator has the wrong shape".to_string(),
        ));
    };
    let sum = sum
        .checked_add(i128::from(value))
        .ok_or_else(|| DbError::invalid("AVG overflows"))?;
    let count = count
        .checked_add(1)
        .ok_or_else(|| DbError::invalid("AVG overflows INTEGER"))?;
    Ok(Some(AccValue::Avg { sum, count }))
}

fn next_extreme(
    current: &AccValue,
    row: InputRow<'_>,
    ordinal: usize,
    min: bool,
) -> Result<Option<AccValue>, DbError> {
    let value = cell(row, ordinal)?;
    if value.is_null() {
        return Ok(None);
    }
    let AccValue::Extreme(stored) = current else {
        return Err(DbError::Internal(
            "MIN/MAX accumulator has the wrong shape".to_string(),
        ));
    };
    let replace = match stored {
        None => true,
        Some(stored) => {
            let ordering = value::cmp_scalar(value, stored).ok_or_else(|| {
                DbError::Internal("MIN/MAX values have different types".to_string())
            })?;
            if min {
                ordering.is_lt()
            } else {
                ordering.is_gt()
            }
        }
    };
    if replace {
        Ok(Some(AccValue::Extreme(Some(value.clone()))))
    } else {
        Ok(None)
    }
}

fn bumped_count(slot: &AccValue, name: &str) -> Result<AccValue, DbError> {
    let AccValue::Count(count) = slot else {
        return Err(DbError::Internal(
            "COUNT accumulator has the wrong shape".to_string(),
        ));
    };
    Ok(AccValue::Count(count.checked_add(1).ok_or_else(|| {
        DbError::invalid(format!("{name} overflows INTEGER"))
    })?))
}

#[cfg(test)]
fn update_plain(acc: &mut [AccValue], row: InputRow<'_>, aggs: &[AggFn]) -> Result<(), DbError> {
    let mut seen = Vec::new();
    seen.resize_with(aggs.len(), || None);
    update_acc(acc, &mut seen, row, aggs)
}

fn cell(row: InputRow<'_>, ordinal: usize) -> Result<&Value, DbError> {
    row.get(ordinal)
        .ok_or_else(|| DbError::Internal("aggregate column is outside the row".to_string()))
}

fn materialize_group(key: &[Value], acc: &[AccValue]) -> Vec<Value> {
    let mut row = Vec::with_capacity(key.len() + acc.len());
    row.extend_from_slice(key);
    for value in acc {
        row.push(match value {
            AccValue::Count(count) => Value::Integer(*count),
            AccValue::Sum(total) => total.map(Value::Integer).unwrap_or(Value::Null),
            AccValue::Avg { sum, count } => {
                if *count > 0 {
                    Value::ratio(*sum, *count)
                } else {
                    Value::Null
                }
            }
            AccValue::Extreme(value) => value.clone().unwrap_or(Value::Null),
        });
    }
    row
}

fn fingerprint(value: &Value) -> u64 {
    #[cfg(test)]
    if let Some(forced) = FORCED_HASH.with(std::cell::Cell::get)
        && !value.is_null()
    {
        return forced;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match value {
        Value::Null => 0u8.hash(&mut hasher),
        Value::Integer(item) => {
            1u8.hash(&mut hasher);
            item.hash(&mut hasher);
        }
        Value::Text(item) => {
            2u8.hash(&mut hasher);
            item.hash(&mut hasher);
        }
        Value::Boolean(item) => {
            3u8.hash(&mut hasher);
            item.hash(&mut hasher);
        }
        Value::Ratio {
            numerator,
            denominator,
        } => {
            4u8.hash(&mut hasher);
            numerator.hash(&mut hasher);
            denominator.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn hash_group(row: InputRow<'_>, ordinals: &[usize]) -> Result<u64, DbError> {
    #[cfg(test)]
    if let Some(forced) = FORCED_HASH.with(std::cell::Cell::get) {
        for &ordinal in ordinals {
            let _ = cell(row, ordinal)?;
        }
        return Ok(forced);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for &ordinal in ordinals {
        match cell(row, ordinal)? {
            Value::Null => 0u8.hash(&mut hasher),
            Value::Integer(value) => {
                1u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Text(value) => {
                2u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Boolean(value) => {
                3u8.hash(&mut hasher);
                value.hash(&mut hasher);
            }
            Value::Ratio {
                numerator,
                denominator,
            } => {
                4u8.hash(&mut hasher);
                numerator.hash(&mut hasher);
                denominator.hash(&mut hasher);
            }
        }
    }
    Ok(hasher.finish())
}

fn group_equals(key: &[Value], row: InputRow<'_>, ordinals: &[usize]) -> Result<bool, DbError> {
    if key.len() != ordinals.len() {
        return Ok(false);
    }
    for (stored, &ordinal) in key.iter().zip(ordinals) {
        let value = cell(row, ordinal)?;
        let same = match (stored, value) {
            (Value::Null, Value::Null) => true,
            (Value::Integer(left), Value::Integer(right)) => left == right,
            (Value::Text(left), Value::Text(right)) => left == right,
            (Value::Boolean(left), Value::Boolean(right)) => left == right,
            (
                Value::Ratio {
                    numerator: left_n,
                    denominator: left_d,
                },
                Value::Ratio {
                    numerator: right_n,
                    denominator: right_d,
                },
            ) => left_n == right_n && left_d == right_d,
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

fn finish_aggregate(
    groups: Vec<Vec<Value>>,
    plan: AggPlan,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    let groups = apply_having(groups, plan.having.as_ref())?;
    finish_rows(
        groups,
        &plan.sort_keys,
        limit,
        &plan.outputs,
        plan.output_names,
    )
}

fn apply_having(
    groups: Vec<Vec<Value>>,
    having: Option<&BoundExpr>,
) -> Result<Vec<Vec<Value>>, DbError> {
    let Some(having) = having else {
        return Ok(groups);
    };
    let mut kept = Vec::new();
    for group in groups {
        if expr::row_matches(Some(having), &group)? {
            kept.push(group);
        }
    }
    Ok(kept)
}

fn bind_optional_where(
    selection: Option<&Expr>,
    scope: &Scope,
) -> Result<Option<BoundExpr>, DbError> {
    selection
        .map(|expr| expr::bind_where(expr, scope))
        .transpose()
}

struct OutputPlan {
    indexes: Vec<usize>,
    names: Vec<String>,
    explicit: Vec<bool>,
    types: Vec<expr::ScalarType>,
}

impl OutputPlan {
    fn new() -> Self {
        Self {
            indexes: Vec::new(),
            names: Vec::new(),
            explicit: Vec::new(),
            types: Vec::new(),
        }
    }

    fn push(&mut self, index: usize, name: String, explicit: bool, scalar: expr::ScalarType) {
        self.indexes.push(index);
        self.names.push(name);
        self.explicit.push(explicit);
        self.types.push(scalar);
    }
}

fn push_star(outputs: &mut OutputPlan, scope: &Scope, qualifier: &str) -> Result<(), DbError> {
    for index in scope.columns_of(qualifier)? {
        let Some(name) = scope.name_at(index) else {
            return Err(DbError::Internal(
                "select column is outside the table".to_string(),
            ));
        };
        outputs.push(index, name.to_string(), false, stored_scalar(scope, index)?);
    }
    Ok(())
}

fn stored_scalar(scope: &Scope, index: usize) -> Result<expr::ScalarType, DbError> {
    scope
        .type_at(index)
        .map(expr::ScalarType::from)
        .ok_or_else(|| DbError::Internal("select column is outside the table".to_string()))
}

fn output_columns(scope: &Scope, projection: &Projection) -> Result<OutputPlan, DbError> {
    let mut outputs = OutputPlan::new();
    let mut explicit_aliases = HashSet::new();
    match projection {
        Projection::All => {
            for index in 0..scope.len() {
                let Some(name) = scope.name_at(index) else {
                    return Err(DbError::Internal(
                        "select column is outside the table".to_string(),
                    ));
                };
                outputs.push(index, name.to_string(), false, stored_scalar(scope, index)?);
            }
        }
        Projection::QualifiedAll { qualifier } => {
            push_star(&mut outputs, scope, qualifier)?;
        }
        Projection::QualifiedStars { qualifiers } => {
            for qualifier in qualifiers {
                push_star(&mut outputs, scope, qualifier)?;
            }
        }
        Projection::Columns(columns) => {
            for item in columns {
                match item {
                    SelectItem::Column { column, alias } => {
                        note_explicit_alias(&mut explicit_aliases, alias.as_deref())?;
                        let index = scope.resolve(column)?;
                        let Some(implicit) = scope.name_at(index) else {
                            return Err(DbError::Internal(
                                "select column is outside the table".to_string(),
                            ));
                        };
                        outputs.push(
                            index,
                            alias.clone().unwrap_or_else(|| implicit.to_string()),
                            alias.is_some(),
                            stored_scalar(scope, index)?,
                        );
                    }
                    SelectItem::Aggregate { .. } => {
                        return Err(DbError::Internal(
                            "aggregate select item in a non-aggregate query".to_string(),
                        ));
                    }
                }
            }
        }
    }
    Ok(outputs)
}

fn note_explicit_alias(seen: &mut HashSet<String>, alias: Option<&str>) -> Result<(), DbError> {
    let Some(alias) = alias else {
        return Ok(());
    };
    if !seen.insert(alias.to_string()) {
        return Err(DbError::invalid(format!(
            "duplicate column alias '{alias}'"
        )));
    }
    Ok(())
}

fn project_row(row: &[Value], indexes: &[usize]) -> Result<Vec<Value>, DbError> {
    project_at(indexes, |index| row.get(index))
}

fn project_pair(
    outer: &[Value],
    inner: &[Value],
    indexes: &[usize],
) -> Result<Vec<Value>, DbError> {
    project_at(indexes, |index| {
        if index < outer.len() {
            outer.get(index)
        } else {
            inner.get(index - outer.len())
        }
    })
}

fn project_at<'a>(
    indexes: &[usize],
    mut value_at: impl FnMut(usize) -> Option<&'a Value>,
) -> Result<Vec<Value>, DbError> {
    let mut projected = Vec::with_capacity(indexes.len());
    for index in indexes {
        let Some(value) = value_at(*index) else {
            return Err(DbError::Internal(format!(
                "row is missing column index {index}"
            )));
        };
        projected.push(value.clone());
    }
    Ok(projected)
}

fn bind_assignments(
    scope: &Scope,
    table_name: &str,
    assignments: &[Assignment],
) -> Result<Vec<BoundAssignment>, DbError> {
    if assignments.is_empty() {
        return Err(DbError::invalid("UPDATE requires at least one assignment"));
    }
    let mut bound = Vec::with_capacity(assignments.len());
    let mut seen = vec![false; scope.len()];
    for assignment in assignments {
        let index = scope.resolve(&assignment.column)?;
        if seen[index] {
            return Err(DbError::invalid(format!(
                "column '{}' is assigned more than once in table '{table_name}'",
                assignment.column.name
            )));
        }
        seen[index] = true;
        let Some(name) = scope.name_at(index) else {
            return Err(DbError::Internal(format!(
                "assignment index {index} is outside table '{table_name}'"
            )));
        };
        let Some(data_type) = scope.type_at(index) else {
            return Err(DbError::Internal(format!(
                "assignment index {index} is outside table '{table_name}'"
            )));
        };
        let value = expr::bind_assignment(&assignment.value, scope, name, data_type)?;
        bound.push(BoundAssignment { index, value });
    }
    Ok(bound)
}

/// Evaluate every right-hand side against `row`, then write them.
fn assign_row(row: &[Value], assignments: &[BoundAssignment]) -> Result<Vec<Value>, DbError> {
    let mut values = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        values.push((assignment.index, expr::eval(&assignment.value, row)?));
    }
    let mut updated = row.to_vec();
    for (index, value) in values {
        let Some(slot) = updated.get_mut(index) else {
            return Err(DbError::Internal(format!(
                "assignment index {index} is outside the row"
            )));
        };
        value.ensure_persistent()?;
        *slot = value;
    }
    Ok(updated)
}

fn bind_order(
    scope: &Scope,
    keys: &[OrderKey],
    outputs: &OutputPlan,
) -> Result<Vec<SortKey>, DbError> {
    let mut bound = Vec::with_capacity(keys.len());
    for key in keys {
        let OrderTarget::Column(column) = &key.target else {
            return Err(DbError::unsupported(
                "ORDER BY aggregate requires GROUP BY or an aggregate select list",
            ));
        };
        let index = order_output_index(column, outputs, |column| scope.resolve(column))?;
        bound.push(SortKey {
            index,
            ascending: key.ascending,
            nulls_first: key.nulls_first.unwrap_or(!key.ascending),
        });
    }
    Ok(bound)
}

/// `SELECT DISTINCT` sorts projected rows, so each key is an output position.
///
/// An unqualified name uses a unique explicit alias, then a unique implicit
/// output name, then the one projected source column of that name. A qualified
/// name ignores aliases and must identify exactly one projected source column.
/// A column that is not in the projection is rejected: after deduplication the
/// hidden value is not a single row.
fn bind_distinct_order(
    scope: &Scope,
    keys: &[OrderKey],
    outputs: &OutputPlan,
) -> Result<Vec<SortKey>, DbError> {
    let mut bound = Vec::with_capacity(keys.len());
    for key in keys {
        let OrderTarget::Column(column) = &key.target else {
            return Err(DbError::unsupported(
                "DISTINCT is not supported with aggregate functions",
            ));
        };
        let index = distinct_output_slot(scope, column, outputs)?;
        bound.push(SortKey {
            index,
            ascending: key.ascending,
            nulls_first: key.nulls_first.unwrap_or(!key.ascending),
        });
    }
    Ok(bound)
}

fn distinct_output_slot(
    scope: &Scope,
    column: &SelectColumn,
    outputs: &OutputPlan,
) -> Result<usize, DbError> {
    if let Some(hit) = output_name_index(column, outputs)? {
        return Ok(hit);
    }
    let source = scope.resolve(column)?;
    let hits: Vec<usize> = outputs
        .indexes
        .iter()
        .enumerate()
        .filter(|(_, index)| **index == source)
        .map(|(hit, _)| hit)
        .collect();
    match hits.as_slice() {
        [hit] => Ok(*hit),
        [] => Err(DbError::invalid(format!(
            "column '{}' must appear in the SELECT DISTINCT list",
            column.render()
        ))),
        _ => Err(ambiguous_column(&column.name)),
    }
}

/// Project, deduplicate, sort, then apply `LIMIT`.
///
/// Every candidate is visited. A later scan or reservation error is returned
/// even when `LIMIT` would have discarded that row.
fn finish_distinct(
    table_name: &str,
    stored: &[Vec<Value>],
    positions: Option<&[usize]>,
    prepared: &PreparedSelect,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    let mut distinct = DistinctRows::new();
    let mut admit = |row: &[Value]| -> Result<(), DbError> {
        if expr::row_matches(prepared.filter.as_ref(), row)? {
            distinct.admit(project_row(row, &prepared.output_indexes)?)?;
        }
        Ok(())
    };
    if let Some(positions) = positions {
        for position in positions {
            let Some(row) = stored.get(*position) else {
                return Err(DbError::Internal(format!(
                    "index position {position} is outside table '{table_name}'"
                )));
            };
            admit(row)?;
        }
    } else {
        for row in stored {
            admit(row)?;
        }
    }
    Ok(finish_distinct_rows(
        distinct,
        &prepared.sort_keys,
        limit,
        prepared.output_names.clone(),
    ))
}

fn finish_distinct_rows(
    distinct: DistinctRows,
    sort_keys: &[SortKey],
    limit: Option<u64>,
    columns: Vec<String>,
) -> StatementResult {
    let mut rows = distinct.into_rows();
    if !sort_keys.is_empty() {
        rows.sort_by(|left, right| compare_rows(left, right, sort_keys));
    }
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if rows.len() > limit {
            rows.truncate(limit);
        }
    }
    StatementResult::Query(QueryResult { columns, rows })
}

/// First-seen projected rows of one `SELECT DISTINCT`.
///
/// The map key is a fingerprint of the whole output tuple. Each bucket stores
/// positions into `rows`. Equality compares every cell, and NULL matches NULL,
/// so a hash collision cannot merge unequal tuples. A row is stored once, when
/// it is first admitted. Text is the clone made by projection; a duplicate
/// drops that clone and reserves nothing. Memory is one stored tuple per
/// unique output row, plus one hash and one bucket slot. There is no disk spill.
struct DistinctRows {
    buckets: HashMap<u64, Vec<usize>>,
    rows: Vec<Vec<Value>>,
}

impl DistinctRows {
    fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            rows: Vec::new(),
        }
    }

    fn admit(&mut self, row: Vec<Value>) -> Result<(), DbError> {
        let hash = fingerprint_row(&row);
        let seen = self.buckets.get(&hash).is_some_and(|bucket| {
            bucket
                .iter()
                .any(|&index| same_projected(&self.rows[index], &row))
        });
        if seen {
            return Ok(());
        }
        if self.buckets.contains_key(&hash) {
            self.admit_collision(hash, row)
        } else {
            self.admit_fresh(hash, row)
        }
    }

    fn admit_fresh(&mut self, hash: u64, row: Vec<Value>) -> Result<(), DbError> {
        charge_reserve(distinct_row_capacity())?;
        self.buckets
            .try_reserve(1)
            .map_err(|_| distinct_row_capacity())?;
        let mut bucket = Vec::new();
        charge_reserve(distinct_row_capacity())?;
        bucket.try_reserve(1).map_err(|_| distinct_row_capacity())?;
        charge_reserve(distinct_row_capacity())?;
        self.rows
            .try_reserve(1)
            .map_err(|_| distinct_row_capacity())?;
        debug_assert!(!self.buckets.contains_key(&hash));
        debug_assert!(bucket.capacity() > bucket.len());
        debug_assert!(self.buckets.capacity() > self.buckets.len());
        debug_assert!(self.rows.capacity() > self.rows.len());
        let bucket_capacity = bucket.capacity();
        let map_capacity = self.buckets.capacity();
        let row_capacity = self.rows.capacity();
        bucket.push(self.rows.len());
        debug_assert_eq!(bucket.capacity(), bucket_capacity);
        self.rows.push(row);
        debug_assert_eq!(self.rows.capacity(), row_capacity);
        self.buckets.insert(hash, bucket);
        debug_assert_eq!(self.buckets.capacity(), map_capacity);
        Ok(())
    }

    fn admit_collision(&mut self, hash: u64, row: Vec<Value>) -> Result<(), DbError> {
        charge_reserve(distinct_row_capacity())?;
        {
            let Some(bucket) = self.buckets.get_mut(&hash) else {
                return Err(DbError::Internal(
                    "SELECT DISTINCT bucket disappeared".to_string(),
                ));
            };
            bucket.try_reserve(1).map_err(|_| distinct_row_capacity())?;
        }
        charge_reserve(distinct_row_capacity())?;
        self.rows
            .try_reserve(1)
            .map_err(|_| distinct_row_capacity())?;
        let index = self.rows.len();
        {
            let Some(bucket) = self.buckets.get_mut(&hash) else {
                return Err(DbError::Internal(
                    "SELECT DISTINCT bucket disappeared".to_string(),
                ));
            };
            debug_assert!(bucket.capacity() > bucket.len());
            let bucket_capacity = bucket.capacity();
            bucket.push(index);
            debug_assert_eq!(bucket.capacity(), bucket_capacity);
        }
        debug_assert!(self.rows.capacity() > self.rows.len());
        let row_capacity = self.rows.capacity();
        self.rows.push(row);
        debug_assert_eq!(self.rows.capacity(), row_capacity);
        Ok(())
    }

    fn into_rows(self) -> Vec<Vec<Value>> {
        self.rows
    }
}

fn distinct_row_capacity() -> DbError {
    DbError::Internal("SELECT DISTINCT result is too large".to_string())
}

fn fingerprint_row(row: &[Value]) -> u64 {
    #[cfg(test)]
    if let Some(forced) = FORCED_HASH.with(std::cell::Cell::get) {
        return forced;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for value in row {
        match value {
            Value::Null => 0u8.hash(&mut hasher),
            Value::Integer(item) => {
                1u8.hash(&mut hasher);
                item.hash(&mut hasher);
            }
            Value::Text(item) => {
                2u8.hash(&mut hasher);
                item.hash(&mut hasher);
            }
            Value::Boolean(item) => {
                3u8.hash(&mut hasher);
                item.hash(&mut hasher);
            }
            Value::Ratio {
                numerator,
                denominator,
            } => {
                4u8.hash(&mut hasher);
                numerator.hash(&mut hasher);
                denominator.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn same_projected(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| distinct_equal(left, right))
}

/// NULL matches NULL. Unequal types do not match. Text keeps its case.
fn distinct_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (
            Value::Ratio {
                numerator: left_n,
                denominator: left_d,
            },
            Value::Ratio {
                numerator: right_n,
                denominator: right_d,
            },
        ) => left_n == right_n && left_d == right_d,
        _ => false,
    }
}

fn projection_label(projection: &Projection) -> String {
    match projection {
        Projection::All => "*".to_string(),
        Projection::QualifiedAll { qualifier } => format!("{qualifier}.*"),
        Projection::QualifiedStars { qualifiers } => qualifiers
            .iter()
            .map(|qualifier| format!("{qualifier}.*"))
            .collect::<Vec<_>>()
            .join(", "),
        Projection::Columns(items) => items
            .iter()
            .map(select_item_label)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn select_item_label(item: &SelectItem) -> String {
    match item {
        SelectItem::Column {
            column,
            alias: None,
        } => column.render(),
        SelectItem::Column {
            column,
            alias: Some(alias),
        } => format!("{} AS {alias}", column.render()),
        SelectItem::Aggregate { call, alias: None } => call.render(),
        SelectItem::Aggregate {
            call,
            alias: Some(alias),
        } => format!("{} AS {alias}", call.render()),
    }
}

fn render_order_key(key: &OrderKey) -> String {
    let target = match &key.target {
        OrderTarget::Column(column) => column.render(),
        OrderTarget::Aggregate(call) => call.render(),
    };
    let direction = if key.ascending { "ASC" } else { "DESC" };
    match key.nulls_first {
        None => format!("{target} {direction}"),
        Some(true) => format!("{target} {direction} NULLS FIRST"),
        Some(false) => format!("{target} {direction} NULLS LAST"),
    }
}

fn limit_cell(limit: Option<u64>) -> Value {
    match limit {
        None => Value::Null,
        Some(limit) => match i64::try_from(limit) {
            Ok(limit) => Value::Integer(limit),
            Err(_) => Value::Text(limit.to_string()),
        },
    }
}

/// Unqualified names prefer one explicit alias, then one implicit output name.
///
/// A qualified name never consults aliases. Two implicit names with the same
/// text are ambiguous. A unique explicit alias wins over a source column and
/// over another output that only has that text as its implicit name.
fn order_output_index(
    column: &SelectColumn,
    outputs: &OutputPlan,
    fallback: impl FnOnce(&SelectColumn) -> Result<usize, DbError>,
) -> Result<usize, DbError> {
    if let Some(hit) = output_name_index(column, outputs)? {
        return Ok(outputs.indexes[hit]);
    }
    fallback(column)
}

/// Unqualified output lookup shared by `ORDER BY` and `HAVING`.
///
/// A unique explicit alias wins, including over a source column of the same
/// name. A unique implicit output name is next. A qualified name returns
/// `None` so the caller resolves it in the source scope. There is no second
/// substitution: the hit is that select item's own slot.
fn output_name_index(
    column: &SelectColumn,
    outputs: &OutputPlan,
) -> Result<Option<usize>, DbError> {
    if column.qualifier.is_some() {
        return Ok(None);
    }
    let explicit = output_hits(outputs, &column.name, true);
    if explicit.len() > 1 {
        return Err(ambiguous_column(&column.name));
    }
    if let [hit] = explicit.as_slice() {
        return Ok(Some(*hit));
    }
    let implicit = output_hits(outputs, &column.name, false);
    if implicit.len() > 1 {
        return Err(ambiguous_column(&column.name));
    }
    if let [hit] = implicit.as_slice() {
        return Ok(Some(*hit));
    }
    Ok(None)
}

fn having_output(
    column: &SelectColumn,
    outputs: &OutputPlan,
) -> Result<Option<(usize, expr::ScalarType)>, DbError> {
    let Some(hit) = output_name_index(column, outputs)? else {
        return Ok(None);
    };
    Ok(Some((outputs.indexes[hit], outputs.types[hit])))
}

fn output_hits(outputs: &OutputPlan, name: &str, explicit: bool) -> Vec<usize> {
    outputs
        .names
        .iter()
        .enumerate()
        .filter(|(index, output)| output == &name && outputs.explicit[*index] == explicit)
        .map(|(index, _)| index)
        .collect()
}

fn ambiguous_column(name: &str) -> DbError {
    DbError::invalid(format!("column '{name}' is ambiguous"))
}

/// One equality key inside a hash bucket.
///
/// `head` is the first build row that owns the key. Later rows with the same
/// SQL value append to `positions`. A second key that lands on the same `u64`
/// is a different group, so the probe never treats it as a match.
struct KeyGroup {
    head: usize,
    positions: Vec<usize>,
}

/// Keep an index probe when the inner table is more than this many times the
/// outer table. At 20× the hash scan was faster. At 100× and 400× the index
/// probe was faster. 64 sits between those measurements.
const INDEX_PROBE_INNER_FACTOR: usize = 64;

/// Replace an indexed equality with a hash join unless the inner table is the
/// one the measurements say is cheaper to probe.
fn finish_join_path(path: JoinAccess, outer_len: usize, inner_len: usize) -> JoinAccess {
    let JoinAccess::IndexNestedLoop {
        name,
        inner_ordinal,
        outer_ordinal,
        predicate,
        residual,
    } = path
    else {
        return path;
    };
    if prefer_index_probe(outer_len, inner_len) {
        return JoinAccess::IndexNestedLoop {
            name,
            inner_ordinal,
            outer_ordinal,
            predicate,
            residual,
        };
    }
    JoinAccess::HashJoin {
        outer_ordinal,
        inner_ordinal,
        predicate,
        residual,
    }
}

fn prefer_index_probe(outer_len: usize, inner_len: usize) -> bool {
    #[cfg(test)]
    if FORCE_INDEX_JOIN.with(std::cell::Cell::get) {
        return true;
    }
    if outer_len == 0 {
        return true;
    }
    inner_len > outer_len.saturating_mul(INDEX_PROBE_INNER_FACTOR)
}

/// Keeps an eligible index probe for the life of the guard.
#[cfg(test)]
pub(crate) struct ForceIndexJoin;

#[cfg(test)]
impl ForceIndexJoin {
    pub(crate) fn new() -> Self {
        FORCE_INDEX_JOIN.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceIndexJoin {
    fn drop(&mut self) {
        FORCE_INDEX_JOIN.with(|flag| flag.set(false));
    }
}

/// Forces the nested-loop join for the life of the guard.
#[cfg(test)]
pub(crate) struct ForceNestedJoin;

#[cfg(test)]
impl ForceNestedJoin {
    pub(crate) fn new() -> Self {
        FORCE_NESTED_JOIN.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceNestedJoin {
    fn drop(&mut self) {
        FORCE_NESTED_JOIN.with(|flag| flag.set(false));
    }
}

/// Skips the `ON` cell-type check so a profile can time that pass.
#[cfg(test)]
struct SkipOnSchemaCheck;

#[cfg(test)]
impl SkipOnSchemaCheck {
    fn new() -> Self {
        SKIP_ON_SCHEMA_CHECK.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for SkipOnSchemaCheck {
    fn drop(&mut self) {
        SKIP_ON_SCHEMA_CHECK.with(|flag| flag.set(false));
    }
}

/// Bytes occupied by hash-join positions, key groups, and bucket headers.
///
/// This counts one `usize` per stored position, one group header per distinct
/// key, and one map entry per distinct key. Colliding keys share a map entry,
/// so this is an upper bound on the map and a lower bound on the payload. It
/// does not count allocator slack, the copied build rows, or the probed table,
/// and it is not process RSS.
#[cfg(test)]
fn hash_table_bytes(groups: usize, positions: usize) -> usize {
    let position = std::mem::size_of::<usize>();
    let group = std::mem::size_of::<usize>() + std::mem::size_of::<Vec<usize>>();
    let entry = std::mem::size_of::<u64>() + std::mem::size_of::<Vec<KeyGroup>>();
    positions
        .saturating_mul(position)
        .saturating_add(groups.saturating_mul(group))
        .saturating_add(groups.saturating_mul(entry))
}

fn run_hash_join(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    keys: HashKeys,
    build_inner: bool,
    prepared: PreparedJoin,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    let limit_n = limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    if prepared.sort_keys.is_empty() {
        let mut projected = Vec::new();
        let outputs = prepared.output_indexes.clone();
        visit_hash_pairs(
            outer_rows,
            inner_rows,
            keys,
            build_inner,
            &prepared,
            |outer_row, inner_row| {
                if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                    return Ok(());
                }
                projected.push(project_pair(outer_row, inner_row, &outputs)?);
                Ok(())
            },
        )?;
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }
    let mut joined = Vec::new();
    visit_hash_pairs(
        outer_rows,
        inner_rows,
        keys,
        build_inner,
        &prepared,
        |outer_row, inner_row| {
            joined.push(concat_pair(outer_row, inner_row));
            Ok(())
        },
    )?;
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
}

fn visit_hash_pairs(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    keys: HashKeys,
    build_inner: bool,
    prepared: &PreparedJoin,
    mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    if build_inner {
        let table = build_hash(inner_rows, keys.inner_ordinal, hash_key)?;
        for outer_row in outer_rows {
            visit_hash_hits(
                outer_row,
                inner_rows,
                keys.outer_ordinal,
                &table,
                keys.inner_ordinal,
                prepared,
                |inner_row| visit(outer_row, inner_row),
            )?;
        }
        return Ok(());
    }
    // The probe walks the inner table, so matches arrive inner-major. Keep one
    // pair of positions per kept match, not the rows, and emit them
    // outer-then-inner. The buffer grows with the match count.
    let table = build_hash(outer_rows, keys.outer_ordinal, hash_key)?;
    let mut pairs = Vec::new();
    for (inner_pos, inner_row) in inner_rows.iter().enumerate() {
        let Some(hits) = matching_positions(
            &table,
            outer_rows,
            keys.outer_ordinal,
            inner_row,
            keys.inner_ordinal,
            hash_key,
        )?
        else {
            continue;
        };
        for &outer_pos in hits {
            let Some(outer_row) = outer_rows.get(outer_pos) else {
                return Err(DbError::Internal(
                    "hash join position is outside the build side".to_string(),
                ));
            };
            if !pair_kept(prepared, outer_row, inner_row)? {
                continue;
            }
            pairs.push((outer_pos, inner_pos));
        }
    }
    pairs.sort_unstable();
    for (outer_pos, inner_pos) in pairs {
        visit(&outer_rows[outer_pos], &inner_rows[inner_pos])?;
    }
    Ok(())
}

fn visit_hash_hits(
    outer_row: &[Value],
    inner_rows: &[Vec<Value>],
    outer_ordinal: usize,
    table: &HashMap<u64, Vec<KeyGroup>>,
    inner_ordinal: usize,
    prepared: &PreparedJoin,
    mut visit: impl FnMut(&[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    let Some(hits) = matching_positions(
        table,
        inner_rows,
        inner_ordinal,
        outer_row,
        outer_ordinal,
        hash_key,
    )?
    else {
        return Ok(());
    };
    for &position in hits {
        let Some(inner_row) = inner_rows.get(position) else {
            return Err(DbError::Internal(
                "hash join position is outside the build side".to_string(),
            ));
        };
        if !pair_kept(prepared, outer_row, inner_row)? {
            continue;
        }
        visit(inner_row)?;
    }
    Ok(())
}

fn pair_kept(
    prepared: &PreparedJoin,
    outer_row: &[Value],
    inner_row: &[Value],
) -> Result<bool, DbError> {
    if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
        return Ok(false);
    }
    expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)
}

fn concat_pair(outer_row: &[Value], inner_row: &[Value]) -> Vec<Value> {
    let mut row = Vec::with_capacity(outer_row.len().saturating_add(inner_row.len()));
    row.extend_from_slice(outer_row);
    row.extend_from_slice(inner_row);
    row
}

fn build_hash(
    rows: &[Vec<Value>],
    ordinal: usize,
    hash_of: fn(KeyRef<'_>) -> u64,
) -> Result<HashMap<u64, Vec<KeyGroup>>, DbError> {
    let mut table: HashMap<u64, Vec<KeyGroup>> = HashMap::with_capacity(rows.len());
    for position in 0..rows.len() {
        let Some(key) = row_key(rows, position, ordinal)? else {
            continue;
        };
        let hash = hash_of(key);
        if let Some(groups) = table.get_mut(&hash) {
            if let Some(group) = groups
                .iter_mut()
                .find(|group| same_key(rows, ordinal, position, group.head))
            {
                group.positions.push(position);
                continue;
            }
            groups.push(KeyGroup {
                head: position,
                positions: vec![position],
            });
            continue;
        }
        table.insert(
            hash,
            vec![KeyGroup {
                head: position,
                positions: vec![position],
            }],
        );
    }
    Ok(table)
}

/// Positions whose stored key equals the probe value.
///
/// `None` means the probe value is NULL or absent. An empty slice means the
/// hash missed or every group in that bucket was a different key.
fn matching_positions<'a>(
    table: &'a HashMap<u64, Vec<KeyGroup>>,
    build_rows: &[Vec<Value>],
    build_ordinal: usize,
    probe_row: &[Value],
    probe_ordinal: usize,
    hash_of: fn(KeyRef<'_>) -> u64,
) -> Result<Option<&'a [usize]>, DbError> {
    let Some(probe) = probe_row.get(probe_ordinal) else {
        return Err(DbError::Internal(
            "hash join key is outside the row".to_string(),
        ));
    };
    let Some(key) = key_ref(probe) else {
        return Ok(None);
    };
    let Some(groups) = table.get(&hash_of(key)) else {
        return Ok(Some(&[]));
    };
    for group in groups {
        let Some(stored) = build_rows
            .get(group.head)
            .and_then(|row| row.get(build_ordinal))
        else {
            return Err(DbError::Internal(
                "hash join position is outside the build side".to_string(),
            ));
        };
        if same_cell(stored, probe) {
            return Ok(Some(&group.positions));
        }
    }
    Ok(Some(&[]))
}

fn row_key(
    rows: &[Vec<Value>],
    position: usize,
    ordinal: usize,
) -> Result<Option<KeyRef<'_>>, DbError> {
    let Some(value) = rows.get(position).and_then(|row| row.get(ordinal)) else {
        return Err(DbError::Internal(
            "hash join key is outside the row".to_string(),
        ));
    };
    Ok(key_ref(value))
}

fn same_key(rows: &[Vec<Value>], ordinal: usize, left: usize, right: usize) -> bool {
    let Some(left) = rows.get(left).and_then(|row| row.get(ordinal)) else {
        return false;
    };
    let Some(right) = rows.get(right).and_then(|row| row.get(ordinal)) else {
        return false;
    };
    same_cell(left, right)
}

fn same_cell(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) => left == right,
        (Value::Text(left), Value::Text(right)) => left == right,
        (Value::Boolean(left), Value::Boolean(right)) => left == right,
        (
            Value::Ratio {
                numerator: left_n,
                denominator: left_d,
            },
            Value::Ratio {
                numerator: right_n,
                denominator: right_d,
            },
        ) => left_n == right_n && left_d == right_d,
        _ => false,
    }
}

/// A borrowed join key. NULL is not a key.
enum KeyRef<'a> {
    Integer(i64),
    Text(&'a str),
    Boolean(bool),
}

fn key_ref(value: &Value) -> Option<KeyRef<'_>> {
    match value {
        Value::Null => None,
        Value::Integer(key) => Some(KeyRef::Integer(*key)),
        Value::Text(key) => Some(KeyRef::Text(key)),
        Value::Boolean(key) => Some(KeyRef::Boolean(*key)),
        Value::Ratio { .. } => None,
    }
}

/// Hash a scalar key. Equal SQL values hash equal, and the type tag keeps an
/// integer from sharing a fingerprint with a boolean or a text value.
///
/// `DefaultHasher` is deterministic for one process. The hash does not include
/// a row id, page id, or any other persistent identifier.
fn hash_key(key: KeyRef<'_>) -> u64 {
    #[cfg(test)]
    if let Some(forced) = FORCED_HASH.with(std::cell::Cell::get) {
        return forced;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match key {
        KeyRef::Integer(key) => {
            1u8.hash(&mut hasher);
            key.hash(&mut hasher);
        }
        KeyRef::Text(key) => {
            2u8.hash(&mut hasher);
            key.hash(&mut hasher);
        }
        KeyRef::Boolean(key) => {
            3u8.hash(&mut hasher);
            key.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Feed every surviving left-join row to `visit`.
///
/// A stored cell that is not NULL and not its declared type uses the nested
/// loop, matching a plain `HashLeftJoin`. The callback sees `ON`-true pairs
/// and one null-extended row per unmatched left row that passes `WHERE`.
fn consume_left_join(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    outer_types: &[DataType],
    inner_types: &[DataType],
    access: &plan::LeftAccess,
    prepared: &PreparedJoin,
    visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    let nulls = vec![Value::Null; inner_types.len()];
    match access {
        plan::LeftAccess::NestedLoop => {
            visit_left_pairs(outer_rows, inner_rows, &nulls, prepared, visit)
        }
        plan::LeftAccess::Hash {
            outer_ordinal,
            inner_ordinal,
            ..
        } => {
            if !on_cells_match_schema(
                outer_types,
                inner_types,
                outer_rows,
                inner_rows,
                &prepared.on,
            ) {
                return visit_left_pairs(outer_rows, inner_rows, &nulls, prepared, visit);
            }
            visit_hash_left_pairs(
                outer_rows,
                inner_rows,
                &nulls,
                HashKeys {
                    outer_ordinal: *outer_ordinal,
                    inner_ordinal: *inner_ordinal,
                },
                prepared,
                visit,
            )
        }
    }
}

fn scan_hash_left_join(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    outer_types: &[DataType],
    inner_types: &[DataType],
    keys: HashKeys,
    prepared: PreparedJoin,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    // A non-NULL cell whose runtime type disagrees with the column makes the
    // original `ON` raise "different types" for every pair. Exact-key pruning
    // would skip that pair. The nested loop evaluates it.
    if !on_cells_match_schema(
        outer_types,
        inner_types,
        outer_rows,
        inner_rows,
        &prepared.on,
    ) {
        return scan_left_join(outer_rows, inner_rows, inner_types.len(), prepared, limit);
    }
    let nulls = vec![Value::Null; inner_types.len()];
    let limit_n = limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    if prepared.sort_keys.is_empty() {
        let mut projected = Vec::new();
        let outputs = prepared.output_indexes.clone();
        visit_hash_left_pairs(
            outer_rows,
            inner_rows,
            &nulls,
            keys,
            &prepared,
            |outer_row, inner_row| {
                if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                    return Ok(());
                }
                projected.push(project_pair(outer_row, inner_row, &outputs)?);
                Ok(())
            },
        )?;
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }
    let mut joined = Vec::new();
    visit_hash_left_pairs(
        outer_rows,
        inner_rows,
        &nulls,
        keys,
        &prepared,
        |outer_row, inner_row| {
            joined.push(concat_pair(outer_row, inner_row));
            Ok(())
        },
    )?;
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
}

fn declared_types(table: &Table) -> Vec<DataType> {
    table
        .schema()
        .columns()
        .iter()
        .map(crate::schema::Column::data_type)
        .collect()
}

/// `true` when the hash or index path can prune without hiding a type error.
///
/// The check walks each input once for row width, then once per `ON` column.
/// It allocates the column-index list and does not copy rows or pairs. A test
/// profile can skip it to measure that pass; production always runs it.
fn on_cells_allow_fast_join(
    outer_types: &[DataType],
    inner_types: &[DataType],
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    on: &BoundExpr,
) -> bool {
    #[cfg(test)]
    if SKIP_ON_SCHEMA_CHECK.with(std::cell::Cell::get) {
        return true;
    }
    on_cells_match_schema(outer_types, inner_types, outer_rows, inner_rows, on)
}

/// True when every `ON` column agrees with its declared type.
///
/// `NULL` is allowed. A missing cell, a ratio, or any other runtime type is
/// not: the nested loop reports that when it evaluates the comparison.
fn on_cells_match_schema(
    outer_types: &[DataType],
    inner_types: &[DataType],
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    on: &BoundExpr,
) -> bool {
    if outer_rows.iter().any(|row| row.len() != outer_types.len())
        || inner_rows.iter().any(|row| row.len() != inner_types.len())
    {
        return false;
    }
    let mut columns = Vec::new();
    bound_columns(on, &mut columns);
    let outer_width = outer_types.len();
    columns.into_iter().all(|index| {
        let (rows, expected) = if index < outer_width {
            (outer_rows, outer_types.get(index).copied())
        } else {
            (inner_rows, inner_types.get(index - outer_width).copied())
        };
        let Some(expected) = expected else {
            return false;
        };
        let local = index - usize::from(index >= outer_width) * outer_width;
        rows.iter()
            .all(|row| cell_matches_type(row.get(local), expected))
    })
}

fn bound_columns(expr: &BoundExpr, out: &mut Vec<usize>) {
    match expr {
        BoundExpr::Literal(_) => {}
        BoundExpr::Column(index) => {
            if !out.contains(index) {
                out.push(*index);
            }
        }
        BoundExpr::Not(inner) | BoundExpr::IsNull(inner) | BoundExpr::IsNotNull(inner) => {
            bound_columns(inner, out);
        }
        BoundExpr::Binary { left, right, .. } => {
            bound_columns(left, out);
            bound_columns(right, out);
        }
    }
}

fn cell_matches_type(cell: Option<&Value>, expected: DataType) -> bool {
    match cell {
        Some(Value::Null) => true,
        Some(value) => value.data_type() == Some(expected),
        None => false,
    }
}

/// Hash the right table, then walk left rows in insertion order.
///
/// A bucket group stores positions, and those positions stay in right
/// insertion order. Exact key equality selects the group. The original `ON`
/// still decides whether the left row matched, including a residual predicate.
/// `WHERE` runs only after that decision. NULL keys are not hashed and do not
/// probe. There is no pair-position buffer: the right table is always built,
/// even when it is the larger input. Text keys are borrowed from the right
/// rows for the lookup and are not copied into the table.
fn visit_hash_left_pairs(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    nulls: &[Value],
    keys: HashKeys,
    prepared: &PreparedJoin,
    mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    let table = build_hash(inner_rows, keys.inner_ordinal, hash_key)?;
    for outer_row in outer_rows {
        let mut matched = false;
        if let Some(hits) = matching_positions(
            &table,
            inner_rows,
            keys.inner_ordinal,
            outer_row,
            keys.outer_ordinal,
            hash_key,
        )? {
            for &position in hits {
                let Some(inner_row) = inner_rows.get(position) else {
                    return Err(DbError::Internal(
                        "hash left join position is outside the build side".to_string(),
                    ));
                };
                if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
                    continue;
                }
                matched = true;
                if expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)? {
                    visit(outer_row, inner_row)?;
                }
            }
        }
        if !matched && expr::pair_matches(prepared.filter.as_ref(), outer_row, nulls)? {
            visit(outer_row, nulls)?;
        }
    }
    Ok(())
}

fn scan_left_join(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    inner_width: usize,
    prepared: PreparedJoin,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    let nulls = vec![Value::Null; inner_width];
    let limit_n = limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    if prepared.sort_keys.is_empty() {
        let mut projected = Vec::new();
        let outputs = prepared.output_indexes.clone();
        visit_left_pairs(
            outer_rows,
            inner_rows,
            &nulls,
            &prepared,
            |outer_row, inner_row| {
                if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                    return Ok(());
                }
                projected.push(project_pair(outer_row, inner_row, &outputs)?);
                Ok(())
            },
        )?;
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }
    let mut joined = Vec::new();
    visit_left_pairs(
        outer_rows,
        inner_rows,
        &nulls,
        &prepared,
        |outer_row, inner_row| {
            joined.push(concat_pair(outer_row, inner_row));
            Ok(())
        },
    )?;
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
}

/// Emit every TRUE `ON` pair. A left row that never matches is emitted once
/// against `nulls`. `WHERE` runs only on those constructed rows, so filtering
/// a match does not invent a null-extended row. Evaluation continues after
/// `LIMIT` so a later error is not hidden.
fn visit_left_pairs(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    nulls: &[Value],
    prepared: &PreparedJoin,
    mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    for outer_row in outer_rows {
        let mut matched = false;
        for inner_row in inner_rows {
            if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
                continue;
            }
            matched = true;
            if expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)? {
                visit(outer_row, inner_row)?;
            }
        }
        if !matched && expr::pair_matches(prepared.filter.as_ref(), outer_row, nulls)? {
            visit(outer_row, nulls)?;
        }
    }
    Ok(())
}

fn scan_join(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    prepared: PreparedJoin,
    limit: Option<u64>,
) -> Result<StatementResult, DbError> {
    let limit_n = limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    if prepared.sort_keys.is_empty() {
        let mut projected = Vec::new();
        let outputs = prepared.output_indexes.clone();
        visit_nested_pairs(outer_rows, inner_rows, &prepared, |outer_row, inner_row| {
            if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                return Ok(());
            }
            projected.push(project_pair(outer_row, inner_row, &outputs)?);
            Ok(())
        })?;
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }
    let mut joined = Vec::new();
    visit_nested_pairs(outer_rows, inner_rows, &prepared, |outer_row, inner_row| {
        joined.push(concat_pair(outer_row, inner_row));
        Ok(())
    })?;
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
}

fn visit_nested_pairs(
    outer_rows: &[Vec<Value>],
    inner_rows: &[Vec<Value>],
    prepared: &PreparedJoin,
    mut visit: impl FnMut(&[Value], &[Value]) -> Result<(), DbError>,
) -> Result<(), DbError> {
    for outer_row in outer_rows {
        for inner_row in inner_rows {
            if !pair_kept(prepared, outer_row, inner_row)? {
                continue;
            }
            visit(outer_row, inner_row)?;
        }
    }
    Ok(())
}

enum FeedKind {
    Index {
        name: String,
        inner_ordinal: usize,
        outer_ordinal: usize,
    },
    Hash {
        outer_ordinal: usize,
        inner_ordinal: usize,
    },
    Nested,
}

fn feed_kind(path: &JoinAccess) -> FeedKind {
    match path {
        JoinAccess::IndexNestedLoop {
            name,
            inner_ordinal,
            outer_ordinal,
            ..
        } => FeedKind::Index {
            name: name.clone(),
            inner_ordinal: *inner_ordinal,
            outer_ordinal: *outer_ordinal,
        },
        JoinAccess::HashJoin {
            outer_ordinal,
            inner_ordinal,
            ..
        } => FeedKind::Hash {
            outer_ordinal: *outer_ordinal,
            inner_ordinal: *inner_ordinal,
        },
        JoinAccess::NestedLoop => FeedKind::Nested,
    }
}

fn finish_rows(
    mut rows: Vec<Vec<Value>>,
    sort_keys: &[SortKey],
    limit: Option<u64>,
    output_indexes: &[usize],
    output_names: Vec<String>,
) -> Result<StatementResult, DbError> {
    if !sort_keys.is_empty() {
        rows.sort_by(|left, right| compare_rows(left, right, sort_keys));
    }
    if let Some(limit) = limit {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if rows.len() > limit {
            rows.truncate(limit);
        }
    }
    let mut projected = Vec::with_capacity(rows.len());
    for row in &rows {
        projected.push(project_row(row, output_indexes)?);
    }
    Ok(StatementResult::Query(QueryResult {
        columns: output_names,
        rows: projected,
    }))
}

fn compare_rows(left: &[Value], right: &[Value], keys: &[SortKey]) -> Ordering {
    for key in keys {
        let ordering = compare_key(
            left.get(key.index),
            right.get(key.index),
            key.ascending,
            key.nulls_first,
        );
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn compare_key(
    left: Option<&Value>,
    right: Option<&Value>,
    ascending: bool,
    nulls_first: bool,
) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => compare_values(left, right, ascending, nulls_first),
        _ => Ordering::Equal,
    }
}

fn compare_values(left: &Value, right: &Value, ascending: bool, nulls_first: bool) -> Ordering {
    match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let ordering = value::cmp_scalar(left, right).unwrap_or(Ordering::Equal);
            if ascending {
                ordering
            } else {
                ordering.reverse()
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static FORCE_NESTED_JOIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When set, every non-NULL key hashes to this value.
    static FORCED_HASH: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    static FORCE_INDEX_JOIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// When set, the hash and index paths do not precheck `ON` cell types.
    static SKIP_ON_SCHEMA_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MATERIALIZE_AGGREGATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// `Some(0)` fails the next distinct reservation. `None` does not.
    static DISTINCT_RESERVE_BUDGET: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn force_nested_join() -> bool {
    FORCE_NESTED_JOIN.with(std::cell::Cell::get)
}

#[cfg(test)]
fn materialize_aggregate() -> bool {
    MATERIALIZE_AGGREGATE.with(std::cell::Cell::get)
}

/// Stores every joined pair before grouping, for the life of the guard.
#[cfg(test)]
struct MaterializeAggregate;

#[cfg(test)]
impl MaterializeAggregate {
    fn new() -> Self {
        MATERIALIZE_AGGREGATE.with(|flag| flag.set(true));
        Self
    }
}

#[cfg(test)]
impl Drop for MaterializeAggregate {
    fn drop(&mut self) {
        MATERIALIZE_AGGREGATE.with(|flag| flag.set(false));
    }
}

/// Puts every key in one hash bucket for the life of the guard.
#[cfg(test)]
struct ForceHashCollision;

#[cfg(test)]
impl ForceHashCollision {
    fn new() -> Self {
        FORCED_HASH.with(|flag| flag.set(Some(0)));
        Self
    }
}

#[cfg(test)]
impl Drop for ForceHashCollision {
    fn drop(&mut self) {
        FORCED_HASH.with(|flag| flag.set(None));
    }
}

/// Fails distinct-set reservation after `calls` successful charges.
#[cfg(test)]
struct DistinctReserveBudget {
    previous: Option<usize>,
}

#[cfg(test)]
impl DistinctReserveBudget {
    fn limit(calls: usize) -> Self {
        let previous = DISTINCT_RESERVE_BUDGET.with(std::cell::Cell::get);
        DISTINCT_RESERVE_BUDGET.with(|budget| budget.set(Some(calls)));
        Self { previous }
    }
}

#[cfg(test)]
impl Drop for DistinctReserveBudget {
    fn drop(&mut self) {
        DISTINCT_RESERVE_BUDGET.with(|budget| budget.set(self.previous));
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

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

    #[test]
    fn hash_join_matches_the_engine_nested_loop() {
        let mut db = Database::new();
        db.execute("CREATE TABLE lhs (id INT, name TEXT);").unwrap();
        db.execute("CREATE TABLE rhs (id INT, name TEXT);").unwrap();
        db.execute("INSERT INTO lhs VALUES (1, 'Ada'), (NULL, 'Cy'), (1, 'Dee');")
            .unwrap();
        db.execute("INSERT INTO rhs VALUES (1, 'x'), (1, 'y'), (NULL, 'z');")
            .unwrap();
        let sql = "SELECT lhs.name, rhs.name FROM lhs JOIN rhs ON lhs.id = rhs.id;";
        let hashed = query(&mut db, sql);
        let nested = {
            let _force = ForceNestedJoin::new();
            query(&mut db, sql)
        };
        assert_eq!(hashed.rows, nested.rows);
        assert_eq!(hashed.rows.len(), 4);
        assert_eq!(key_ref(&Value::Null).map(hash_key), None);
        assert_eq!(
            key_ref(&Value::Integer(1)).map(hash_key),
            key_ref(&Value::Integer(1)).map(hash_key)
        );
        assert_eq!(
            key_ref(&Value::Text("Ada".to_string())).map(hash_key),
            key_ref(&Value::Text("Ada".to_string())).map(hash_key)
        );
        assert_ne!(
            key_ref(&Value::Integer(0)).map(hash_key),
            key_ref(&Value::Boolean(false)).map(hash_key)
        );
        assert!(!same_cell(&Value::Integer(0), &Value::Boolean(false)));
        assert!(!same_cell(
            &Value::Integer(0),
            &Value::Text("0".to_string())
        ));
        let position = std::mem::size_of::<usize>();
        let group = std::mem::size_of::<usize>() + std::mem::size_of::<Vec<usize>>();
        let entry = std::mem::size_of::<u64>() + std::mem::size_of::<Vec<KeyGroup>>();
        assert_eq!(hash_table_bytes(2, 5), 5 * position + 2 * group + 2 * entry);
    }

    #[test]
    fn index_probe_is_kept_only_when_the_inner_table_is_much_larger() {
        assert!(prefer_index_probe(0, 1_000));
        assert!(!prefer_index_probe(1, INDEX_PROBE_INNER_FACTOR));
        assert!(prefer_index_probe(1, INDEX_PROBE_INNER_FACTOR + 1));
        assert!(!prefer_index_probe(1_000, 20_000));
        assert!(prefer_index_probe(100, 10_000));
        assert!(!prefer_index_probe(10_000, 100));
    }

    #[test]
    fn a_forced_hash_collision_does_not_mix_keys() {
        let rows = vec![
            vec![Value::Integer(1), Value::Text("a".to_string())],
            vec![Value::Integer(2), Value::Text("b".to_string())],
            vec![Value::Integer(1), Value::Text("c".to_string())],
            vec![Value::Null, Value::Text("d".to_string())],
            vec![Value::Boolean(false), Value::Text("e".to_string())],
            vec![Value::Integer(0), Value::Text("f".to_string())],
            vec![Value::Text("Ada".to_string()), Value::Integer(7)],
            vec![Value::Text("Bea".to_string()), Value::Integer(8)],
            vec![Value::Text("Ada".to_string()), Value::Integer(9)],
        ];
        let table = build_hash(&rows, 0, |_| 0).unwrap();
        assert_eq!(table.len(), 1, "every key shares one fingerprint");
        let groups = &table[&0];
        assert_eq!(groups.len(), 6);
        let hits = matching_positions(&table, &rows, 0, &rows[0], 0, |_| 0)
            .unwrap()
            .unwrap();
        assert_eq!(hits, &[0, 2]);
        let zero = matching_positions(&table, &rows, 0, &rows[5], 0, |_| 0)
            .unwrap()
            .unwrap();
        assert_eq!(zero, &[5]);
        let text = matching_positions(&table, &rows, 0, &rows[6], 0, |_| 0)
            .unwrap()
            .unwrap();
        assert_eq!(text, &[6, 8]);
        assert!(
            matching_positions(&table, &rows, 0, &rows[3], 0, |_| 0)
                .unwrap()
                .is_none()
        );

        let mut db = Database::new();
        db.execute("CREATE TABLE lhs (id INT, name TEXT, flag BOOLEAN);")
            .unwrap();
        db.execute("CREATE TABLE rhs (id INT, name TEXT, flag BOOLEAN);")
            .unwrap();
        db.execute(
            "INSERT INTO lhs VALUES (1, 'Ada', TRUE), (2, 'Bea', FALSE), (1, 'Ada', TRUE), (NULL, 'Cy', NULL), (0, 'Dee', FALSE);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO rhs VALUES (1, 'Ada', FALSE), (2, 'Bea', TRUE), (1, 'No', TRUE), (NULL, 'Ada', NULL), (0, 'Dee', TRUE);",
        )
        .unwrap();
        let sql = "SELECT lhs.name, rhs.name FROM lhs JOIN rhs ON lhs.id = rhs.id AND lhs.name = rhs.name;";
        let nested = {
            let _force = ForceNestedJoin::new();
            query(&mut db, sql)
        };
        let collided = {
            let _force = ForceHashCollision::new();
            query(&mut db, sql)
        };
        assert_eq!(collided.rows, nested.rows);
        let ada = Value::Text("Ada".to_string());
        let bea = Value::Text("Bea".to_string());
        let dee = Value::Text("Dee".to_string());
        assert_eq!(
            collided.rows,
            vec![
                vec![ada.clone(), ada.clone()],
                vec![bea.clone(), bea],
                vec![ada.clone(), ada],
                vec![dee.clone(), dee],
            ]
        );
    }

    #[test]
    #[ignore = "release profile of hash join"]
    fn profile_hash_join() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let memory = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!("hash_join cpu={cpu} mem_bytes={memory} rustc={rustc}");
        let sql = "SELECT lhs.id, rhs.note FROM lhs JOIN rhs ON lhs.id = rhs.id;";
        for rows in [100_i64, 1_000, 4_000, 10_000] {
            profile_shape("identity", rows, rows, sql, |database, rows| {
                load_identity(database, "lhs", rows, false);
                load_identity(database, "rhs", rows, false);
            });
            profile_shape("duplicate", rows, rows, sql, |database, rows| {
                load_identity(database, "lhs", rows, false);
                load_mod(database, "rhs", rows, 50);
            });
            profile_shape("nulls", rows, rows, sql, |database, rows| {
                load_identity(database, "lhs", rows, true);
                load_identity(database, "rhs", rows, true);
            });
        }
        profile_shape("skew-outer", 100, 10_000, sql, |database, _| {
            load_identity(database, "lhs", 100, false);
            load_mod(database, "rhs", 10_000, 100);
        });
        profile_shape("skew-inner", 10_000, 100, sql, |database, _| {
            load_mod(database, "lhs", 10_000, 100);
            load_identity(database, "rhs", 100, false);
        });
        profile_shape("skew-outer-wide", 100, 40_000, sql, |database, _| {
            load_identity(database, "lhs", 100, false);
            load_mod(database, "rhs", 40_000, 100);
        });
        profile_shape("skew-outer-mid", 1_000, 20_000, sql, |database, _| {
            load_identity(database, "lhs", 1_000, false);
            load_mod(database, "rhs", 20_000, 1_000);
        });
    }

    fn profile_shape(
        shape: &str,
        outer_rows: i64,
        inner_rows: i64,
        sql: &str,
        load: impl FnOnce(&mut Database, i64),
    ) {
        let dir = crate::test_dir::TestDir::new("hash-profile");
        let path = dir.join("db");
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE TABLE lhs (id INT, note INT);")
            .unwrap();
        database
            .execute("CREATE TABLE rhs (id INT, note INT);")
            .unwrap();
        load(&mut database, outer_rows.max(inner_rows));
        drop(database);
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        let nested = profile_samples(&mut database, sql, true);
        let hashed = profile_samples(&mut database, sql, false);
        database
            .execute("CREATE INDEX idx_rhs_id ON rhs(id);")
            .unwrap();
        let indexed = {
            let _force = ForceIndexJoin::new();
            profile_samples(&mut database, sql, false)
        };
        let Value::Text(chosen) =
            query(&mut database, &format!("EXPLAIN {sql}")).rows[0][0].clone()
        else {
            panic!("expected the access path");
        };
        assert_eq!(nested.matches, hashed.matches);
        assert_eq!(hashed.matches, indexed.matches);
        assert_eq!(nested.sum, hashed.sum);
        assert_eq!(hashed.sum, indexed.sum);
        let build_inner = database.table("rhs").unwrap().rows().len()
            <= database.table("lhs").unwrap().rows().len();
        let build_rows = if build_inner {
            database.table("rhs").unwrap().rows()
        } else {
            database.table("lhs").unwrap().rows()
        };
        let mut groups = HashSet::new();
        let mut positions = 0usize;
        for row in build_rows {
            if let Value::Integer(key) = row[0] {
                groups.insert(key);
                positions += 1;
            }
        }
        let bytes = hash_table_bytes(groups.len(), positions);
        let hash_speedup = speedup(nested.warm_us, hashed.warm_us);
        let index_speedup = speedup(nested.warm_us, indexed.warm_us);
        println!(
            "hash_join shape={shape} outer={outer_rows} inner={inner_rows} matches={} build={} groups={} positions={} hash_bytes={bytes} chosen={chosen} nested_cold_us={} nested_warm_us={} hash_cold_us={} hash_warm_us={} index_cold_us={} index_warm_us={} hash_speedup={hash_speedup:.2} index_speedup={index_speedup:.2}",
            nested.matches,
            if build_inner { "rhs" } else { "lhs" },
            groups.len(),
            positions,
            nested.cold_us,
            nested.warm_us,
            hashed.cold_us,
            hashed.warm_us,
            indexed.cold_us,
            indexed.warm_us
        );
    }

    struct ProfileSample {
        cold_us: u128,
        warm_us: u128,
        matches: usize,
        sum: i64,
    }

    fn profile_samples(database: &mut Database, sql: &str, nested: bool) -> ProfileSample {
        let (cold_us, matches, sum) = profile_once(database, sql, nested);
        let mut warm = Vec::with_capacity(5);
        for _ in 0..5 {
            let (elapsed, rows, checksum) = profile_once(database, sql, nested);
            assert_eq!(rows, matches);
            assert_eq!(checksum, sum);
            warm.push(elapsed);
        }
        warm.sort_unstable();
        ProfileSample {
            cold_us,
            warm_us: warm[warm.len() / 2],
            matches,
            sum,
        }
    }

    fn profile_once(database: &mut Database, sql: &str, nested: bool) -> (u128, usize, i64) {
        let _force = nested.then(ForceNestedJoin::new);
        let started = std::time::Instant::now();
        let result = query(database, sql);
        let elapsed = started.elapsed().as_micros();
        let mut sum = 0i64;
        for row in &result.rows {
            let Value::Integer(id) = row[0] else {
                panic!("expected an integer id");
            };
            sum = sum.wrapping_add(id);
        }
        std::hint::black_box(sum);
        std::hint::black_box(result.rows.len());
        (elapsed, result.rows.len(), sum)
    }

    fn speedup(base: u128, faster: u128) -> f64 {
        if faster == 0 {
            0.0
        } else {
            base as f64 / faster as f64
        }
    }

    fn load_identity(database: &mut Database, table: &str, rows: i64, nulls: bool) {
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 499).min(rows - 1);
            let mut sql = format!("INSERT INTO {table} VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                if nulls && id % 2 == 0 {
                    sql.push_str(&format!("(NULL, {id})"));
                } else {
                    sql.push_str(&format!("({id}, {id})"));
                }
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    fn load_mod(database: &mut Database, table: &str, rows: i64, modulus: i64) {
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 499).min(rows - 1);
            let mut sql = format!("INSERT INTO {table} VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                sql.push_str(&format!("({}, {id})", id % modulus));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    fn profile_command(program: &str, args: &[&str]) -> String {
        let Ok(output) = std::process::Command::new(program).args(args).output() else {
            return "unknown".to_string();
        };
        if !output.status.success() {
            return "unknown".to_string();
        }
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[test]
    fn count_overflow_is_an_error_and_the_accumulator_is_one_slot() {
        let slot = AccValue::Count(i64::MAX);
        let error = bumped_count(&slot, "COUNT").unwrap_err();
        assert_eq!(error.to_string(), "COUNT overflows INTEGER");
        let AccValue::Count(count) = slot else {
            panic!("COUNT overflow changed the accumulator");
        };
        assert_eq!(count, i64::MAX);
        // AVG carries an i128, and MIN/MAX can own a string, so every slot is
        // the width of that larger enum. The size is recorded, not assumed.
        assert_eq!(std::mem::size_of::<AccValue>(), 48);
        assert_eq!(std::mem::size_of::<Value>(), 32);
    }

    #[test]
    fn a_failing_row_does_not_change_accumulators_already_stored() {
        let mut acc = [
            AccValue::Count(3),
            AccValue::Sum(Some(i64::MAX - 1)),
            AccValue::Sum(Some(4)),
        ];
        let row = [Value::Integer(2), Value::Integer(9)];
        let error = update_plain(
            &mut acc,
            InputRow::One(&row),
            &[AggFn::CountStar, AggFn::Sum(0), AggFn::Sum(1)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Count(3)));
        assert!(matches!(acc[1], AccValue::Sum(Some(value)) if value == i64::MAX - 1));
        assert!(matches!(acc[2], AccValue::Sum(Some(4))));

        update_plain(
            &mut acc,
            InputRow::One(&[Value::Integer(1), Value::Null]),
            &[AggFn::CountStar, AggFn::Sum(0), AggFn::Sum(1)],
        )
        .unwrap();
        assert!(matches!(acc[0], AccValue::Count(4)));
        assert!(matches!(acc[1], AccValue::Sum(Some(i64::MAX))));
        assert!(matches!(acc[2], AccValue::Sum(Some(4))));
        assert_eq!(
            join_group_bytes(usize::MAX, usize::MAX, usize::MAX),
            usize::MAX
        );
        assert_eq!(hash_table_bytes(usize::MAX, usize::MAX), usize::MAX);
    }

    #[test]
    fn a_failing_avg_does_not_replace_a_text_extreme() {
        let mut acc = [
            AccValue::Extreme(Some(Value::Text("mmm".to_string()))),
            AccValue::Avg {
                sum: 0,
                count: i64::MAX,
            },
        ];
        let row = [Value::Text("aaa".to_string()), Value::Integer(1)];
        let error = update_plain(
            &mut acc,
            InputRow::One(&row),
            &[AggFn::Min(0), AggFn::Avg(1)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows INTEGER");
        assert!(matches!(&acc[0], AccValue::Extreme(Some(Value::Text(text))) if text == "mmm"));
        assert!(matches!(acc[1], AccValue::Avg { count, .. } if count == i64::MAX));

        let mut acc = [
            AccValue::Extreme(Some(Value::Text("mmm".to_string()))),
            AccValue::Avg { sum: 1, count: 1 },
        ];
        update_plain(
            &mut acc,
            InputRow::One(&[Value::Text("zzz".to_string()), Value::Integer(4)]),
            &[AggFn::Min(0), AggFn::Avg(1)],
        )
        .unwrap();
        assert!(matches!(&acc[0], AccValue::Extreme(Some(Value::Text(text))) if text == "mmm"));
        assert!(matches!(
            &acc[1],
            AccValue::Avg { sum, count } if *sum == 5 && *count == 2
        ));
    }

    #[test]
    fn a_late_overflow_discards_pending_text_and_numeric_updates() {
        let original = [
            AccValue::Extreme(Some(Value::Text("mmm".to_string()))),
            AccValue::Extreme(Some(Value::Text("mmm".to_string()))),
            AccValue::Sum(Some(i64::MAX)),
            AccValue::Avg { sum: 1, count: 1 },
            AccValue::Count(4),
        ];
        let mut acc = original.clone();
        let row = [
            Value::Text("aaa".to_string()),
            Value::Text("zzz".to_string()),
            Value::Integer(1),
        ];
        let aggs = [
            AggFn::Min(0),
            AggFn::Max(1),
            AggFn::Sum(2),
            AggFn::Avg(2),
            AggFn::CountStar,
        ];
        let error = update_plain(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert_eq!(acc, original);

        acc[2] = AccValue::Sum(Some(1));
        acc[4] = AccValue::Count(i64::MAX);
        let before_count = acc.clone();
        let error = update_plain(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "COUNT overflows INTEGER");
        assert_eq!(acc, before_count);

        acc[3] = AccValue::Avg {
            sum: 0,
            count: i64::MAX,
        };
        acc[4] = AccValue::Count(1);
        let before_avg = acc.clone();
        let error = update_plain(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows INTEGER");
        assert_eq!(acc, before_avg);

        acc[3] = AccValue::Avg {
            sum: i128::MAX,
            count: 1,
        };
        let before_sum = acc.clone();
        let error = update_plain(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows");
        assert_eq!(acc, before_sum);

        acc[2] = AccValue::Sum(Some(i64::MAX));
        acc[3] = AccValue::Avg { sum: 1, count: 1 };
        let nulls = [Value::Null, Value::Null, Value::Integer(1)];
        let before_null = acc.clone();
        let error = update_plain(&mut acc, InputRow::One(&nulls), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert_eq!(acc, before_null);

        acc[2] = AccValue::Sum(Some(1));
        update_plain(
            &mut acc,
            InputRow::One(&[
                Value::Text("aaa".to_string()),
                Value::Text("zzz".to_string()),
                Value::Integer(4),
            ]),
            &aggs,
        )
        .unwrap();
        assert!(matches!(&acc[0], AccValue::Extreme(Some(Value::Text(text))) if text == "aaa"));
        assert!(matches!(&acc[1], AccValue::Extreme(Some(Value::Text(text))) if text == "zzz"));
        assert!(matches!(acc[2], AccValue::Sum(Some(5))));
        assert!(matches!(acc[3], AccValue::Avg { sum: 5, count: 2 }));
        assert!(matches!(acc[4], AccValue::Count(2)));
    }

    #[test]
    fn a_forced_group_hash_collision_does_not_merge_unequal_groups() {
        let distinct = [
            vec![
                Value::Integer(0),
                Value::Text("0".to_string()),
                Value::Boolean(false),
            ],
            vec![
                Value::Integer(-1),
                Value::Text("0".to_string()),
                Value::Boolean(false),
            ],
            vec![Value::Integer(i64::MIN), Value::Null, Value::Boolean(true)],
            vec![
                Value::Null,
                Value::Text("0".to_string()),
                Value::Boolean(false),
            ],
            vec![
                Value::Integer(0),
                Value::Text("0".to_string()),
                Value::Boolean(true),
            ],
            vec![Value::Null, Value::Null, Value::Null],
        ];
        assert_ne!(
            hash_group(InputRow::One(&distinct[0]), &[0, 1, 2]).unwrap(),
            hash_group(InputRow::One(&distinct[1]), &[0, 1, 2]).unwrap()
        );
        {
            let _force = ForceHashCollision::new();
            let forced = hash_group(InputRow::One(&distinct[0]), &[0, 1, 2]).unwrap();
            for row in &distinct {
                assert_eq!(hash_group(InputRow::One(row), &[0, 1, 2]).unwrap(), forced);
            }
            assert!(hash_group(InputRow::One(&distinct[0]), &[9]).is_err());
        }
        assert_ne!(
            hash_group(InputRow::One(&distinct[0]), &[0, 1, 2]).unwrap(),
            hash_group(InputRow::One(&distinct[1]), &[0, 1, 2]).unwrap()
        );

        let mut db = Database::new();
        db.execute("CREATE TABLE items (n INT, label TEXT, flag BOOLEAN);")
            .unwrap();
        db.execute(
            "INSERT INTO items VALUES
                (0, '0', FALSE),
                (-1, '0', FALSE),
                (-9223372036854775808, NULL, TRUE),
                (NULL, '0', FALSE),
                (0, '0', TRUE),
                (NULL, NULL, NULL),
                (0, '0', FALSE),
                (1, 'a', FALSE),
                (NULL, NULL, NULL);",
        )
        .unwrap();
        let grouped = "SELECT n, label, flag, COUNT(*), COUNT(n), SUM(n)
             FROM items GROUP BY n, label, flag;";
        let distinct_rows = "SELECT n, label, flag FROM items GROUP BY n, label, flag;";
        let normal = query(&mut db, grouped);
        let normal_keys = query(&mut db, distinct_rows);
        let forced = {
            let _force = ForceHashCollision::new();
            let grouped = query(&mut db, grouped);
            let keys = query(&mut db, distinct_rows);
            assert_eq!(keys.rows, normal_keys.rows);
            grouped
        };
        assert_eq!(forced.rows, normal.rows);
        assert_eq!(normal.rows.len(), 7);
        assert_eq!(
            normal.rows[0],
            vec![
                Value::Integer(0),
                Value::Text("0".to_string()),
                Value::Boolean(false),
                Value::Integer(2),
                Value::Integer(2),
                Value::Integer(0),
            ]
        );
        assert!(
            normal
                .rows
                .iter()
                .any(|row| row[0] == Value::Null && row[3] == Value::Integer(2))
        );
        assert!(
            normal
                .rows
                .iter()
                .any(|row| row[0] == Value::Integer(i64::MIN))
        );
    }

    #[test]
    fn identical_aggregates_share_one_accumulator() {
        let table = Table::new(
            "items".to_string(),
            vec![crate::schema::Column::new("n", DataType::Integer)],
        )
        .unwrap();
        let scope = Scope::from_table(&table, "items");
        let sum = AggregateCall {
            function: AggregateFunction::Sum,
            distinct: false,
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: None,
            }),
        };
        let qualified = AggregateCall {
            function: AggregateFunction::Sum,
            distinct: false,
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: Some("items".to_string()),
            }),
        };
        let mut aggs = Vec::new();
        let first = bind_aggregate(&sum, &scope, &mut aggs).unwrap().0;
        let second = bind_aggregate(&sum, &scope, &mut aggs).unwrap().0;
        let third = bind_aggregate(&qualified, &scope, &mut aggs).unwrap().0;
        assert_eq!((first, second, third, aggs.len()), (0, 0, 0, 1));
        let count = AggregateCall {
            function: AggregateFunction::Count,
            distinct: false,
            argument: AggregateArg::Star,
        };
        assert_eq!(bind_aggregate(&count, &scope, &mut aggs).unwrap().0, 1);
        assert_eq!(aggs.len(), 2);
        let avg = AggregateCall {
            function: AggregateFunction::Avg,
            distinct: false,
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: Some("items".to_string()),
            }),
        };
        let again = AggregateCall {
            function: AggregateFunction::Avg,
            distinct: false,
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: None,
            }),
        };
        assert_eq!(bind_aggregate(&avg, &scope, &mut aggs).unwrap().0, 2);
        assert_eq!(bind_aggregate(&again, &scope, &mut aggs).unwrap().0, 2);
        assert_eq!(aggs.len(), 3);
    }

    #[test]
    fn join_aggregates_agree_across_join_algorithms() {
        let grouped = "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id
            WHERE o.paid = TRUE
            GROUP BY u.name
            HAVING COUNT(*) >= 1
            ORDER BY SUM(o.amount) DESC, u.name ASC;";
        let mut memory = Database::new();
        load_algorithm_shop(&mut memory, 2, 6);
        let hash_rows = query(&mut memory, grouped).rows;
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            query(&mut memory, grouped).rows
        };
        assert_eq!(nested_rows, hash_rows);
        let streamed = hash_rows;
        let materialized = {
            let _guard = MaterializeAggregate::new();
            query(&mut memory, grouped).rows
        };
        assert_eq!(materialized, streamed);

        let mut wide = Database::new();
        load_algorithm_shop(&mut wide, 6, 2);
        let hash_rows = query(&mut wide, grouped).rows;
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            query(&mut wide, grouped).rows
        };
        assert_eq!(nested_rows, hash_rows);

        let dir = crate::test_dir::TestDir::new("join-agg-algorithms");
        let path = dir.join("db");
        let mut database = Database::open(&path).unwrap();
        load_algorithm_shop(&mut database, 4, 6);
        let hash_rows = query(&mut database, grouped).rows;
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            query(&mut database, grouped).rows
        };
        assert_eq!(nested_rows, hash_rows);
        drop(database);
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
            .unwrap();
        let indexed = {
            let _force = ForceIndexJoin::new();
            query(&mut database, grouped).rows
        };
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            query(&mut database, grouped).rows
        };
        assert_eq!(indexed, hash_rows);
        assert_eq!(nested_rows, hash_rows);
        let explained = {
            let _force = ForceIndexJoin::new();
            query(&mut database, &format!("EXPLAIN {grouped}"))
        };
        assert_eq!(
            explained.rows[0][4],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        assert_eq!(
            explained.rows[0][8],
            Value::Text("idx_orders_user".to_string())
        );
    }

    fn load_algorithm_shop(database: &mut Database, users: i64, orders: i64) {
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
            .unwrap();
        let mut sql = String::from("INSERT INTO users VALUES ");
        for id in 0..users {
            if id > 0 {
                sql.push(',');
            }
            if id == users - 1 {
                sql.push_str("(NULL, 'tail')");
            } else {
                sql.push_str(&format!("({id}, 'u{id}')"));
            }
        }
        database.execute(&sql).unwrap();
        let mut sql = String::from("INSERT INTO orders VALUES ");
        for id in 0..orders {
            if id > 0 {
                sql.push(',');
            }
            let user = if id % 5 == 0 {
                "NULL".to_string()
            } else {
                ((id % users.max(1)) % (users - 1).max(1)).to_string()
            };
            let amount = if id % 3 == 0 {
                "NULL".to_string()
            } else {
                id.to_string()
            };
            let paid = if id % 4 == 0 { "FALSE" } else { "TRUE" };
            sql.push_str(&format!("({user}, {amount}, {paid})"));
        }
        database.execute(&sql).unwrap();
    }

    /// Seeds compared with the independent join reference in this review.
    const REVIEW_SEEDS: [u64; 6] = [0x0a99, 0x51c0, 0xc0ff, 0x5eed, 0xa11c, 0x11ab];

    #[test]
    fn integer_join_aggregates_match_a_reference_on_both_build_sides() {
        let (outer, inner) = outer_build_shop();
        let grouped = aggregate_sql(
            "u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)",
            "users u",
            "orders o",
            "u.id = o.user_id",
            "",
            "u.name",
            "",
        );
        let expected = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[1].clone()],
        ));
        assert_eq!(
            expected,
            vec![
                vec![
                    Value::Text("Ada".to_string()),
                    Value::Integer(20),
                    Value::Integer(10),
                    Value::Integer(30),
                ],
                vec![
                    Value::Text("Al".to_string()),
                    Value::Integer(10),
                    Value::Integer(5),
                    Value::Integer(15),
                ],
                vec![
                    Value::Text("Bea".to_string()),
                    Value::Integer(2),
                    Value::Integer(1),
                    Value::Integer(8),
                ],
                vec![
                    Value::Text("Dee".to_string()),
                    Value::Integer(2),
                    Value::Integer(0),
                    Value::Null,
                ],
            ]
        );
        let pairs = pair_rows(
            &outer,
            &inner,
            |left, right| sql_eq(&left[0], &right[0]),
            |left, right| vec![left[1].clone(), right[1].clone()],
        );
        assert_eq!(pairs.len(), 34);
        let by_flag = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[2].clone()],
        ));
        assert_eq!(by_flag[0][0], Value::Boolean(true));
        assert_eq!(by_flag[0][1], Value::Integer(22));
        assert_eq!(by_flag[0][2], Value::Integer(10));
        assert_eq!(by_flag[0][3], Value::Integer(30));
        let positive = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            Some(&|_, inner| matches!(inner[1], Value::Integer(amount) if amount > 0)),
            false,
            1,
            |row, _| vec![row[1].clone()],
        ));
        let multiple = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[1].clone(), row[2].clone()],
        ));
        let mut ordered = aggregate_equijoin(&outer, &inner, 0, 0, None, false, 1, |row, _| {
            vec![row[1].clone()]
        });
        ordered.retain(|group| group.count >= 2);
        order_groups(&mut ordered, 0);
        let ordered = project_groups(&ordered)
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        let first = expected.first().cloned().into_iter().collect::<Vec<_>>();
        let load = |database: &mut Database| {
            load_tables(
                database,
                "CREATE TABLE users (id INT, name TEXT, flag BOOLEAN);",
                "CREATE TABLE orders (user_id INT, amount INT, flag BOOLEAN);",
                "users",
                "orders",
                &outer,
                &inner,
            );
        };
        assert_cases(
            &load,
            &[
                (grouped.as_str(), expected.clone()),
                (
                    "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
                    pairs,
                ),
                (
                    "SELECT u.flag, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id
                     GROUP BY u.flag;",
                    by_flag,
                ),
                (
                    "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0
                     GROUP BY u.name;",
                    positive.clone(),
                ),
                (
                    "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id
                     WHERE o.amount > 0
                     GROUP BY u.name;",
                    positive,
                ),
                (
                    "SELECT u.name, u.flag, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id
                     GROUP BY u.name, u.flag;",
                    multiple,
                ),
                (
                    "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id
                     GROUP BY u.name
                     HAVING COUNT(*) >= 2
                     ORDER BY SUM(o.amount) DESC NULLS LAST, u.name ASC
                     LIMIT 2;",
                    ordered,
                ),
                (
                    "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                     FROM users u JOIN orders o ON u.id = o.user_id
                     GROUP BY u.name
                     LIMIT 1;",
                    first,
                ),
            ],
            Some("CREATE INDEX idx_orders_user ON orders(user_id);"),
        );

        let (wide_outer, wide_inner) = inner_build_shop();
        let expected = project_groups(&aggregate_equijoin(
            &wide_outer,
            &wide_inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[1].clone()],
        ));
        assert_eq!(
            expected,
            vec![
                vec![
                    Value::Text("n0".to_string()),
                    Value::Integer(2),
                    Value::Integer(1),
                    Value::Integer(5),
                ],
                vec![
                    Value::Text("n0b".to_string()),
                    Value::Integer(2),
                    Value::Integer(1),
                    Value::Integer(5),
                ],
                vec![
                    Value::Text("n1".to_string()),
                    Value::Integer(1),
                    Value::Integer(1),
                    Value::Integer(8),
                ],
            ]
        );
        let load = |database: &mut Database| {
            load_tables(
                database,
                "CREATE TABLE users (id INT, name TEXT, flag BOOLEAN);",
                "CREATE TABLE orders (user_id INT, amount INT, flag BOOLEAN);",
                "users",
                "orders",
                &wide_outer,
                &wide_inner,
            );
        };
        assert_cases(
            &load,
            &[(
                "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                 FROM users u JOIN orders o ON u.id = o.user_id
                 GROUP BY u.name;",
                expected,
            )],
            Some("CREATE INDEX idx_orders_user ON orders(user_id);"),
        );
    }

    #[test]
    fn text_and_boolean_join_aggregates_match_a_reference() {
        let outer = vec![
            vec![Value::Text("Ada".to_string()), Value::Integer(1)],
            vec![Value::Text("Ada".to_string()), Value::Integer(2)],
            vec![Value::Null, Value::Integer(3)],
            vec![Value::Text("Bea".to_string()), Value::Integer(4)],
            vec![Value::Text(String::new()), Value::Integer(5)],
        ];
        let inner = vec![
            vec![Value::Text("Ada".to_string()), Value::Integer(10)],
            vec![Value::Text("Ada".to_string()), Value::Null],
            vec![Value::Null, Value::Integer(7)],
            vec![Value::Text(String::new()), Value::Integer(8)],
            vec![Value::Text("Cy".to_string()), Value::Integer(9)],
        ];
        let expected = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[0].clone()],
        ));
        assert_eq!(
            expected,
            vec![
                vec![
                    Value::Text("Ada".to_string()),
                    Value::Integer(4),
                    Value::Integer(2),
                    Value::Integer(20),
                ],
                vec![
                    Value::Text(String::new()),
                    Value::Integer(1),
                    Value::Integer(1),
                    Value::Integer(8),
                ],
            ]
        );
        let load = |database: &mut Database| {
            load_tables(
                database,
                "CREATE TABLE users (name TEXT, score INT);",
                "CREATE TABLE codes (label TEXT, amount INT);",
                "users",
                "codes",
                &outer,
                &inner,
            );
        };
        assert_cases(
            &load,
            &[(
                "SELECT u.name, COUNT(*), COUNT(c.amount), SUM(c.amount)
                 FROM users u JOIN codes c ON u.name = c.label
                 GROUP BY u.name;",
                expected,
            )],
            None,
        );

        let outer = vec![
            vec![Value::Boolean(true), Value::Text("a".to_string())],
            vec![Value::Boolean(true), Value::Text("b".to_string())],
            vec![Value::Boolean(false), Value::Text("c".to_string())],
            vec![Value::Null, Value::Text("d".to_string())],
            vec![Value::Boolean(false), Value::Text("e".to_string())],
        ];
        let inner = vec![
            vec![Value::Boolean(true), Value::Integer(5)],
            vec![Value::Boolean(true), Value::Null],
            vec![Value::Boolean(false), Value::Integer(7)],
            vec![Value::Null, Value::Integer(9)],
        ];
        let expected = project_groups(&aggregate_equijoin(
            &outer,
            &inner,
            0,
            0,
            None,
            false,
            1,
            |row, _| vec![row[1].clone()],
        ));
        assert_eq!(
            expected,
            vec![
                row4("a", 2, 1, Some(5)),
                row4("b", 2, 1, Some(5)),
                row4("c", 1, 1, Some(7)),
                row4("e", 1, 1, Some(7)),
            ]
        );
        let load = |database: &mut Database| {
            load_tables(
                database,
                "CREATE TABLE users (flag BOOLEAN, name TEXT);",
                "CREATE TABLE bits (flag BOOLEAN, amount INT);",
                "users",
                "bits",
                &outer,
                &inner,
            );
        };
        assert_cases(
            &load,
            &[(
                "SELECT u.name, COUNT(*), COUNT(b.amount), SUM(b.amount)
                 FROM users u JOIN bits b ON u.flag = b.flag
                 GROUP BY u.name;",
                expected,
            )],
            None,
        );
    }

    #[test]
    fn self_join_aggregates_match_across_algorithms() {
        let rows = vec![
            vec![Value::Integer(1), Value::Text("Ada".to_string())],
            vec![Value::Integer(1), Value::Text("Bea".to_string())],
            vec![Value::Null, Value::Text("Cy".to_string())],
            vec![Value::Integer(2), Value::Text("Dee".to_string())],
            vec![Value::Integer(1), Value::Text("Ann".to_string())],
        ];
        let expected = project_groups(&aggregate_equijoin(
            &rows,
            &rows,
            0,
            0,
            None,
            true,
            0,
            |row, _| vec![row[1].clone()],
        ));
        assert_eq!(
            expected,
            vec![
                row4("Ada", 3, 3, Some(3)),
                row4("Bea", 3, 3, Some(3)),
                row4("Dee", 1, 1, Some(2)),
                row4("Ann", 3, 3, Some(3)),
            ]
        );
        let load = |database: &mut Database| {
            database
                .execute("CREATE TABLE staff (team INT, name TEXT);")
                .unwrap();
            insert_rows(database, "staff", &rows);
        };
        assert_cases(
            &load,
            &[(
                "SELECT e.name, COUNT(*), COUNT(e.team), SUM(e.team)
                 FROM staff e JOIN staff b ON e.team = b.team
                 GROUP BY e.name;",
                expected,
            )],
            Some("CREATE INDEX idx_staff_team ON staff(team);"),
        );
    }

    #[test]
    fn empty_join_aggregates_match_across_algorithms() {
        let scalar = vec![vec![Value::Integer(0), Value::Integer(0), Value::Null]];
        let grouped = "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id
            GROUP BY u.name;";
        let scalar_sql = "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id;";
        let kept = "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id
            HAVING COUNT(*) = 0;";
        let dropped = "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id
            HAVING COUNT(*) >= 1;";
        let load_empty = |database: &mut Database| {
            database
                .execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT);")
                .unwrap();
        };
        assert_cases(
            &load_empty,
            &[
                (scalar_sql, scalar.clone()),
                (kept, scalar),
                (dropped, Vec::new()),
                (grouped, Vec::new()),
            ],
            Some("CREATE INDEX idx_orders_user ON orders(user_id);"),
        );
        let load_users = |database: &mut Database| {
            database
                .execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT);")
                .unwrap();
            database
                .execute("INSERT INTO users VALUES (1, 'Ada');")
                .unwrap();
        };
        assert_cases(
            &load_users,
            &[(
                scalar_sql,
                vec![vec![Value::Integer(0), Value::Integer(0), Value::Null]],
            )],
            Some("CREATE INDEX idx_orders_user ON orders(user_id);"),
        );
    }

    #[test]
    fn recorded_seeds_match_across_join_algorithms() {
        for seed in REVIEW_SEEDS {
            let (outer, inner) = generate_review_shop(seed);
            let grouped = project_groups(&aggregate_equijoin(
                &outer,
                &inner,
                0,
                0,
                None,
                false,
                1,
                |row, _| vec![row[1].clone()],
            ));
            let pair_count = pair_rows(
                &outer,
                &inner,
                |left, right| sql_eq(&left[0], &right[0]),
                |_, _| Vec::new(),
            )
            .len() as i64;
            let summed: i64 = grouped
                .iter()
                .map(|row| match row[1] {
                    Value::Integer(count) => count,
                    _ => panic!("count"),
                })
                .sum();
            assert_eq!(summed, pair_count, "seed {seed:#x}");
            let paid = project_groups(&aggregate_equijoin(
                &outer,
                &inner,
                0,
                0,
                Some(&|_, inner| matches!(inner[2], Value::Boolean(true))),
                false,
                1,
                |row, _| vec![row[1].clone()],
            ));
            let mut having = aggregate_equijoin(&outer, &inner, 0, 0, None, false, 1, |row, _| {
                vec![row[1].clone()]
            });
            having.retain(|group| group.count >= 2 && matches!(group.sum, Some(sum) if sum > 0));
            let having = project_groups(&having);
            let both = project_groups(&aggregate_equijoin(
                &outer,
                &inner,
                0,
                0,
                None,
                false,
                1,
                |row, _| vec![row[1].clone(), row[2].clone()],
            ));
            let load = |database: &mut Database| {
                load_tables(
                    database,
                    "CREATE TABLE users (id INT, name TEXT, flag BOOLEAN);",
                    "CREATE TABLE orders (user_id INT, amount INT, flag BOOLEAN);",
                    "users",
                    "orders",
                    &outer,
                    &inner,
                );
            };
            assert_cases(
                &load,
                &[
                    (
                        "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                         FROM users u JOIN orders o ON u.id = o.user_id
                         GROUP BY u.name;",
                        grouped,
                    ),
                    (
                        "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                         FROM users u JOIN orders o ON u.id = o.user_id
                         WHERE o.flag = TRUE
                         GROUP BY u.name;",
                        paid,
                    ),
                    (
                        "SELECT u.name, COUNT(*), COUNT(o.amount), SUM(o.amount)
                         FROM users u JOIN orders o ON u.id = o.user_id
                         GROUP BY u.name
                         HAVING COUNT(*) >= 2 AND SUM(o.amount) > 0;",
                        having,
                    ),
                    (
                        "SELECT u.name, u.flag, COUNT(*), COUNT(o.amount), SUM(o.amount)
                         FROM users u JOIN orders o ON u.id = o.user_id
                         GROUP BY u.name, u.flag;",
                        both,
                    ),
                ],
                Some("CREATE INDEX idx_orders_user ON orders(user_id);"),
            );
        }
    }

    #[test]
    fn late_overflow_agrees_across_join_algorithms() {
        let load = |database: &mut Database| {
            database
                .execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            database
                .execute("CREATE TABLE orders (user_id INT, amount INT);")
                .unwrap();
            database
                .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
                .unwrap();
            database
                .execute(
                    "INSERT INTO orders VALUES (1, 10), (1, 5), (2, 9223372036854775807), (2, 1);",
                )
                .unwrap();
        };
        let queries = [
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING u.name = 'Ada';",
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING SUM(o.amount) < 0;",
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name LIMIT 1;",
            "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name LIMIT 0;",
        ];
        let mut memory = Database::new();
        load(&mut memory);
        for sql in queries {
            assert_overflow(&mut memory, sql, "");
            let nested = {
                let _force = ForceNestedJoin::new();
                memory.execute(sql).unwrap_err().to_string()
            };
            assert_eq!(nested, "SUM overflows INTEGER", "{sql}");
            let stored = {
                let _guard = MaterializeAggregate::new();
                memory.execute(sql).unwrap_err().to_string()
            };
            assert_eq!(stored, "SUM overflows INTEGER", "{sql}");
        }
        assert_eq!(
            query(&mut memory, "SELECT COUNT(*) FROM users;").rows,
            vec![vec![Value::Integer(2)]]
        );
        let dir = crate::test_dir::TestDir::new("review-overflow");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load(&mut database);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        for sql in queries {
            let indexed = {
                let _force = ForceIndexJoin::new();
                database.execute(sql).unwrap_err().to_string()
            };
            assert_eq!(indexed, "SUM overflows INTEGER", "{sql}");
        }
        let explained = {
            let _force = ForceIndexJoin::new();
            query(
                &mut database,
                "EXPLAIN SELECT u.name, SUM(o.amount)
                 FROM users u JOIN orders o ON u.id = o.user_id
                 GROUP BY u.name HAVING u.name = 'Ada' LIMIT 1;",
            )
        };
        assert_eq!(
            explained.rows[0][4],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        drop(database);
        let mut wal = path.clone().into_os_string();
        wal.push(".wal");
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    }

    #[test]
    fn explain_reports_the_algorithm_execution_uses() {
        let (outer, inner) = outer_build_shop();
        let mut database = Database::new();
        load_tables(
            &mut database,
            "CREATE TABLE users (id INT, name TEXT, flag BOOLEAN);",
            "CREATE TABLE orders (user_id INT, amount INT, flag BOOLEAN);",
            "users",
            "orders",
            &outer,
            &inner,
        );
        let sql = "SELECT u.name, COUNT(*), SUM(o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0
            GROUP BY u.name
            HAVING SUM(o.amount) > 0 AND SUM(o.amount) > 0
            ORDER BY COUNT(*);";
        let hashed = query(&mut database, sql).rows;
        let explained = query(&mut database, &format!("EXPLAIN {sql}"));
        assert_eq!(
            explained.rows[0][2],
            Value::Text("COUNT(*), SUM(o.amount)".to_string())
        );
        assert_eq!(explained.rows[0][4], Value::Text("HashJoin".to_string()));
        assert_eq!(explained.rows[0][7], Value::Text("u (users)".to_string()));
        assert_eq!(
            explained.rows[0][9],
            Value::Text("u.id = o.user_id".to_string())
        );
        assert_eq!(
            explained.rows[0][10],
            Value::Text("u.id = o.user_id AND o.amount > 0".to_string())
        );
        let nested_plan = {
            let _force = ForceNestedJoin::new();
            let explained = query(&mut database, &format!("EXPLAIN {sql}"));
            let rows = query(&mut database, sql).rows;
            assert_eq!(rows, hashed);
            explained
        };
        assert_eq!(
            nested_plan.rows[0][4],
            Value::Text("NestedLoopJoin".to_string())
        );
        let plain = "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;";
        assert_eq!(
            query(&mut database, &format!("EXPLAIN {plain}")).rows[0][0],
            Value::Text("HashJoin".to_string())
        );
        {
            let _force = ForceNestedJoin::new();
            assert_eq!(
                query(&mut database, &format!("EXPLAIN {plain}")).rows[0][0],
                Value::Text("NestedLoopJoin".to_string())
            );
        }

        let dir = crate::test_dir::TestDir::new("review-explain");
        let path = dir.join("db");
        {
            let mut stored = Database::open(&path).unwrap();
            load_tables(
                &mut stored,
                "CREATE TABLE users (id INT, name TEXT, flag BOOLEAN);",
                "CREATE TABLE orders (user_id INT, amount INT, flag BOOLEAN);",
                "users",
                "orders",
                &outer,
                &inner,
            );
        }
        Database::migrate(&path).unwrap();
        let mut stored = Database::open(&path).unwrap();
        stored
            .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();
        let unforced = query(&mut stored, &format!("EXPLAIN {sql}"));
        assert_eq!(unforced.rows[0][4], Value::Text("HashJoin".to_string()));
        let indexed = {
            let _force = ForceIndexJoin::new();
            let explained = query(&mut stored, &format!("EXPLAIN {sql}"));
            let rows = query(&mut stored, sql).rows;
            assert_eq!(rows, hashed);
            explained
        };
        assert_eq!(
            indexed.rows[0][4],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        assert_eq!(
            indexed.rows[0][8],
            Value::Text("idx_orders_user".to_string())
        );
        let ambiguous =
            "SELECT flag, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY flag;";
        let select_error = stored.execute(ambiguous).unwrap_err().to_string();
        let explain_error = stored
            .execute(&format!("EXPLAIN {ambiguous}"))
            .unwrap_err()
            .to_string();
        assert_eq!(select_error, "column 'flag' is ambiguous");
        assert_eq!(explain_error, select_error);
        drop(stored);
        let mut wal = path.clone().into_os_string();
        wal.push(".wal");
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);
    }

    fn assert_overflow(database: &mut Database, sql: &str, label: &str) {
        assert_eq!(
            database.execute(sql).unwrap_err().to_string(),
            "SUM overflows INTEGER",
            "{label}{sql}"
        );
    }

    fn assert_cases(
        load: &dyn Fn(&mut Database),
        cases: &[(&str, Vec<Vec<Value>>)],
        index_sql: Option<&str>,
    ) {
        let mut memory = Database::new();
        load(&mut memory);
        for (sql, expected) in cases {
            assert_eq!(query(&mut memory, sql).rows, *expected, "hash {sql}");
            let nested = {
                let _force = ForceNestedJoin::new();
                query(&mut memory, sql).rows
            };
            assert_eq!(nested, *expected, "nested {sql}");
            let stored = {
                let _guard = MaterializeAggregate::new();
                query(&mut memory, sql).rows
            };
            assert_eq!(stored, *expected, "materialized {sql}");
            let nested_stored = {
                let _force = ForceNestedJoin::new();
                let _guard = MaterializeAggregate::new();
                query(&mut memory, sql).rows
            };
            assert_eq!(nested_stored, *expected, "nested materialized {sql}");
            let collided = {
                let _force = ForceHashCollision::new();
                query(&mut memory, sql).rows
            };
            assert_eq!(collided, *expected, "collided {sql}");
        }
        let Some(index_sql) = index_sql else {
            return;
        };
        let dir = crate::test_dir::TestDir::new("review-algorithms");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load(&mut database);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database.execute(index_sql).unwrap();
        let _force = ForceIndexJoin::new();
        for (sql, expected) in cases {
            assert_eq!(query(&mut database, sql).rows, *expected, "index {sql}");
        }
    }

    fn aggregate_sql(
        projection: &str,
        outer: &str,
        inner: &str,
        on: &str,
        selection: &str,
        groups: &str,
        tail: &str,
    ) -> String {
        format!(
            "SELECT {projection} FROM {outer} JOIN {inner} ON {on}{selection} GROUP BY {groups}{tail};"
        )
    }

    fn row4(name: &str, count: i64, counted: i64, sum: Option<i64>) -> Vec<Value> {
        vec![
            Value::Text(name.to_string()),
            Value::Integer(count),
            Value::Integer(counted),
            sum.map(Value::Integer).unwrap_or(Value::Null),
        ]
    }

    struct RefGroup {
        key: Vec<Value>,
        count: i64,
        counted: i64,
        sum: Option<i64>,
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn aggregate_equijoin(
        outer: &[Vec<Value>],
        inner: &[Vec<Value>],
        outer_key: usize,
        inner_key: usize,
        extra: Option<&dyn Fn(&[Value], &[Value]) -> bool>,
        sum_outer: bool,
        sum_at: usize,
        group_key: impl Fn(&[Value], &[Value]) -> Vec<Value>,
    ) -> Vec<RefGroup> {
        let mut groups: Vec<RefGroup> = Vec::new();
        for outer_row in outer {
            for inner_row in inner {
                if !sql_eq(&outer_row[outer_key], &inner_row[inner_key]) {
                    continue;
                }
                if extra.is_some_and(|extra| !extra(outer_row, inner_row)) {
                    continue;
                }
                let key = group_key(outer_row, inner_row);
                let summed = if sum_outer { outer_row } else { inner_row };
                let amount = match summed.get(sum_at) {
                    Some(Value::Null) => None,
                    Some(Value::Integer(amount)) => Some(*amount),
                    Some(other) => panic!("amount is {other:?}"),
                    None => panic!("amount column is outside the row"),
                };
                if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
                    group.count += 1;
                    if let Some(amount) = amount {
                        group.counted += 1;
                        group.sum = Some(group.sum.unwrap_or(0).checked_add(amount).unwrap());
                    }
                } else {
                    groups.push(RefGroup {
                        key,
                        count: 1,
                        counted: i64::from(amount.is_some()),
                        sum: amount,
                    });
                }
            }
        }
        groups
    }

    fn project_groups(groups: &[RefGroup]) -> Vec<Vec<Value>> {
        groups
            .iter()
            .map(|group| {
                let mut row = group.key.clone();
                row.push(Value::Integer(group.count));
                row.push(Value::Integer(group.counted));
                row.push(group.sum.map(Value::Integer).unwrap_or(Value::Null));
                row
            })
            .collect()
    }

    fn order_groups(groups: &mut [RefGroup], name_at: usize) {
        groups.sort_by(|left, right| {
            cmp_sum_desc_nulls_last(left.sum, right.sum)
                .then_with(|| cmp_asc_nulls_last(&left.key[name_at], &right.key[name_at]))
        });
    }

    fn cmp_sum_desc_nulls_last(left: Option<i64>, right: Option<i64>) -> Ordering {
        match (left, right) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => right.cmp(&left),
        }
    }

    fn cmp_asc_nulls_last(left: &Value, right: &Value) -> Ordering {
        match (left, right) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
            (Value::Text(left), Value::Text(right)) => left.cmp(right),
            (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
            _ => Ordering::Equal,
        }
    }

    fn pair_rows(
        outer: &[Vec<Value>],
        inner: &[Vec<Value>],
        keep: impl Fn(&[Value], &[Value]) -> bool,
        project: impl Fn(&[Value], &[Value]) -> Vec<Value>,
    ) -> Vec<Vec<Value>> {
        let mut rows = Vec::new();
        for outer_row in outer {
            for inner_row in inner {
                if keep(outer_row, inner_row) {
                    rows.push(project(outer_row, inner_row));
                }
            }
        }
        rows
    }

    fn sql_eq(left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Integer(left), Value::Integer(right)) => left == right,
            (Value::Text(left), Value::Text(right)) => left == right,
            (Value::Boolean(left), Value::Boolean(right)) => left == right,
            _ => false,
        }
    }

    fn outer_build_shop() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
        let outer = vec![
            shop_user(Some(1), "Ada", Some(true)),
            shop_user(Some(1), "Ada", Some(false)),
            shop_user(Some(1), "Al", Some(true)),
            shop_user(Some(2), "Bea", None),
            shop_user(Some(3), "Dee", Some(true)),
            shop_user(None, "Cy", Some(false)),
        ];
        let mut inner = Vec::new();
        for amount in 1..=5 {
            inner.push(shop_order(Some(1), Some(amount), Some(true)));
            inner.push(shop_order(Some(1), None, Some(false)));
        }
        inner.push(shop_order(Some(2), None, Some(true)));
        inner.push(shop_order(Some(2), Some(8), Some(true)));
        inner.push(shop_order(Some(3), None, Some(true)));
        inner.push(shop_order(Some(3), None, Some(false)));
        inner.push(shop_order(None, Some(5), Some(true)));
        inner.push(shop_order(Some(9), Some(4), Some(true)));
        (outer, inner)
    }

    fn inner_build_shop() -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
        let mut outer = Vec::new();
        outer.push(shop_user(Some(0), "n0", Some(true)));
        outer.push(shop_user(Some(0), "n0b", Some(false)));
        for id in 1..7 {
            outer.push(shop_user(Some(id), &format!("n{id}"), Some(id % 2 == 0)));
        }
        outer.push(shop_user(None, "tail", None));
        let inner = vec![
            shop_order(Some(0), Some(5), Some(true)),
            shop_order(Some(0), None, Some(false)),
            shop_order(Some(1), Some(8), Some(true)),
        ];
        (outer, inner)
    }

    fn shop_user(id: Option<i64>, name: &str, flag: Option<bool>) -> Vec<Value> {
        vec![opt_int(id), Value::Text(name.to_string()), opt_bool(flag)]
    }

    fn shop_order(user_id: Option<i64>, amount: Option<i64>, flag: Option<bool>) -> Vec<Value> {
        vec![opt_int(user_id), opt_int(amount), opt_bool(flag)]
    }

    fn copied_int(value: &Value) -> Option<i64> {
        match value {
            Value::Integer(value) => Some(*value),
            _ => None,
        }
    }

    fn opt_int(value: Option<i64>) -> Value {
        value.map(Value::Integer).unwrap_or(Value::Null)
    }

    fn opt_bool(value: Option<bool>) -> Value {
        value.map(Value::Boolean).unwrap_or(Value::Null)
    }

    fn load_tables(
        database: &mut Database,
        outer_sql: &str,
        inner_sql: &str,
        outer_name: &str,
        inner_name: &str,
        outer: &[Vec<Value>],
        inner: &[Vec<Value>],
    ) {
        database.execute(outer_sql).unwrap();
        if outer_name != inner_name {
            database.execute(inner_sql).unwrap();
        }
        insert_rows(database, outer_name, outer);
        if outer_name != inner_name {
            insert_rows(database, inner_name, inner);
        }
    }

    fn insert_rows(database: &mut Database, table: &str, rows: &[Vec<Value>]) {
        if rows.is_empty() {
            return;
        }
        let mut sql = format!("INSERT INTO {table} VALUES ");
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                sql.push(',');
            }
            sql.push('(');
            for (column, value) in row.iter().enumerate() {
                if column > 0 {
                    sql.push(',');
                }
                sql.push_str(&sql_literal(value));
            }
            sql.push(')');
        }
        database.execute(&sql).unwrap();
    }

    fn sql_literal(value: &Value) -> String {
        match value {
            Value::Null => "NULL".to_string(),
            Value::Integer(value) => value.to_string(),
            Value::Boolean(true) => "TRUE".to_string(),
            Value::Boolean(false) => "FALSE".to_string(),
            Value::Text(text) => {
                let mut out = String::from("'");
                for ch in text.chars() {
                    if ch == '\'' {
                        out.push('\'');
                    }
                    out.push(ch);
                }
                out.push('\'');
                out
            }
            Value::Ratio { .. } => value.to_string(),
        }
    }

    fn generate_review_shop(seed: u64) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        let users = 4 + (next() % 5) as usize;
        let mut outer: Vec<Vec<Value>> = Vec::with_capacity(users);
        for index in 0..users {
            let mut id = if next() % 5 == 0 {
                None
            } else {
                Some(i64::try_from(index).unwrap())
            };
            if index > 0 && next() % 4 == 0 {
                id = copied_int(&outer[0][0]);
            }
            let flag = match next() % 3 {
                0 => None,
                1 => Some(false),
                _ => Some(true),
            };
            outer.push(shop_user(id, &format!("n{index}"), flag));
        }
        let orders = 6 + (next() % 10) as usize;
        let mut inner = Vec::with_capacity(orders);
        for index in 0..orders {
            let user_id = match next() % 5 {
                0 => None,
                1 => Some(50 + i64::try_from(index).unwrap()),
                _ => copied_int(&outer[(next() as usize) % outer.len()][0]),
            };
            let amount = if next() % 3 == 0 {
                None
            } else {
                Some(i64::try_from(next() % 20).unwrap())
            };
            let flag = match next() % 3 {
                0 => None,
                1 => Some(false),
                _ => Some(true),
            };
            inner.push(shop_order(user_id, amount, flag));
        }
        (outer, inner)
    }

    #[test]
    #[ignore = "release profile of join aggregation"]
    fn profile_join_aggregation() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let memory = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!("join_aggregate cpu={cpu} mem_bytes={memory} rustc={rustc}");
        for users in [200_i64, 1_000, 4_000] {
            profile_join_shape(users, "bucket", false, false);
            profile_join_shape(users, "id", false, false);
            profile_join_shape(users, "bucket", true, false);
        }
        for users in [1_000_i64, 4_000] {
            profile_join_shape(users, "bucket", false, true);
            profile_join_shape(users, "id", false, true);
        }
        let dir = crate::test_dir::TestDir::new("join-agg-profile");
        let path = dir.join("db");
        for users in [200_i64, 1_000, 4_000] {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}.wal", path.display()));
            let mut database = Database::open(&path).unwrap();
            load_join_profile(&mut database, users);
            drop(database);
            Database::migrate(&path).unwrap();
            let mut database = Database::open(&path).unwrap();
            database
                .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
                .unwrap();
            profile_join_query(&mut database, users, "index", "bucket", false, false);
            profile_join_query(&mut database, users, "index", "id", false, false);
            profile_join_query(&mut database, users, "index", "bucket", true, false);
        }
    }

    fn profile_join_shape(users: i64, key: &str, having: bool, materialize: bool) {
        let mut nested = Database::new();
        load_join_profile(&mut nested, users);
        profile_join_query(&mut nested, users, "nested", key, having, materialize);
        let mut hashed = Database::new();
        load_join_profile(&mut hashed, users);
        profile_join_query(&mut hashed, users, "hash", key, having, materialize);
    }

    fn profile_join_query(
        database: &mut Database,
        users: i64,
        algorithm: &str,
        key: &str,
        having: bool,
        materialize: bool,
    ) {
        let threshold = users.saturating_mul(4) / 10;
        let having_sql = if having {
            format!(" HAVING COUNT(*) >= {threshold}")
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT u.{key}, COUNT(*), SUM(o.amount)
             FROM users u JOIN orders o ON u.id = o.user_id
             GROUP BY u.{key}{having_sql};"
        );
        let _nested = (algorithm == "nested").then(ForceNestedJoin::new);
        let _index = (algorithm == "index").then(ForceIndexJoin::new);
        let _stored = materialize.then(MaterializeAggregate::new);
        let mode = if materialize {
            "materialized"
        } else {
            "stream"
        };
        let mut samples = Vec::with_capacity(6);
        let mut groups = 0usize;
        let mut checksum = 0i64;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, &sql);
            samples.push(started.elapsed().as_micros());
            checksum = 0;
            for row in &result.rows {
                let Value::Integer(count) = row[1] else {
                    panic!("expected a count");
                };
                checksum = checksum.wrapping_add(count);
            }
            groups = result.rows.len();
            std::hint::black_box(&result.rows);
        }
        let orders = users.saturating_mul(4);
        let matches = usize::try_from(orders).unwrap_or(usize::MAX);
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        let returned = groups;
        let accumulated = if key == "bucket" { 10 } else { returned };
        let bytes = join_group_bytes(accumulated, 1, 2);
        let pair_bytes = matches.saturating_mul(24 + 5 * 24);
        println!(
            "join_aggregate algo={algorithm} mode={mode} users={users} orders={orders} matches={matches} key={key} returned={returned} groups={accumulated} having={having} bytes={bytes} pair_bytes={pair_bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    fn load_join_profile(database: &mut Database, users: i64) {
        database
            .execute("CREATE TABLE users (id INT, name TEXT, bucket INT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        let mut start = 0_i64;
        while start < users {
            let end = (start + 499).min(users - 1);
            let mut sql = String::from("INSERT INTO users VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                sql.push_str(&format!("({id}, 'u{id}', {})", id % 10));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
        let orders = users.saturating_mul(4);
        let mut start = 0_i64;
        while start < orders {
            let end = (start + 499).min(orders - 1);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let user = if id % 3 == 0 { 0 } else { id % users };
                if id % 2 == 0 {
                    sql.push_str(&format!("({user}, NULL)"));
                } else {
                    sql.push_str(&format!("({user}, {id})"));
                }
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    fn join_group_bytes(groups: usize, key_cells: usize, aggs: usize) -> usize {
        let value = std::mem::size_of::<Value>();
        let vector = std::mem::size_of::<Vec<Value>>();
        let accumulator = std::mem::size_of::<AccValue>();
        let keys = groups.saturating_mul(vector.saturating_add(key_cells.saturating_mul(value)));
        let acc = groups.saturating_mul(vector.saturating_add(aggs.saturating_mul(accumulator)));
        let map_entry = std::mem::size_of::<u64>()
            + std::mem::size_of::<Vec<usize>>()
            + std::mem::size_of::<usize>();
        keys.saturating_add(acc)
            .saturating_add(groups.saturating_mul(map_entry))
    }

    #[test]
    #[ignore = "release profile of AVG, MIN, and MAX"]
    fn profile_milestone12() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone12 cpu={cpu} mem_bytes={mem} rustc={rustc} acc_bytes={} value_bytes={}",
            std::mem::size_of::<AccValue>(),
            std::mem::size_of::<Value>()
        );
        for rows in [1_000_i64, 10_000, 100_000] {
            let mut database = Database::new();
            load_avg_profile(&mut database, rows);
            profile_avg_shape(
                &mut database,
                rows,
                "sum",
                "SELECT SUM(salary) FROM staff;",
                true,
                1,
                1,
            );
            profile_avg_shape(
                &mut database,
                rows,
                "avg",
                "SELECT AVG(salary) FROM staff;",
                true,
                1,
                1,
            );
            profile_avg_shape(
                &mut database,
                rows,
                "sum_avg",
                "SELECT SUM(salary), AVG(salary) FROM staff;",
                true,
                1,
                2,
            );
            profile_avg_shape(
                &mut database,
                rows,
                "low_card",
                "SELECT bucket, AVG(salary), MIN(salary), MAX(salary), MIN(name) FROM staff GROUP BY bucket;",
                false,
                10,
                4,
            );
            profile_avg_shape(
                &mut database,
                rows,
                "high_card",
                "SELECT id, AVG(salary) FROM staff GROUP BY id;",
                false,
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
            );
            let threshold = rows / 2;
            profile_avg_shape(
                &mut database,
                rows,
                "having",
                &format!(
                    "SELECT bucket, AVG(salary) FROM staff GROUP BY bucket HAVING AVG(salary) > {threshold};"
                ),
                false,
                10,
                1,
            );
            profile_avg_shape(
                &mut database,
                rows,
                "join",
                "SELECT u.bucket, AVG(o.amount), MIN(o.amount), MAX(o.amount)
                 FROM users u JOIN orders o ON u.id = o.user_id
                 GROUP BY u.bucket;",
                false,
                10,
                3,
            );
        }
    }

    fn profile_avg_shape(
        database: &mut Database,
        rows: i64,
        shape: &str,
        sql: &str,
        scalar: bool,
        accumulated: usize,
        aggs: usize,
    ) {
        let mut samples = Vec::with_capacity(6);
        let mut returned = 0;
        let mut checksum = 0i64;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            checksum = 0;
            for row in &result.rows {
                for cell in row {
                    checksum = checksum.wrapping_add(profile_cell(cell));
                }
            }
            returned = result.rows.len();
            std::hint::black_box(&result.rows);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        let bytes = if scalar {
            std::mem::size_of::<Vec<AccValue>>()
                .saturating_add(aggs.saturating_mul(std::mem::size_of::<AccValue>()))
        } else {
            join_group_bytes(accumulated, 1, aggs)
        };
        println!(
            "milestone12 shape={shape} rows={rows} returned={returned} groups={accumulated} aggs={aggs} bytes={bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    fn profile_cell(value: &Value) -> i64 {
        match value {
            Value::Null => 0,
            Value::Integer(value) => *value,
            Value::Boolean(value) => i64::from(*value),
            Value::Text(text) => i64::try_from(text.len()).unwrap_or(i64::MAX),
            Value::Ratio {
                numerator,
                denominator,
            } => (*numerator as i64).wrapping_add(*denominator),
        }
    }

    fn load_avg_profile(database: &mut Database, rows: i64) {
        database
            .execute(
                "CREATE TABLE staff (id INT, name TEXT, bucket INT, salary INT, active BOOLEAN);",
            )
            .unwrap();
        database
            .execute("CREATE TABLE users (id INT, bucket INT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 499).min(rows - 1);
            let mut staff = String::from("INSERT INTO staff VALUES ");
            let mut users = String::from("INSERT INTO users VALUES ");
            for id in start..=end {
                if id != start {
                    staff.push(',');
                    users.push(',');
                }
                staff.push_str(&format!(
                    "({id}, 'n{id}', {}, {id}, {})",
                    id % 10,
                    id % 2 == 0
                ));
                users.push_str(&format!("({id}, {})", id % 10));
            }
            database.execute(&staff).unwrap();
            database.execute(&users).unwrap();
            start = end + 1;
        }
        let orders = rows.saturating_mul(4);
        let mut start = 0_i64;
        while start < orders {
            let end = (start + 499).min(orders - 1);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let user = id % rows;
                if id % 2 == 0 {
                    sql.push_str(&format!("({user}, NULL)"));
                } else {
                    sql.push_str(&format!("({user}, {id})"));
                }
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    #[test]
    fn a_failed_aggregate_does_not_admit_distinct_membership() {
        let label = Value::Text("a".to_string());
        let row = [label.clone(), Value::Integer(1)];
        let mut acc = [AccValue::Count(0), AccValue::Sum(Some(i64::MAX))];
        let mut seen = vec![Some(DistinctSet::new()), None];
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&row),
            &[AggFn::CountDistinct(0), AggFn::Sum(1)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Count(0)));
        assert!(!seen[0].as_ref().unwrap().contains(&label));

        let mut acc = [AccValue::Sum(Some(i64::MAX)), AccValue::Count(0)];
        let mut seen = vec![None, Some(DistinctSet::new())];
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&row),
            &[AggFn::Sum(1), AggFn::CountDistinct(0)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[1], AccValue::Count(0)));
        assert!(!seen[1].as_ref().unwrap().contains(&label));

        let mut acc = [AccValue::Sum(Some(i64::MAX))];
        let mut seen = vec![Some(DistinctSet::new())];
        let amount = [Value::Integer(1)];
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&amount),
            &[AggFn::SumDistinct(0)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Sum(Some(value)) if value == i64::MAX));
        assert!(!seen[0].as_ref().unwrap().contains(&amount[0]));
        let again = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&amount),
            &[AggFn::SumDistinct(0)],
        )
        .unwrap_err();
        assert_eq!(again.to_string(), "SUM overflows INTEGER");

        let mut acc = [AccValue::Avg {
            sum: i128::MAX,
            count: 1,
        }];
        let mut seen = vec![Some(DistinctSet::new())];
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&amount),
            &[AggFn::AvgDistinct(0)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows");
        assert!(matches!(acc[0], AccValue::Avg { sum, count } if sum == i128::MAX && count == 1));
        assert!(!seen[0].as_ref().unwrap().contains(&amount[0]));

        let mut acc = [AccValue::Count(0), AccValue::Sum(Some(0))];
        let mut seen = vec![Some(DistinctSet::new()), None];
        let aggs = [AggFn::CountDistinct(0), AggFn::Sum(1)];
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&[Value::Text("a".to_string()), Value::Integer(1)]),
            &aggs,
        )
        .unwrap();
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&[Value::Text("a".to_string()), Value::Integer(2)]),
            &aggs,
        )
        .unwrap();
        assert!(matches!(acc[0], AccValue::Count(1)));
        assert!(matches!(acc[1], AccValue::Sum(Some(3))));
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&[Value::Text("b".to_string()), Value::Integer(i64::MAX)]),
            &aggs,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Count(1)));
        assert!(
            !seen[0]
                .as_ref()
                .unwrap()
                .contains(&Value::Text("b".to_string()))
        );
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&[Value::Text("b".to_string()), Value::Integer(4)]),
            &aggs,
        )
        .unwrap();
        assert!(matches!(acc[0], AccValue::Count(2)));
        assert!(matches!(acc[1], AccValue::Sum(Some(7))));
        assert_eq!(seen[0].as_ref().unwrap().len(), 2);
    }

    #[test]
    fn a_failed_reservation_does_not_admit_any_slot() {
        let row = [
            Value::Text("a".to_string()),
            Value::Text("b".to_string()),
            Value::Integer(4),
        ];
        let aggs = [
            AggFn::CountDistinct(0),
            AggFn::CountDistinct(1),
            AggFn::AvgDistinct(2),
        ];
        let mut acc = [
            AccValue::Count(0),
            AccValue::Count(i64::MAX),
            AccValue::Avg { sum: 5, count: 1 },
        ];
        let mut seen = vec![
            Some(DistinctSet::new()),
            Some(DistinctSet::new()),
            Some(DistinctSet::new()),
        ];
        let error = update_acc(&mut acc, &mut seen, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "COUNT overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Count(0)));
        assert!(matches!(acc[1], AccValue::Count(i64::MAX)));
        assert!(matches!(acc[2], AccValue::Avg { sum: 5, count: 1 }));
        assert!(seen.iter().all(|set| set.as_ref().unwrap().len() == 0));

        let mut acc = [AccValue::Avg {
            sum: 5,
            count: i64::MAX,
        }];
        let mut seen = vec![Some(DistinctSet::new())];
        let amount = [Value::Integer(3)];
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::One(&amount),
            &[AggFn::AvgDistinct(0)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows INTEGER");
        assert!(matches!(
            acc[0],
            AccValue::Avg { sum, count } if sum == 5 && count == i64::MAX
        ));
        assert_eq!(seen[0].as_ref().unwrap().len(), 0);

        let mut acc = [AccValue::Count(0), AccValue::Count(0)];
        let mut seen = vec![Some(DistinctSet::new()), Some(DistinctSet::new())];
        let texts = [AggFn::CountDistinct(0), AggFn::CountDistinct(1)];
        {
            let _budget = DistinctReserveBudget::limit(2);
            let error = update_acc(&mut acc, &mut seen, InputRow::One(&row), &texts).unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: DISTINCT set is too large"
            );
        }
        assert!(matches!(acc[0], AccValue::Count(0)));
        assert!(matches!(acc[1], AccValue::Count(0)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 0);
        assert_eq!(seen[1].as_ref().unwrap().len(), 0);
        update_acc(&mut acc, &mut seen, InputRow::One(&row), &texts).unwrap();
        assert!(matches!(acc[0], AccValue::Count(1)));
        assert!(matches!(acc[1], AccValue::Count(1)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 1);
        assert_eq!(seen[1].as_ref().unwrap().len(), 1);
        {
            let _budget = DistinctReserveBudget::limit(0);
            update_acc(&mut acc, &mut seen, InputRow::One(&row), &texts).unwrap();
            let error = update_acc(
                &mut acc,
                &mut seen,
                InputRow::One(&[Value::Text("c".to_string()), Value::Text("d".to_string())]),
                &texts,
            )
            .unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: DISTINCT set is too large"
            );
        }
        assert!(matches!(acc[0], AccValue::Count(1)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 1);
        assert!(
            !seen[0]
                .as_ref()
                .unwrap()
                .contains(&Value::Text("c".to_string()))
        );

        let mut acc = [AccValue::Count(0)];
        let mut seen = vec![Some(DistinctSet::new())];
        {
            let _budget = DistinctReserveBudget::limit(0);
            update_acc(
                &mut acc,
                &mut seen,
                InputRow::One(&[Value::Null]),
                &[AggFn::CountDistinct(0)],
            )
            .unwrap();
        }
        assert!(matches!(acc[0], AccValue::Count(0)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 0);
    }

    #[test]
    fn commit_does_not_reserve_after_the_first_mutation() {
        let row = [
            Value::Text("left".to_string()),
            Value::Text("right".to_string()),
        ];
        let aggs = [AggFn::CountDistinct(0), AggFn::CountDistinct(1)];
        let mut acc = [AccValue::Count(0), AccValue::Count(0)];
        let mut seen = vec![Some(DistinctSet::new()), Some(DistinctSet::new())];
        {
            // Each new key charges the map and its bucket during preparation.
            // A charge inside commit would fail this exact budget after the
            // first slot had already been published.
            let _budget = DistinctReserveBudget::limit(4);
            update_acc(&mut acc, &mut seen, InputRow::One(&row), &aggs).unwrap();
        }
        assert!(matches!(acc[0], AccValue::Count(1)));
        assert!(matches!(acc[1], AccValue::Count(1)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 1);
        assert_eq!(seen[1].as_ref().unwrap().len(), 1);
        assert!(
            seen[0]
                .as_ref()
                .unwrap()
                .contains(&Value::Text("left".to_string()))
        );
        assert!(
            seen[1]
                .as_ref()
                .unwrap()
                .contains(&Value::Text("right".to_string()))
        );

        let mut acc = [AccValue::Count(0), AccValue::Count(0)];
        let mut seen = vec![Some(DistinctSet::new()), Some(DistinctSet::new())];
        {
            let _budget = DistinctReserveBudget::limit(3);
            let error = update_acc(&mut acc, &mut seen, InputRow::One(&row), &aggs).unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: DISTINCT set is too large"
            );
        }
        assert!(matches!(acc[0], AccValue::Count(0)));
        assert!(matches!(acc[1], AccValue::Count(0)));
        assert_eq!(seen[0].as_ref().unwrap().len(), 0);
        assert_eq!(seen[1].as_ref().unwrap().len(), 0);
    }

    #[test]
    fn colliding_distinct_values_are_stored_once_each() {
        let _force = ForceHashCollision::new();
        let mut acc = [AccValue::Count(0)];
        let mut seen = vec![Some(DistinctSet::new())];
        let aggs = [AggFn::CountDistinct(0)];
        for label in ["a", "a", "b"] {
            update_acc(
                &mut acc,
                &mut seen,
                InputRow::One(&[Value::Text(label.to_string())]),
                &aggs,
            )
            .unwrap();
        }
        let set = seen[0].as_ref().unwrap();
        assert_eq!(set.buckets.len(), 1);
        assert_eq!(set.len(), 2);
        assert!(matches!(acc[0], AccValue::Count(2)));
    }

    #[test]
    fn distinct_groups_keep_first_seen_order_when_hashes_collide() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE items (dept TEXT, amount INT);")
            .unwrap();
        database
            .execute(
                "INSERT INTO items VALUES ('b', 1), ('a', 1), ('b', 2), ('c', NULL), ('a', 1);",
            )
            .unwrap();
        let _force = ForceHashCollision::new();
        assert_eq!(
            query(
                &mut database,
                "SELECT dept, COUNT(DISTINCT amount), SUM(DISTINCT amount)
                 FROM items GROUP BY dept;",
            )
            .rows,
            vec![
                vec![
                    Value::Text("b".to_string()),
                    Value::Integer(2),
                    Value::Integer(3),
                ],
                vec![
                    Value::Text("a".to_string()),
                    Value::Integer(1),
                    Value::Integer(1),
                ],
                vec![Value::Text("c".to_string()), Value::Integer(0), Value::Null,],
            ]
        );
    }

    #[test]
    fn distinct_values_stay_apart_when_every_hash_collides() {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE items (label TEXT, amount INT, flag BOOLEAN);")
            .unwrap();
        database
            .execute(
                "INSERT INTO items VALUES
                    ('a', 1, TRUE),
                    ('b', 2, FALSE),
                    ('a', 1, TRUE),
                    ('b', 2, TRUE),
                    (NULL, NULL, NULL);",
            )
            .unwrap();
        let sql = "SELECT COUNT(DISTINCT label), COUNT(DISTINCT amount), COUNT(DISTINCT flag),
            SUM(DISTINCT amount), AVG(DISTINCT amount) FROM items;";
        let _force = ForceHashCollision::new();
        assert_eq!(
            query(&mut database, sql).rows,
            vec![vec![
                Value::Integer(2),
                Value::Integer(2),
                Value::Integer(2),
                Value::Integer(3),
                Value::ratio(3, 2),
            ]]
        );
    }

    #[test]
    fn distinct_join_agrees_across_algorithms_and_materialization() {
        let sql = "SELECT u.name, COUNT(*), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount)
            FROM users u JOIN orders o ON u.id = o.user_id
            GROUP BY u.name
            ORDER BY u.name ASC;";
        let mut memory = Database::new();
        load_algorithm_shop(&mut memory, 4, 8);
        let hash_rows = query(&mut memory, sql).rows;
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            query(&mut memory, sql).rows
        };
        let materialized = {
            let _guard = MaterializeAggregate::new();
            query(&mut memory, sql).rows
        };
        assert_eq!(nested_rows, hash_rows);
        assert_eq!(materialized, hash_rows);
        assert!(hash_rows.len() > 1);

        let dir = crate::test_dir::TestDir::new("distinct-join-algorithms");
        let path = dir.join("db");
        let mut database = Database::open(&path).unwrap();
        load_algorithm_shop(&mut database, 4, 8);
        let hash_rows = query(&mut database, sql).rows;
        drop(database);
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders(user_id);")
            .unwrap();
        let indexed = {
            let _force = ForceIndexJoin::new();
            query(&mut database, sql).rows
        };
        let nested_rows = {
            let _force = ForceNestedJoin::new();
            let _guard = MaterializeAggregate::new();
            query(&mut database, sql).rows
        };
        assert_eq!(indexed, hash_rows);
        assert_eq!(nested_rows, hash_rows);
        let explained = {
            let _force = ForceIndexJoin::new();
            query(&mut database, &format!("EXPLAIN {sql}"))
        };
        assert_eq!(
            explained.rows[0][4],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        assert_eq!(
            explained.rows[0][2],
            Value::Text(
                "COUNT(*), COUNT(DISTINCT o.amount), SUM(o.amount), SUM(DISTINCT o.amount)"
                    .to_string()
            )
        );
    }

    #[test]
    fn select_distinct_checks_values_after_a_forced_collision() {
        let mut db = Database::new();
        db.execute("CREATE TABLE items (n INT, label TEXT, active BOOLEAN);")
            .unwrap();
        db.execute(
            "INSERT INTO items VALUES
                (1, 'A', TRUE),
                (1, 'A', TRUE),
                (1, 'a', TRUE),
                (NULL, 'A', TRUE),
                (1, 'A', FALSE),
                (1, NULL, NULL);",
        )
        .unwrap();
        let _force = ForceHashCollision::new();
        let result = query(&mut db, "SELECT DISTINCT n, label, active FROM items;");
        assert_eq!(
            result.rows,
            vec![
                vec![
                    Value::Integer(1),
                    Value::Text("A".to_string()),
                    Value::Boolean(true)
                ],
                vec![
                    Value::Integer(1),
                    Value::Text("a".to_string()),
                    Value::Boolean(true)
                ],
                vec![
                    Value::Null,
                    Value::Text("A".to_string()),
                    Value::Boolean(true)
                ],
                vec![
                    Value::Integer(1),
                    Value::Text("A".to_string()),
                    Value::Boolean(false)
                ],
                vec![Value::Integer(1), Value::Null, Value::Null],
            ]
        );
    }

    #[test]
    fn distinct_join_strategies_keep_the_same_first_seen_rows() {
        let dir = crate::test_dir::TestDir::new("distinct-join-strategies");
        let path = dir.join("db");
        let sql = "SELECT DISTINCT u.name, o.amount
            FROM users u
            JOIN orders o ON u.id = o.user_id;";
        let expected = vec![
            vec![Value::Text("Ada".to_string()), Value::Integer(10)],
            vec![Value::Text("Ada".to_string()), Value::Null],
            vec![Value::Null, Value::Integer(10)],
        ];
        {
            let mut db = Database::open(&path).unwrap();
            load_distinct_join(&mut db);
            let explained = query(&mut db, &format!("EXPLAIN {sql}"));
            assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
            assert_eq!(explained.rows[0][3], Value::Text("u (users)".to_string()));
            assert_eq!(query(&mut db, sql).rows, expected);
            let _nested = ForceNestedJoin::new();
            let nested_plan = query(&mut db, &format!("EXPLAIN {sql}"));
            assert_eq!(
                nested_plan.rows[0][0],
                Value::Text("NestedLoopJoin".to_string())
            );
            assert_eq!(query(&mut db, sql).rows, expected);
        }
        Database::migrate(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        let _index = ForceIndexJoin::new();
        let explained = query(&mut db, &format!("EXPLAIN {sql}"));
        assert_eq!(
            explained.rows[0][0],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        assert_eq!(
            explained.rows[0][4],
            Value::Text("idx_orders_user".to_string())
        );
        assert_eq!(query(&mut db, sql).rows, expected);

        let wide = "SELECT DISTINCT u.name, o.note
            FROM users u JOIN orders o ON o.user_id = u.id
            WHERE o.amount > 0;";
        let _force = ForceHashCollision::new();
        let collided = query(&mut db, wide);
        let mut linear = Vec::new();
        for row in &query_without_distinct(&mut db, wide.replace("DISTINCT ", "")) {
            if !linear.iter().any(|kept| kept == row) {
                linear.push(row.clone());
            }
        }
        assert_eq!(collided.rows, linear);
    }

    #[test]
    fn a_distinct_join_limit_does_not_hide_a_reservation_failure() {
        let mut db = Database::new();
        load_distinct_join(&mut db);
        let _budget = DistinctReserveBudget::limit(3);
        let error = db
            .execute(
                "SELECT DISTINCT u.name FROM users u
                 JOIN orders o ON u.id = o.user_id
                 LIMIT 1;",
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "internal error: SELECT DISTINCT result is too large"
        );
        drop(_budget);
        let rows = query(
            &mut db,
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(
            rows.rows,
            vec![vec![Value::Text("Ada".to_string())], vec![Value::Null]]
        );
    }

    fn load_distinct_join(db: &mut Database) {
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT, note TEXT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada'), (3, NULL);")
            .unwrap();
        db.execute(
            "INSERT INTO orders VALUES
                (1, 10, 'alpha'),
                (1, 10, 'alpha'),
                (2, 10, 'beta'),
                (2, NULL, 'gamma'),
                (NULL, 10, 'none'),
                (3, 10, 'delta');",
        )
        .unwrap();
    }

    fn query_without_distinct(db: &mut Database, sql: String) -> Vec<Vec<Value>> {
        query(db, &sql).rows
    }

    #[test]
    fn the_first_distinct_tuple_comes_from_the_earlier_outer_row() {
        let expected = vec![
            vec![Value::Text("A".to_string()), Value::Integer(30)],
            vec![Value::Text("A".to_string()), Value::Integer(10)],
        ];
        let sql = "SELECT DISTINCT u.name, o.amount
            FROM users u JOIN orders o ON u.id = o.user_id;";
        let reversed = "SELECT DISTINCT u.name, o.amount
            FROM users u JOIN orders o ON o.user_id = u.id;";
        for (users, build) in [("build-outer", "u (users)"), ("build-inner", "o (orders)")] {
            let mut db = Database::new();
            load_later_inner(&mut db, users == "build-inner");
            let explained = query(&mut db, &format!("EXPLAIN {sql}"));
            assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
            assert_eq!(
                explained.rows[0][3],
                Value::Text(build.to_string()),
                "{users}"
            );
            assert_eq!(query(&mut db, sql).rows, expected, "{users}");
            assert_eq!(query(&mut db, reversed).rows, expected, "{users}");
            let _nested = ForceNestedJoin::new();
            assert_eq!(
                query(&mut db, &format!("EXPLAIN {sql}")).rows[0][0],
                Value::Text("NestedLoopJoin".to_string())
            );
            assert_eq!(query(&mut db, sql).rows, expected, "{users}");
        }
        let dir = crate::test_dir::TestDir::new("distinct-first-seen");
        let path = dir.join("db");
        {
            let mut db = Database::open(&path).unwrap();
            load_later_inner(&mut db, false);
        }
        Database::migrate(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        let _index = ForceIndexJoin::new();
        let explained = query(&mut db, &format!("EXPLAIN {reversed}"));
        assert_eq!(
            explained.rows[0][0],
            Value::Text("IndexNestedLoopJoin".to_string())
        );
        assert_eq!(
            explained.rows[0][4],
            Value::Text("idx_orders_user".to_string())
        );
        assert_eq!(query(&mut db, sql).rows, expected);
        assert_eq!(query(&mut db, reversed).rows, expected);
    }

    #[test]
    fn distinct_joins_match_an_independent_nested_loop() {
        let sql = "SELECT DISTINCT u.name, o.amount, o.paid
            FROM users u
            JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
            WHERE o.amount > 0;";
        let reversed = "SELECT DISTINCT u.name, o.amount, o.paid
            FROM users u
            JOIN orders o ON o.user_id = u.id AND o.paid = TRUE
            WHERE o.amount > 0;";
        let ordered = "SELECT DISTINCT u.name, o.amount, o.paid
            FROM users u
            JOIN orders o ON u.id = o.user_id AND o.paid = TRUE
            WHERE o.amount > 0
            ORDER BY name ASC, amount DESC NULLS FIRST
            LIMIT 3;";
        let nested_sql = "SELECT DISTINCT u.name, o.amount
            FROM users u JOIN orders o ON u.id < o.user_id;";
        for seed in [0x16a1_u64, 0x16b2, 0x16c3, 0x16d4] {
            for (users_n, orders_n) in [(0, 0), (0, 6), (6, 0), (4, 15), (15, 4), (8, 8)] {
                let (users, orders) = sample_join(seed, users_n, orders_n);
                let expected = independent_join(&users, &orders, true);
                let ordered_rows = order_independent(expected.clone());
                let lower = independent_join(&users, &orders, false);
                let label = format!("seed {seed:#x} users {users_n} orders {orders_n}");
                let mut db = Database::new();
                load_sample(&mut db, &users, &orders);
                assert_join(&mut db, sql, "HashJoin", &expected, &label);
                assert_join(&mut db, reversed, "HashJoin", &expected, &label);
                assert_join(&mut db, ordered, "HashJoin", &ordered_rows, &label);
                let build = if orders_n <= users_n {
                    "o (orders)"
                } else {
                    "u (users)"
                };
                assert_eq!(
                    query(&mut db, &format!("EXPLAIN {sql}")).rows[0][3],
                    Value::Text(build.to_string()),
                    "{label}"
                );
                assert_join(&mut db, nested_sql, "NestedLoopJoin", &lower, &label);
                {
                    let _nested = ForceNestedJoin::new();
                    assert_join(&mut db, sql, "NestedLoopJoin", &expected, &label);
                    assert_join(&mut db, reversed, "NestedLoopJoin", &expected, &label);
                }
                let dir = crate::test_dir::TestDir::new("distinct-ref");
                let path = dir.join("db");
                {
                    let mut file = Database::open(&path).unwrap();
                    load_sample(&mut file, &users, &orders);
                }
                Database::migrate(&path).unwrap();
                let mut file = Database::open(&path).unwrap();
                file.execute("CREATE INDEX idx_orders_user ON orders (user_id);")
                    .unwrap();
                let _index = ForceIndexJoin::new();
                assert_join(&mut file, sql, "IndexNestedLoopJoin", &expected, &label);
                assert_join(
                    &mut file,
                    reversed,
                    "IndexNestedLoopJoin",
                    &expected,
                    &label,
                );
                assert_eq!(
                    query(&mut file, &format!("EXPLAIN {sql}")).rows[0][4],
                    Value::Text("idx_orders_user".to_string()),
                    "{label}"
                );
            }
        }
    }

    #[test]
    fn a_distinct_join_collision_does_not_merge_unequal_tuples() {
        let mut db = Database::new();
        load_charge_join(&mut db);
        let expected = independent_charge();
        let sql = "SELECT DISTINCT u.name, o.amount
            FROM users u JOIN orders o ON u.id = o.user_id;";
        let _force = ForceHashCollision::new();
        assert_eq!(query(&mut db, sql).rows, expected);
        let _nested = ForceNestedJoin::new();
        assert_eq!(query(&mut db, sql).rows, expected);
    }

    #[test]
    fn duplicate_join_tuples_do_not_reserve_and_limit_does_not_hide_failure() {
        let mut db = Database::new();
        load_charge_join(&mut db);
        {
            let _budget = DistinctReserveBudget::limit(3);
            assert_eq!(
                query(
                    &mut db,
                    "SELECT DISTINCT u.name FROM users u
                     JOIN orders o ON u.id = o.user_id
                     WHERE u.name = 'Ada';",
                )
                .rows,
                vec![vec![Value::Text("Ada".to_string())]]
            );
        }
        {
            let _budget = DistinctReserveBudget::limit(3);
            let error = db
                .execute(
                    "SELECT DISTINCT u.name, o.amount
                     FROM users u JOIN orders o ON u.id = o.user_id
                     LIMIT 0;",
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: SELECT DISTINCT result is too large"
            );
        }
        assert_eq!(
            query(&mut db, "SELECT id FROM users;").rows,
            vec![
                vec![Value::Integer(1)],
                vec![Value::Integer(1)],
                vec![Value::Integer(2)]
            ]
        );
        assert_eq!(
            query(
                &mut db,
                "SELECT DISTINCT u.name, o.amount
                 FROM users u JOIN orders o ON u.id = o.user_id;",
            )
            .rows,
            independent_charge()
        );
    }

    #[test]
    fn explain_and_a_failed_distinct_join_do_not_write() {
        let dir = crate::test_dir::TestDir::new("distinct-no-write");
        let path = dir.join("db");
        {
            let mut db = Database::open(&path).unwrap();
            load_charge_join(&mut db);
        }
        let before = std::fs::read(&path).unwrap();
        let wal = wal_len(&path);
        {
            let mut db = Database::open(&path).unwrap();
            db.execute(
                "EXPLAIN SELECT DISTINCT u.name, o.amount
                 FROM users u JOIN orders o ON u.id = o.user_id
                 ORDER BY name ASC NULLS LAST LIMIT 1;",
            )
            .unwrap();
            let _budget = DistinctReserveBudget::limit(3);
            let error = db
                .execute(
                    "SELECT DISTINCT u.name, o.amount
                     FROM users u JOIN orders o ON u.id = o.user_id
                     LIMIT 1;",
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: SELECT DISTINCT result is too large"
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(wal_len(&path), wal);
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(
                query(
                    &mut db,
                    "SELECT DISTINCT u.name, o.amount
                     FROM users u JOIN orders o ON u.id = o.user_id;",
                )
                .rows,
                independent_charge()
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(wal_len(&path), wal);
    }

    fn wal_len(path: &std::path::Path) -> u64 {
        let mut wal = path.as_os_str().to_owned();
        wal.push(".wal");
        std::fs::metadata(wal).map(|meta| meta.len()).unwrap_or(0)
    }

    fn load_later_inner(db: &mut Database, extra_users: bool) {
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'A'), (2, 'A');")
            .unwrap();
        if extra_users {
            db.execute("INSERT INTO users VALUES (3, 'Z'), (4, 'Y');")
                .unwrap();
        }
        db.execute("INSERT INTO orders VALUES (2, 10), (1, 30), (1, 10);")
            .unwrap();
    }

    fn load_charge_join(db: &mut Database) {
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (1, 'Ada'), (2, 'Bea');")
            .unwrap();
        db.execute("INSERT INTO orders VALUES (1, 10), (1, 10), (2, 10), (2, 20);")
            .unwrap();
    }

    fn independent_charge() -> Vec<Vec<Value>> {
        vec![
            vec![Value::Text("Ada".to_string()), Value::Integer(10)],
            vec![Value::Text("Bea".to_string()), Value::Integer(10)],
            vec![Value::Text("Bea".to_string()), Value::Integer(20)],
        ]
    }

    fn assert_join(
        db: &mut Database,
        sql: &str,
        access: &str,
        expected: &[Vec<Value>],
        label: &str,
    ) {
        let explained = query(db, &format!("EXPLAIN {sql}"));
        assert_eq!(
            explained.rows[0][0],
            Value::Text(access.to_string()),
            "{label} {sql}"
        );
        assert_eq!(
            explained.rows[0][8],
            Value::Text("HashDistinct".to_string()),
            "{label}"
        );
        assert_eq!(query(db, sql).rows, expected, "{label} {sql}");
    }

    struct RefUser {
        id: Option<i64>,
        name: Option<String>,
        active: Option<bool>,
    }

    struct RefOrder {
        user_id: Option<i64>,
        amount: Option<i64>,
        paid: Option<bool>,
    }

    fn sample_join(seed: u64, users_n: usize, orders_n: usize) -> (Vec<RefUser>, Vec<RefOrder>) {
        let mut state = seed | 1;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            state
        };
        let mut users = Vec::new();
        for index in 0..users_n {
            let draw = next();
            let id = if draw % 5 == 0 {
                None
            } else if draw % 2 == 0 {
                Some(i64::try_from(index % 3).unwrap_or(0))
            } else {
                Some(i64::try_from(index).unwrap_or(0))
            };
            let name = match draw % 7 {
                0 => None,
                1 => Some("Ada".to_string()),
                2 => Some(format!("Ada{}", "x".repeat(40))),
                _ => Some(format!("n{index}")),
            };
            let active = match draw % 3 {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            };
            users.push(RefUser { id, name, active });
        }
        let mut orders = Vec::new();
        for index in 0..orders_n {
            let draw = next();
            let user_id = if draw % 6 == 0 {
                None
            } else {
                Some(i64::try_from(index % 3).unwrap_or(0))
            };
            let amount = if draw % 4 == 0 {
                None
            } else {
                Some(i64::try_from(index % 6).unwrap_or(0))
            };
            let paid = match draw % 3 {
                0 => None,
                1 => Some(true),
                _ => Some(false),
            };
            orders.push(RefOrder {
                user_id,
                amount,
                paid,
            });
        }
        (users, orders)
    }

    fn independent_join(users: &[RefUser], orders: &[RefOrder], equality: bool) -> Vec<Vec<Value>> {
        let mut kept = Vec::new();
        for user in users {
            for order in orders {
                let keep = if equality {
                    user.id.is_some()
                        && user.id == order.user_id
                        && order.paid == Some(true)
                        && order.amount.is_some_and(|amount| amount > 0)
                } else {
                    matches!((user.id, order.user_id), (Some(left), Some(right)) if left < right)
                };
                if !keep {
                    continue;
                }
                let row = if equality {
                    vec![
                        ref_text(&user.name),
                        ref_int(order.amount),
                        ref_bool(order.paid),
                    ]
                } else {
                    vec![ref_text(&user.name), ref_int(order.amount)]
                };
                if !kept.iter().any(|seen| seen == &row) {
                    kept.push(row);
                }
            }
        }
        kept
    }

    fn order_independent(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        rows.sort_by(|left, right| {
            sql_order(&left[0], &right[0], true, false)
                .then(sql_order(&left[1], &right[1], false, true))
        });
        rows.truncate(3);
        rows
    }

    fn sql_order(left: &Value, right: &Value, ascending: bool, nulls_first: bool) -> Ordering {
        match (left, right) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Value::Null) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Value::Integer(left), Value::Integer(right)) => directed(left.cmp(right), ascending),
            (Value::Text(left), Value::Text(right)) => directed(left.cmp(right), ascending),
            (Value::Boolean(left), Value::Boolean(right)) => directed(left.cmp(right), ascending),
            _ => Ordering::Equal,
        }
    }

    fn directed(ordering: Ordering, ascending: bool) -> Ordering {
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    }

    fn ref_text(value: &Option<String>) -> Value {
        value.clone().map(Value::Text).unwrap_or(Value::Null)
    }

    fn ref_int(value: Option<i64>) -> Value {
        value.map(Value::Integer).unwrap_or(Value::Null)
    }

    fn ref_bool(value: Option<bool>) -> Value {
        value.map(Value::Boolean).unwrap_or(Value::Null)
    }

    fn load_sample(db: &mut Database, users: &[RefUser], orders: &[RefOrder]) {
        db.execute("CREATE TABLE users (id INT, name TEXT, active BOOLEAN);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT, paid BOOLEAN);")
            .unwrap();
        if !users.is_empty() {
            let mut sql = String::from("INSERT INTO users VALUES ");
            for (index, user) in users.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('(');
                push_sql_int(&mut sql, user.id);
                sql.push(',');
                push_sql_text(&mut sql, user.name.as_deref());
                sql.push(',');
                push_sql_bool(&mut sql, user.active);
                sql.push(')');
            }
            sql.push(';');
            db.execute(&sql).unwrap();
        }
        if !orders.is_empty() {
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for (index, order) in orders.iter().enumerate() {
                if index > 0 {
                    sql.push(',');
                }
                sql.push('(');
                push_sql_int(&mut sql, order.user_id);
                sql.push(',');
                push_sql_int(&mut sql, order.amount);
                sql.push(',');
                push_sql_bool(&mut sql, order.paid);
                sql.push(')');
            }
            sql.push(';');
            db.execute(&sql).unwrap();
        }
    }

    fn push_sql_int(sql: &mut String, value: Option<i64>) {
        match value {
            Some(value) => sql.push_str(&value.to_string()),
            None => sql.push_str("NULL"),
        }
    }

    fn push_sql_text(sql: &mut String, value: Option<&str>) {
        match value {
            Some(value) => {
                sql.push('\'');
                sql.push_str(value);
                sql.push('\'');
            }
            None => sql.push_str("NULL"),
        }
    }

    fn push_sql_bool(sql: &mut String, value: Option<bool>) {
        match value {
            Some(true) => sql.push_str("TRUE"),
            Some(false) => sql.push_str("FALSE"),
            None => sql.push_str("NULL"),
        }
    }

    #[test]
    fn select_distinct_limit_does_not_hide_a_reservation_failure() {
        let mut db = Database::new();
        db.execute("CREATE TABLE names (name TEXT);").unwrap();
        db.execute("INSERT INTO names VALUES ('a'), ('a'), ('b'), ('c');")
            .unwrap();
        let _budget = DistinctReserveBudget::limit(3);
        let error = db
            .execute("SELECT DISTINCT name FROM names LIMIT 1;")
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "internal error: SELECT DISTINCT result is too large"
        );
    }

    #[test]
    fn a_repeated_distinct_row_reserves_nothing() {
        let mut db = Database::new();
        db.execute("CREATE TABLE names (name TEXT);").unwrap();
        db.execute("INSERT INTO names VALUES ('a'), ('a');")
            .unwrap();
        let _budget = DistinctReserveBudget::limit(3);
        assert_eq!(
            query(&mut db, "SELECT DISTINCT name FROM names;").rows,
            vec![vec![Value::Text("a".to_string())]]
        );
    }

    #[test]
    fn a_colliding_distinct_row_reserves_the_bucket_and_the_row() {
        let mut db = Database::new();
        db.execute("CREATE TABLE nums (n INT);").unwrap();
        db.execute("INSERT INTO nums VALUES (1), (2);").unwrap();
        let _force = ForceHashCollision::new();
        {
            let _budget = DistinctReserveBudget::limit(5);
            assert_eq!(
                query(&mut db, "SELECT DISTINCT n FROM nums;").rows,
                vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
            );
        }
        {
            let _budget = DistinctReserveBudget::limit(4);
            let error = db.execute("SELECT DISTINCT n FROM nums;").unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: SELECT DISTINCT result is too large"
            );
        }
        assert_eq!(
            query(&mut db, "SELECT DISTINCT n FROM nums;").rows,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
        );
    }

    #[test]
    fn a_failed_distinct_admission_keeps_prior_rows_reachable() {
        let mut distinct = DistinctRows::new();
        distinct.admit(vec![Value::Integer(1)]).expect("first row");
        for limit in [0, 1, 2] {
            let _budget = DistinctReserveBudget::limit(limit);
            let error = distinct.admit(vec![Value::Integer(2)]).unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: SELECT DISTINCT result is too large"
            );
            assert_eq!(distinct.rows, vec![vec![Value::Integer(1)]]);
            distinct_rows_are_consistent(&distinct);
        }
        distinct.admit(vec![Value::Integer(2)]).unwrap();
        distinct.admit(vec![Value::Integer(1)]).unwrap();
        assert_eq!(
            distinct.rows,
            vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]
        );
        distinct_rows_are_consistent(&distinct);

        let mut distinct = DistinctRows::new();
        let _force = ForceHashCollision::new();
        distinct.admit(vec![Value::Integer(1)]).unwrap();
        for limit in [0, 1] {
            let _budget = DistinctReserveBudget::limit(limit);
            let error = distinct.admit(vec![Value::Integer(9)]).unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: SELECT DISTINCT result is too large"
            );
            assert_eq!(distinct.rows, vec![vec![Value::Integer(1)]]);
            distinct_rows_are_consistent(&distinct);
        }
        distinct.admit(vec![Value::Integer(9)]).unwrap();
        distinct.admit(vec![Value::Integer(1)]).unwrap();
        assert_eq!(
            distinct.rows,
            vec![vec![Value::Integer(1)], vec![Value::Integer(9)]]
        );
        distinct_rows_are_consistent(&distinct);
    }

    #[test]
    fn colliding_wide_rows_match_a_linear_scan() {
        let mut db = Database::new();
        db.execute("CREATE TABLE items (n INT, label TEXT, active BOOLEAN);")
            .unwrap();
        let alpha = "a".repeat(80);
        let longer = format!("{alpha}B");
        let upper = "A".repeat(80);
        db.execute(&format!(
            "INSERT INTO items VALUES
                (1, '{alpha}', TRUE),
                (1, '{alpha}', TRUE),
                (1, '{longer}', TRUE),
                (NULL, '{alpha}', TRUE),
                (1, '{alpha}', NULL),
                (NULL, NULL, NULL),
                (NULL, NULL, NULL),
                (1, '{upper}', TRUE),
                (NULL, '{alpha}', NULL);"
        ))
        .unwrap();
        let input = vec![
            vec![
                Value::Integer(1),
                Value::Text(alpha.clone()),
                Value::Boolean(true),
            ],
            vec![
                Value::Integer(1),
                Value::Text(alpha.clone()),
                Value::Boolean(true),
            ],
            vec![
                Value::Integer(1),
                Value::Text(longer.clone()),
                Value::Boolean(true),
            ],
            vec![
                Value::Null,
                Value::Text(alpha.clone()),
                Value::Boolean(true),
            ],
            vec![Value::Integer(1), Value::Text(alpha.clone()), Value::Null],
            vec![Value::Null, Value::Null, Value::Null],
            vec![Value::Null, Value::Null, Value::Null],
            vec![Value::Integer(1), Value::Text(upper), Value::Boolean(true)],
            vec![Value::Null, Value::Text(alpha), Value::Null],
        ];
        let expected = linear_distinct(&input);
        let _force = ForceHashCollision::new();
        assert_eq!(
            query(&mut db, "SELECT DISTINCT n, label, active FROM items;").rows,
            expected
        );
        let filtered = linear_distinct(
            &input
                .iter()
                .filter(|row| matches!(row[0], Value::Integer(1)))
                .cloned()
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            query(
                &mut db,
                "SELECT DISTINCT n, label, active FROM items WHERE n = 1;"
            )
            .rows,
            filtered
        );
        let mut ordered = expected.clone();
        ordered.sort_by(|left, right| match (&left[1], &right[1]) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Null, _) => Ordering::Greater,
            (_, Value::Null) => Ordering::Less,
            (Value::Text(left), Value::Text(right)) => left.cmp(right),
            _ => Ordering::Equal,
        });
        ordered.truncate(2);
        assert_eq!(
            query(
                &mut db,
                "SELECT DISTINCT n, label, active FROM items ORDER BY label ASC LIMIT 2;"
            )
            .rows,
            ordered
        );
    }

    fn distinct_rows_are_consistent(distinct: &DistinctRows) {
        let mut seen = vec![false; distinct.rows.len()];
        for bucket in distinct.buckets.values() {
            assert!(!bucket.is_empty());
            for &index in bucket {
                assert!(index < distinct.rows.len());
                assert!(!seen[index]);
                seen[index] = true;
            }
        }
        assert!(seen.iter().all(|kept| *kept));
    }

    fn linear_distinct(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
        let mut kept: Vec<Vec<Value>> = Vec::new();
        for row in rows {
            if !kept.iter().any(|seen| tuple_same(seen, row)) {
                kept.push(row.clone());
            }
        }
        kept
    }

    fn tuple_same(left: &[Value], right: &[Value]) -> bool {
        left.len() == right.len()
            && left
                .iter()
                .zip(right)
                .all(|(left, right)| match (left, right) {
                    (Value::Null, Value::Null) => true,
                    (Value::Integer(left), Value::Integer(right)) => left == right,
                    (Value::Text(left), Value::Text(right)) => left == right,
                    (Value::Boolean(left), Value::Boolean(right)) => left == right,
                    _ => false,
                })
    }

    #[test]
    #[ignore = "release profile of DISTINCT aggregates and HAVING aliases"]
    fn profile_milestone13() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone13 cpu={cpu} mem_bytes={mem} rustc={rustc} acc_bytes={} value_bytes={} map_bytes={} option_bytes={} seen_vec_bytes={}",
            std::mem::size_of::<AccValue>(),
            std::mem::size_of::<Value>(),
            std::mem::size_of::<DistinctSet>(),
            std::mem::size_of::<Option<DistinctSet>>(),
            std::mem::size_of::<Vec<Option<DistinctSet>>>()
        );
        for rows in [1_000_i64, 10_000, 100_000] {
            let mut database = Database::new();
            let text_bytes = load_distinct_profile(&mut database, rows);
            let amount_sum = expected_amount_sum(rows);
            let join_values = join_distinct_values(rows);
            profile_distinct_shape(
                &mut database,
                rows,
                "count",
                "SELECT COUNT(*) FROM items;",
                true,
                1,
                0,
                0,
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(rows)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "count_distinct_low",
                "SELECT COUNT(DISTINCT bucket) FROM items;",
                true,
                1,
                1,
                10,
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(10)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "count_distinct_high",
                "SELECT COUNT(DISTINCT id) FROM items;",
                true,
                1,
                1,
                usize::try_from(rows).unwrap_or(usize::MAX),
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(rows)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "text_distinct_low",
                "SELECT COUNT(DISTINCT label) FROM items;",
                true,
                1,
                1,
                8,
                8,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(8)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "text_distinct_high",
                "SELECT COUNT(DISTINCT tag) FROM items;",
                true,
                1,
                1,
                usize::try_from(rows).unwrap_or(usize::MAX),
                text_bytes,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(rows)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "sum",
                "SELECT SUM(amount) FROM items;",
                true,
                1,
                0,
                0,
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(amount_sum)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "sum_distinct",
                "SELECT SUM(DISTINCT amount) FROM items;",
                true,
                1,
                1,
                17,
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::Integer(136)]]);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "avg",
                "SELECT AVG(amount) FROM items;",
                true,
                1,
                0,
                0,
                0,
                |_| {},
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "avg_distinct",
                "SELECT AVG(DISTINCT amount) FROM items;",
                true,
                1,
                1,
                17,
                0,
                |result| {
                    assert_eq!(result.rows, vec![vec![Value::ratio(136, 17)]]);
                },
            );
            let threshold = rows / 20;
            profile_distinct_shape(
                &mut database,
                rows,
                "having_alias",
                &format!(
                    "SELECT bucket AS dept, COUNT(DISTINCT id) AS total
                     FROM items GROUP BY bucket HAVING total > {threshold};"
                ),
                false,
                10,
                10,
                usize::try_from(rows).unwrap_or(usize::MAX),
                0,
                |result| {
                    assert_eq!(result.rows.len(), 10);
                },
            );
            profile_distinct_shape(
                &mut database,
                rows,
                "join_distinct",
                "SELECT u.bucket, COUNT(DISTINCT o.amount)
                 FROM users u JOIN orders o ON u.id = o.user_id
                 GROUP BY u.bucket;",
                false,
                10,
                10,
                join_values,
                0,
                |result| {
                    assert_eq!(result.rows.len(), 10);
                    let membership: i64 = result
                        .rows
                        .iter()
                        .map(|row| match row[1] {
                            Value::Integer(count) => count,
                            _ => panic!("expected a count"),
                        })
                        .sum();
                    assert_eq!(membership, i64::try_from(join_values).unwrap_or(i64::MAX));
                },
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn profile_distinct_shape(
        database: &mut Database,
        rows: i64,
        shape: &str,
        sql: &str,
        scalar: bool,
        groups: usize,
        set_groups: usize,
        distinct_values: usize,
        text_bytes: usize,
        check: impl Fn(&QueryResult),
    ) {
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0i64;
        let mut returned = 0usize;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            check(&result);
            checksum = 0;
            for row in &result.rows {
                for cell in row {
                    checksum = checksum.wrapping_add(profile_cell(cell));
                }
            }
            returned = result.rows.len();
            std::hint::black_box(&result.rows);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        let acc_bytes = if scalar {
            std::mem::size_of::<Vec<AccValue>>().saturating_add(std::mem::size_of::<AccValue>())
        } else {
            join_group_bytes(groups, 1, 1)
        };
        let set_bytes = distinct_set_bytes(set_groups, distinct_values, text_bytes);
        println!(
            "milestone13 shape={shape} rows={rows} returned={returned} groups={groups} distinct={distinct_values} text_bytes={text_bytes} acc_bytes={acc_bytes} set_bytes={set_bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    /// One `seen` vector and one `Option<DistinctSet>` per group, then one hash,
    /// bucket header, and `Value` per stored cell, plus inserted text lengths.
    /// Hash-table control bytes, spare capacity, and process RSS are not included.
    /// A group with no stored values contributes nothing. The profiled queries
    /// have one distinct aggregate, so each counted group owns one set.
    fn distinct_set_bytes(groups: usize, values: usize, text_bytes: usize) -> usize {
        if values == 0 {
            return 0;
        }
        let group = std::mem::size_of::<Vec<Option<DistinctSet>>>()
            .saturating_add(std::mem::size_of::<Option<DistinctSet>>());
        let entry = std::mem::size_of::<u64>()
            + std::mem::size_of::<Vec<Value>>()
            + std::mem::size_of::<Value>();
        groups
            .saturating_mul(group)
            .saturating_add(values.saturating_mul(entry))
            .saturating_add(text_bytes)
    }

    fn expected_amount_sum(rows: i64) -> i64 {
        (0..rows).filter(|id| id % 11 != 0).map(|id| id % 17).sum()
    }

    fn join_distinct_values(rows: i64) -> usize {
        let mut seen = vec![HashSet::<i64>::new(); 10];
        let orders = rows.saturating_mul(4);
        for id in 0..orders {
            if id % 2 == 0 {
                continue;
            }
            let bucket = usize::try_from((id % rows) % 10).unwrap_or(0);
            seen[bucket].insert(id % 17);
        }
        seen.iter().map(HashSet::len).sum()
    }

    fn load_distinct_profile(database: &mut Database, rows: i64) -> usize {
        database
            .execute("CREATE TABLE items (id INT, label TEXT, tag TEXT, bucket INT, amount INT);")
            .unwrap();
        database
            .execute("CREATE TABLE users (id INT, bucket INT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        let mut text_bytes = 0usize;
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 499).min(rows - 1);
            let mut items = String::from("INSERT INTO items VALUES ");
            let mut users = String::from("INSERT INTO users VALUES ");
            for id in start..=end {
                if id != start {
                    items.push(',');
                    users.push(',');
                }
                let label = (b'a' + u8::try_from(id % 8).unwrap()) as char;
                let tag = format!("n{id}");
                text_bytes += tag.len();
                let amount = if id % 11 == 0 {
                    "NULL".to_string()
                } else {
                    (id % 17).to_string()
                };
                items.push_str(&format!(
                    "({id}, '{label}', '{tag}', {}, {amount})",
                    id % 10
                ));
                users.push_str(&format!("({id}, {})", id % 10));
            }
            database.execute(&items).unwrap();
            database.execute(&users).unwrap();
            start = end + 1;
        }
        let orders = rows.saturating_mul(4);
        let mut start = 0_i64;
        while start < orders {
            let end = (start + 499).min(orders - 1);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let user = id % rows;
                if id % 2 == 0 {
                    sql.push_str(&format!("({user}, NULL)"));
                } else {
                    sql.push_str(&format!("({user}, {})", id % 17));
                }
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
        text_bytes
    }

    #[test]
    #[ignore = "release profile of SELECT DISTINCT"]
    fn profile_milestone14() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone14 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} row_vec_bytes={} map_entry_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Vec<Value>>(),
            std::mem::size_of::<u64>()
                + std::mem::size_of::<Vec<usize>>()
                + std::mem::size_of::<usize>()
        );
        for rows in [1_000_i64, 10_000, 100_000] {
            let mut database = Database::new();
            let text_bytes = load_select_distinct_profile(&mut database, rows);
            let pairs = first_seen_pairs(rows);
            profile_select_shape(
                &mut database,
                rows,
                "plain_bucket",
                "SELECT bucket FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
                0,
                false,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_bucket",
                "SELECT DISTINCT bucket FROM items;",
                10,
                1,
                0,
                true,
                |result| {
                    assert_eq!(
                        result.rows,
                        (0..10)
                            .map(|bucket| vec![Value::Integer(bucket)])
                            .collect::<Vec<_>>()
                    );
                },
            );
            let id_sum = rows.saturating_mul(rows - 1) / 2;
            profile_select_shape(
                &mut database,
                rows,
                "plain_id",
                "SELECT id FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
                0,
                false,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                    assert_eq!(profile_checksum(result), id_sum);
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_id",
                "SELECT DISTINCT id FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
                0,
                true,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                    assert_eq!(profile_checksum(result), id_sum);
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "plain_pair",
                "SELECT bucket, label FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                2,
                usize::try_from(rows).unwrap_or(usize::MAX),
                false,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_pair",
                "SELECT DISTINCT bucket, label FROM items;",
                pairs.len(),
                2,
                pairs.len(),
                true,
                |result| {
                    let expected = pairs
                        .iter()
                        .map(|(bucket, label)| {
                            vec![Value::Integer(*bucket), Value::Text(label.to_string())]
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(result.rows, expected);
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "plain_tag",
                "SELECT tag FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
                text_bytes,
                false,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_tag",
                "SELECT DISTINCT tag FROM items;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                1,
                text_bytes,
                true,
                |result| {
                    assert_eq!(
                        result.rows.len(),
                        usize::try_from(rows).unwrap_or(usize::MAX)
                    );
                    assert_eq!(
                        profile_checksum(result),
                        i64::try_from(text_bytes).unwrap_or(i64::MAX)
                    );
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_where",
                "SELECT DISTINCT bucket FROM items WHERE flag = TRUE;",
                5,
                1,
                0,
                true,
                |result| {
                    assert_eq!(
                        result.rows,
                        [0, 2, 4, 6, 8]
                            .into_iter()
                            .map(|bucket| vec![Value::Integer(bucket)])
                            .collect::<Vec<_>>()
                    );
                },
            );
            profile_select_shape(
                &mut database,
                rows,
                "distinct_order_limit",
                "SELECT DISTINCT label FROM items ORDER BY label ASC LIMIT 3;",
                8,
                1,
                8,
                true,
                |result| {
                    assert_eq!(
                        result.rows,
                        vec![
                            vec![Value::Text("a".to_string())],
                            vec![Value::Text("b".to_string())],
                            vec![Value::Text("c".to_string())],
                        ]
                    );
                },
            );
            let range = "SELECT DISTINCT bucket FROM items WHERE id >= 10 AND id < 40;";
            profile_indexed_range(rows, range);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn profile_select_shape(
        database: &mut Database,
        rows: i64,
        shape: &str,
        sql: &str,
        unique: usize,
        width: usize,
        text_bytes: usize,
        distinct: bool,
        check: impl Fn(&QueryResult),
    ) {
        let bytes = if distinct {
            distinct_output_bytes(unique, width, text_bytes)
        } else {
            projected_output_bytes(unique, width, text_bytes)
        };
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        let mut returned = 0_usize;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            check(&result);
            checksum = profile_checksum(&result);
            returned = result.rows.len();
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        let examined = usize::try_from(rows).unwrap_or(usize::MAX);
        println!(
            "milestone14 shape={shape} rows={rows} examined={examined} returned={returned} unique={unique} width={width} text_bytes={text_bytes} bytes={bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    #[test]
    #[ignore = "release profile of SELECT DISTINCT over an INNER JOIN"]
    fn profile_milestone16() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone16 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} row_vec_bytes={} pair_pos_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Vec<Value>>(),
            std::mem::size_of::<usize>().saturating_mul(2)
        );
        for rows in [100_i64, 1_000, 4_000, 10_000] {
            profile_equal_hash(rows);
            profile_duplicate_hash(rows);
            profile_skew_hash(rows);
            if rows <= 1_000 || rows == 4_000 {
                profile_equal_nested(rows);
            }
            profile_equal_index(rows);
            profile_skew_index(rows);
        }
    }

    fn profile_equal_hash(rows: i64) {
        let mut database = Database::new();
        load_profile_join(&mut database, rows, rows, rows);
        let names = 8_usize;
        let amounts = profile_amount_count(rows);
        let pairs = profile_name_amount_count(rows, rows, rows);
        let tag_bytes = usize::try_from(rows)
            .unwrap_or(usize::MAX)
            .saturating_mul(64);
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "plain_name",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            1,
            usize::try_from(rows).unwrap_or(usize::MAX),
            false,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            names,
            1,
            names,
            true,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "plain_ids",
            "SELECT u.id, o.id FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            2,
            0,
            false,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "distinct_ids",
            "SELECT DISTINCT u.id, o.id FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            2,
            0,
            true,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "distinct_amount",
            "SELECT DISTINCT o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
            amounts,
            1,
            0,
            true,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "distinct_tag",
            "SELECT DISTINCT u.tag FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            1,
            tag_bytes,
            true,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "distinct_order_limit",
            "SELECT DISTINCT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id ORDER BY name ASC, amount ASC LIMIT 5;",
            pairs,
            2,
            pairs,
            true,
        );
    }

    fn profile_duplicate_hash(rows: i64) {
        let mut database = Database::new();
        load_profile_join(&mut database, rows, rows, 10);
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "dup_plain_name",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            1,
            usize::try_from(rows).unwrap_or(usize::MAX),
            false,
        );
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            rows,
            rows,
            rows,
            "dup_distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            8,
            1,
            8,
            true,
        );
    }

    fn profile_skew_hash(rows: i64) {
        let users = (rows / 100).max(1);
        let mut database = Database::new();
        load_profile_join(&mut database, users, rows, users);
        profile_distinct_join_query(
            &mut database,
            "HashJoin",
            users,
            rows,
            rows,
            "skew_distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            profile_name_count(users),
            1,
            profile_name_count(users),
            true,
        );
    }

    fn profile_equal_nested(rows: i64) {
        let mut database = Database::new();
        load_profile_join(&mut database, rows, rows, rows);
        let _force = ForceNestedJoin::new();
        profile_distinct_join_query(
            &mut database,
            "NestedLoopJoin",
            rows,
            rows,
            rows,
            "distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            8,
            1,
            8,
            true,
        );
        if rows <= 1_000 {
            profile_distinct_join_query(
                &mut database,
                "NestedLoopJoin",
                rows,
                rows,
                rows,
                "distinct_ids",
                "SELECT DISTINCT u.id, o.id FROM users u JOIN orders o ON u.id = o.user_id;",
                usize::try_from(rows).unwrap_or(usize::MAX),
                2,
                0,
                true,
            );
        }
    }

    fn profile_equal_index(rows: i64) {
        let dir = crate::test_dir::TestDir::new("profile-m16-equal");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load_profile_join(&mut database, rows, rows, rows);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        let _force = ForceIndexJoin::new();
        profile_distinct_join_query(
            &mut database,
            "IndexNestedLoopJoin",
            rows,
            rows,
            rows,
            "distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            8,
            1,
            8,
            true,
        );
        profile_distinct_join_query(
            &mut database,
            "IndexNestedLoopJoin",
            rows,
            rows,
            rows,
            "distinct_ids",
            "SELECT DISTINCT u.id, o.id FROM users u JOIN orders o ON u.id = o.user_id;",
            usize::try_from(rows).unwrap_or(usize::MAX),
            2,
            0,
            true,
        );
    }

    fn profile_skew_index(rows: i64) {
        let users = (rows / 100).max(1);
        let dir = crate::test_dir::TestDir::new("profile-m16-skew");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load_profile_join(&mut database, users, rows, users);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        let names = profile_name_count(users);
        profile_distinct_join_query(
            &mut database,
            "IndexNestedLoopJoin",
            users,
            rows,
            rows,
            "skew_distinct_name",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            names,
            1,
            names,
            true,
        );
    }

    fn load_profile_join(database: &mut Database, users: i64, orders: i64, user_mod: i64) {
        database
            .execute("CREATE TABLE users (id INT, name TEXT, tag TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (id INT, user_id INT, amount INT);")
            .unwrap();
        let mut start = 0_i64;
        while start < users {
            let end = (start + 200).min(users);
            let mut sql = String::from("INSERT INTO users VALUES ");
            for id in start..end {
                if id > start {
                    sql.push(',');
                }
                let name = (b'a' + u8::try_from(id % 8).unwrap_or(0)) as char;
                sql.push_str(&format!("({id}, '{name}', '{id:064}')"));
            }
            sql.push(';');
            database.execute(&sql).unwrap();
            start = end;
        }
        start = 0;
        while start < orders {
            let end = (start + 200).min(orders);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..end {
                if id > start {
                    sql.push(',');
                }
                let user = id % user_mod;
                if id % 2 == 0 {
                    sql.push_str(&format!("({id}, {user}, NULL)"));
                } else {
                    sql.push_str(&format!("({id}, {user}, {})", id % 5));
                }
            }
            sql.push(';');
            database.execute(&sql).unwrap();
            start = end;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn profile_distinct_join_query(
        database: &mut Database,
        strategy: &str,
        users: i64,
        orders: i64,
        matches: i64,
        shape: &str,
        sql: &str,
        unique: usize,
        width: usize,
        text_bytes: usize,
        distinct: bool,
    ) {
        let explained = query(database, &format!("EXPLAIN {sql}"));
        let access = match &explained.rows[0][0] {
            Value::Text(access) => access.clone(),
            other => panic!("access cell {other:?}"),
        };
        assert_eq!(access, strategy, "{shape} users={users} orders={orders}");
        let returned_expect = if shape.contains("order_limit") {
            unique.min(5)
        } else if distinct {
            unique
        } else {
            usize::try_from(matches).unwrap_or(usize::MAX)
        };
        let bytes = if distinct {
            distinct_output_bytes(unique, width, text_bytes)
        } else {
            projected_output_bytes(returned_expect, width, text_bytes)
        };
        let pair_bytes = if strategy == "HashJoin" && orders > users {
            usize::try_from(matches)
                .unwrap_or(usize::MAX)
                .saturating_mul(std::mem::size_of::<usize>().saturating_mul(2))
        } else {
            0
        };
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        let mut returned = 0_usize;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            assert_eq!(result.rows.len(), returned_expect, "{shape}");
            checksum = profile_checksum(&result);
            returned = result.rows.len();
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        println!(
            "milestone16 strategy={strategy} shape={shape} users={users} orders={orders} matches={matches} returned={returned} unique={unique} width={width} text_bytes={text_bytes} distinct_bytes={bytes} pair_bytes={pair_bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    fn profile_name_count(users: i64) -> usize {
        usize::try_from(users.min(8)).unwrap_or(8)
    }

    fn profile_amount_count(orders: i64) -> usize {
        let mut seen = Vec::new();
        for id in 0..orders {
            let amount = if id % 2 == 0 { None } else { Some(id % 5) };
            if !seen.contains(&amount) {
                seen.push(amount);
            }
        }
        seen.len()
    }

    fn profile_name_amount_count(users: i64, orders: i64, user_mod: i64) -> usize {
        let mut seen = Vec::new();
        for id in 0..orders {
            let user = id % user_mod;
            if user >= users {
                continue;
            }
            let name = (b'a' + u8::try_from(user % 8).unwrap_or(0)) as char;
            let amount = if id % 2 == 0 { None } else { Some(id % 5) };
            let key = (name, amount);
            if !seen.contains(&key) {
                seen.push(key);
            }
        }
        seen.len()
    }

    fn profile_checksum(result: &QueryResult) -> i64 {
        let mut checksum = 0_i64;
        for row in &result.rows {
            for cell in row {
                checksum = checksum.wrapping_add(profile_cell(cell));
            }
        }
        checksum
    }

    fn distinct_output_bytes(unique: usize, width: usize, text_bytes: usize) -> usize {
        if unique == 0 {
            return 0;
        }
        let row = std::mem::size_of::<Vec<Value>>()
            .saturating_add(width.saturating_mul(std::mem::size_of::<Value>()));
        let entry = std::mem::size_of::<u64>()
            .saturating_add(std::mem::size_of::<Vec<usize>>())
            .saturating_add(std::mem::size_of::<usize>());
        unique
            .saturating_mul(row)
            .saturating_add(unique.saturating_mul(entry))
            .saturating_add(text_bytes)
    }

    fn projected_output_bytes(returned: usize, width: usize, text_bytes: usize) -> usize {
        if returned == 0 {
            return 0;
        }
        let row = std::mem::size_of::<Vec<Value>>()
            .saturating_add(width.saturating_mul(std::mem::size_of::<Value>()));
        returned.saturating_mul(row).saturating_add(text_bytes)
    }

    fn first_seen_pairs(rows: i64) -> Vec<(i64, char)> {
        let mut seen = Vec::new();
        for id in 0..rows {
            let pair = (id % 10, (b'a' + u8::try_from(id % 8).unwrap_or(0)) as char);
            if !seen.contains(&pair) {
                seen.push(pair);
            }
        }
        seen
    }

    fn profile_indexed_range(rows: i64, range: &str) {
        let dir = crate::test_dir::TestDir::new("select-distinct-profile");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load_select_distinct_profile(&mut database, rows);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        let expected = (0..10)
            .map(|bucket| vec![Value::Integer(bucket)])
            .collect::<Vec<_>>();
        let explained = query(&mut database, &format!("EXPLAIN {range}"));
        assert_eq!(explained.rows[0][0], Value::Text("SeqScan".to_string()));
        let sequential = query(&mut database, range);
        assert_eq!(sequential.rows, expected);
        profile_select_shape(
            &mut database,
            rows,
            "distinct_file_seq",
            range,
            10,
            1,
            0,
            true,
            |result| assert_eq!(result.rows, expected),
        );
        database
            .execute("CREATE INDEX idx_items_id ON items (id);")
            .unwrap();
        let explained = query(&mut database, &format!("EXPLAIN {range}"));
        assert_eq!(explained.rows[0][0], Value::Text("IndexScan".to_string()));
        assert_eq!(
            explained.rows[0][1],
            Value::Text("idx_items_id".to_string())
        );
        let indexed = query(&mut database, range);
        assert_eq!(indexed.rows, sequential.rows);
        let matches = query(
            &mut database,
            "SELECT COUNT(*) FROM items WHERE id >= 10 AND id < 40;",
        );
        assert_eq!(matches.rows, vec![vec![Value::Integer(30)]]);
        profile_range_shape(
            &mut database,
            rows,
            "distinct_file_index",
            range,
            30,
            &expected,
        );
    }

    fn profile_range_shape(
        database: &mut Database,
        rows: i64,
        shape: &str,
        sql: &str,
        examined: usize,
        expected: &[Vec<Value>],
    ) {
        let bytes = distinct_output_bytes(10, 1, 0);
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            assert_eq!(result.rows, expected);
            checksum = profile_checksum(&result);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        println!(
            "milestone14 shape={shape} rows={rows} examined={examined} returned=10 unique=10 width=1 text_bytes=0 bytes={bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    fn load_select_distinct_profile(database: &mut Database, rows: i64) -> usize {
        database
            .execute(
                "CREATE TABLE items (
                    id INT,
                    label TEXT,
                    tag TEXT,
                    bucket INT,
                    flag BOOLEAN
                );",
            )
            .unwrap();
        let mut text_bytes = 0_usize;
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 499).min(rows - 1);
            let mut sql = String::from("INSERT INTO items VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let label = (b'a' + u8::try_from(id % 8).unwrap_or(0)) as char;
                let tag = format!("{:24}{}", "", id);
                text_bytes = text_bytes.saturating_add(tag.len());
                let flag = if id % 2 == 0 { "TRUE" } else { "FALSE" };
                sql.push_str(&format!("({id}, '{label}', '{tag}', {}, {flag})", id % 10));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
        text_bytes
    }

    #[test]
    fn a_left_join_limit_still_evaluates_the_later_pair() {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![("user_id".to_string(), DataType::Integer)],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Column {
                name: "id".to_string(),
                qualifier: Some("u".to_string()),
            }),
            op: crate::ast::BinOp::Eq,
            right: Box::new(Expr::Column {
                name: "user_id".to_string(),
                qualifier: Some("o".to_string()),
            }),
        };
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: Vec::new(),
            output_indexes: vec![0, 1],
            output_names: vec!["id".to_string(), "user_id".to_string()],
            path: JoinAccess::NestedLoop,
        };
        let outer = vec![vec![Value::Integer(1)]];
        let inner = vec![
            vec![Value::Integer(1)],
            vec![Value::Text("later".to_string())],
        ];
        let error = scan_left_join(&outer, &inner, 1, prepared, Some(1)).unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
    }

    /// `LIMIT` has already accepted a row from an earlier left input. A later
    /// left row still evaluates `ON`, and that error replaces any partial output.
    #[test]
    fn a_later_left_row_still_reports_an_on_error_after_limit() {
        let error = scan_typed_left(
            vec![
                vec![Value::Integer(1)],
                vec![Value::Text("later".to_string())],
            ],
            vec![vec![Value::Integer(1)]],
            OnShape::Equal,
            None,
            false,
            Some(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
    }

    /// The second right row makes `ON` TRUE and then fails in `WHERE`.
    #[test]
    fn a_later_pair_still_reports_a_where_error_after_limit() {
        let error = scan_typed_left(
            vec![vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(1)],
                vec![Value::Text("later".to_string())],
            ],
            OnShape::True,
            Some(false),
            false,
            Some(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
    }

    /// `ON FALSE` rejects the stored right row, so `WHERE` runs on the null
    /// extension and still sees the bad left value.
    #[test]
    fn a_null_extended_row_still_reports_a_where_error() {
        let error = scan_typed_left(
            vec![
                vec![Value::Integer(1)],
                vec![Value::Text("later".to_string())],
            ],
            vec![vec![Value::Integer(9)]],
            OnShape::False,
            Some(true),
            false,
            Some(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
    }

    #[test]
    fn order_by_and_limit_zero_still_report_the_later_error() {
        let outer = vec![
            vec![Value::Integer(1)],
            vec![Value::Text("later".to_string())],
        ];
        let inner = vec![vec![Value::Integer(1)]];
        let sorted = scan_typed_left(
            outer.clone(),
            inner.clone(),
            OnShape::Equal,
            None,
            true,
            Some(1),
        );
        assert!(sorted.unwrap_err().to_string().contains("different types"));
        let limited = scan_typed_left(outer, inner, OnShape::Equal, None, false, Some(0));
        assert!(limited.unwrap_err().to_string().contains("different types"));
    }

    /// How `ON` is built for a late-error scan.
    enum OnShape {
        /// `u.id = o.user_id`.
        Equal,
        /// Literal TRUE. Every right row matches.
        True,
        /// Literal FALSE. Every left row is null-extended.
        False,
    }

    /// `where_outer` chooses `u.id > 0` or `o.user_id > 0`.
    fn scan_typed_left(
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
        on_shape: OnShape,
        where_outer: Option<bool>,
        sort: bool,
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![("user_id".to_string(), DataType::Integer)],
        )
        .unwrap();
        let on = match on_shape {
            OnShape::True => Expr::Literal(Value::Boolean(true)),
            OnShape::False => Expr::Literal(Value::Boolean(false)),
            OnShape::Equal => Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "id".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Eq,
                right: Box::new(Expr::Column {
                    name: "user_id".to_string(),
                    qualifier: Some("o".to_string()),
                }),
            },
        };
        let filter = where_outer.map(|outer_side| {
            let (name, qualifier) = if outer_side {
                ("id", "u")
            } else {
                ("user_id", "o")
            };
            expr::bind_filter(
                &Expr::Binary {
                    left: Box::new(Expr::Column {
                        name: name.to_string(),
                        qualifier: Some(qualifier.to_string()),
                    }),
                    op: crate::ast::BinOp::Gt,
                    right: Box::new(Expr::Literal(Value::Integer(0))),
                },
                &scope,
                "WHERE",
            )
            .unwrap()
        });
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter,
            sort_keys: if sort {
                vec![SortKey {
                    index: 0,
                    ascending: true,
                    nulls_first: false,
                }]
            } else {
                Vec::new()
            },
            output_indexes: vec![0, 1],
            output_names: vec!["id".to_string(), "user_id".to_string()],
            path: JoinAccess::NestedLoop,
        };
        scan_left_join(&outer, &inner, 1, prepared, limit)
    }

    #[test]
    fn a_hash_left_join_still_reports_a_later_residual_error() {
        let error = scan_hash_amount(
            vec![vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(1), Value::Integer(5)],
                vec![Value::Integer(1), Value::Text("later".to_string())],
            ],
            Some(1),
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
        let limited = scan_hash_amount(
            vec![vec![Value::Integer(1)]],
            vec![
                vec![Value::Integer(1), Value::Integer(5)],
                vec![Value::Integer(1), Value::Text("later".to_string())],
            ],
            Some(0),
            true,
        );
        assert!(limited.unwrap_err().to_string().contains("different types"));
    }

    #[test]
    fn a_hash_left_null_extension_still_reports_a_where_error() {
        let error = scan_hash_where_outer(
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Text("later".to_string())],
            ],
            vec![vec![Value::Integer(9), Value::Integer(1)]],
            Some(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
    }

    /// The second right key is text stored in an integer column. It is not an
    /// exact hash key, but `1 = 'later'` is a type error, including after
    /// `LIMIT` has accepted the first match.
    #[test]
    fn a_mistyped_key_still_errors_after_limit() {
        let outer = vec![vec![Value::Integer(1)]];
        let inner = vec![
            vec![Value::Integer(1)],
            vec![Value::Text("later".to_string())],
        ];
        let hashed = scan_hash_equal(outer.clone(), inner.clone(), Some(1), false);
        let nested = scan_typed_left(
            outer.clone(),
            inner.clone(),
            OnShape::Equal,
            None,
            false,
            Some(1),
        );
        assert_eq!(
            hashed.unwrap_err().to_string(),
            nested.unwrap_err().to_string()
        );
        let sorted = scan_hash_equal(outer.clone(), inner.clone(), Some(1), true);
        let nested_sorted = scan_typed_left(outer, inner, OnShape::Equal, None, true, Some(1));
        assert_eq!(
            sorted.unwrap_err().to_string(),
            nested_sorted.unwrap_err().to_string()
        );
    }

    /// `user_id` does not match, and `amount` is text. The residual comparison
    /// still runs, so the hash path cannot treat the key miss as success.
    #[test]
    fn a_mistyped_residual_on_another_key_still_errors() {
        let outer = vec![vec![Value::Integer(1)]];
        let inner = vec![vec![Value::Integer(2), Value::Text("later".to_string())]];
        let hashed = scan_amount(true, outer.clone(), inner.clone(), Some(0), false);
        let nested = scan_amount(false, outer, inner, Some(0), false);
        assert_eq!(
            hashed.unwrap_err().to_string(),
            nested.unwrap_err().to_string()
        );
    }

    /// A NULL left key does not make `NULL = text` a type error by itself.
    /// The residual comparison on the text amount still does.
    #[test]
    fn a_null_probe_still_reports_a_mistyped_residual() {
        let outer = vec![vec![Value::Null]];
        let inner = vec![vec![Value::Integer(1), Value::Text("later".to_string())]];
        let hashed = scan_amount(true, outer.clone(), inner.clone(), None, true);
        let nested = scan_amount(false, outer, inner, None, true);
        assert_eq!(
            hashed.unwrap_err().to_string(),
            nested.unwrap_err().to_string()
        );
    }

    /// `IS NULL` does not reject a text cell. The key matches and the residual
    /// is FALSE, so both paths null-extend and the next query can succeed.
    #[test]
    fn a_mistyped_cell_under_is_null_matches_the_nested_loop() {
        let outer = vec![vec![Value::Integer(1)], vec![Value::Integer(2)]];
        let inner = vec![vec![Value::Integer(1), Value::Text("kept".to_string())]];
        let hashed = scan_is_null_amount(true, outer.clone(), inner.clone()).unwrap();
        let nested = scan_is_null_amount(false, outer, inner).unwrap();
        let StatementResult::Query(hashed) = hashed else {
            panic!("expected a query");
        };
        let StatementResult::Query(nested) = nested else {
            panic!("expected a query");
        };
        assert_eq!(hashed.rows, nested.rows);
        assert_eq!(
            hashed.rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Null],
            ]
        );
        let followed = scan_hash_equal(
            vec![vec![Value::Integer(1)]],
            vec![vec![Value::Integer(1)]],
            None,
            false,
        )
        .unwrap();
        let StatementResult::Query(followed) = followed else {
            panic!("expected a query");
        };
        assert_eq!(
            followed.rows,
            vec![vec![Value::Integer(1), Value::Integer(1)]]
        );
    }

    /// No left row means the mistyped right key is never compared.
    #[test]
    fn an_empty_left_side_does_not_read_a_mistyped_right_key() {
        let result = scan_hash_equal(
            Vec::new(),
            vec![vec![Value::Text("later".to_string())]],
            Some(1),
            false,
        )
        .unwrap();
        let StatementResult::Query(result) = result else {
            panic!("expected a query");
        };
        assert!(result.rows.is_empty());
    }

    #[test]
    fn a_left_aggregate_matches_the_nested_loop_and_a_forced_collision() {
        let mut db = Database::new();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy'), (NULL, 'Dee');")
            .unwrap();
        db.execute("INSERT INTO orders VALUES (1, 50), (1, 150), (1, NULL), (2, 10), (NULL, 5);")
            .unwrap();
        let sqls = [
            "SELECT COUNT(*), COUNT(o.user_id), COUNT(o.amount), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
            "SELECT u.name, COUNT(*), COUNT(o.user_id), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
            "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 100;",
            "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 100;",
            "SELECT o.user_id, COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY o.user_id;",
            "SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
        ];
        let nested = {
            let _force = ForceNestedJoin::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        let hashed = sqls
            .iter()
            .map(|sql| query(&mut db, sql).rows)
            .collect::<Vec<_>>();
        assert_eq!(hashed, nested);
        let collided = {
            let _force = ForceHashCollision::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        assert_eq!(collided, nested);
        let explained = query(
            &mut db,
            "EXPLAIN SELECT u.name, COUNT(*) AS n FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.name;",
        );
        assert_eq!(
            explained.rows[0][0],
            Value::Text("HashLeftJoin".to_string())
        );
        assert_eq!(explained.rows[0][3], Value::Text("o (orders)".to_string()));
    }

    #[test]
    fn a_mistyped_key_fails_a_left_aggregate_before_publish() {
        let prepared = equality_join();
        let access = plan::LeftAccess::Hash {
            outer_ordinal: 0,
            inner_ordinal: 0,
            predicate: "u.id = o.user_id".to_string(),
            residual: None,
        };
        let plan = count_star_plan();
        let mut sink = GroupSink::new(&plan);
        let error = consume_left_join(
            &[vec![Value::Integer(1)]],
            &[
                vec![Value::Integer(1)],
                vec![Value::Text("later".to_string())],
            ],
            &[DataType::Integer],
            &[DataType::Integer],
            &access,
            &prepared,
            |outer_row, inner_row| sink.push(InputRow::Pair(outer_row, inner_row)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
        drop(sink);
        let mut sink = GroupSink::new(&plan);
        consume_left_join(
            &[vec![Value::Integer(1)]],
            &[vec![Value::Integer(1)]],
            &[DataType::Integer],
            &[DataType::Integer],
            &access,
            &prepared,
            |outer_row, inner_row| sink.push(InputRow::Pair(outer_row, inner_row)),
        )
        .unwrap();
        let rows = sink.finish().unwrap();
        assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    }

    #[test]
    fn text_boolean_and_two_equalities_agree_across_left_join_strategies() {
        let mut db = Database::new();
        db.execute("CREATE TABLE flags (id BOOLEAN, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE checks (id BOOLEAN, ok BOOLEAN);")
            .unwrap();
        db.execute("INSERT INTO flags VALUES (TRUE, 'a'), (FALSE, 'a'), (NULL, 'b'), (TRUE, 'b');")
            .unwrap();
        db.execute("INSERT INTO checks VALUES (TRUE, TRUE), (TRUE, FALSE), (FALSE, NULL);")
            .unwrap();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, name TEXT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Ada'), (3, 'Bea'), (NULL, 'Ada');")
            .unwrap();
        db.execute(
            "INSERT INTO orders VALUES (1, 'Ada', 10), (1, 'no', 99), (2, 'Ada', 5), (1, 'Ada', 10);",
        )
        .unwrap();
        let sqls = [
            "SELECT f.name, COUNT(*), COUNT(f.id), COUNT(c.ok), MIN(c.ok), MAX(c.ok) FROM flags f LEFT JOIN checks c ON f.id = c.id GROUP BY f.name;",
            "SELECT COUNT(*), SUM(o.amount) FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.name;",
            "SELECT u.name, COUNT(*), COUNT(b.id) FROM users u LEFT JOIN users b ON u.id = b.id GROUP BY u.name;",
            "SELECT COUNT(*) FROM flags f LEFT JOIN checks c ON f.id = c.id OR c.ok = TRUE;",
            "SELECT t.name, COUNT(*) FROM texts t LEFT JOIN labels l ON t.name = l.name GROUP BY t.name;",
        ];
        db.execute("CREATE TABLE texts (name TEXT);").unwrap();
        db.execute("CREATE TABLE labels (name TEXT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO texts VALUES ('Ada'), ('Bea'), ('Cy');")
            .unwrap();
        db.execute("INSERT INTO labels VALUES ('Ada', 2), ('Ada', 4), ('No', 9);")
            .unwrap();
        let nested = {
            let _force = ForceNestedJoin::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        let hashed = sqls
            .iter()
            .map(|sql| query(&mut db, sql).rows)
            .collect::<Vec<_>>();
        assert_eq!(hashed, nested);
        let collided = {
            let _force = ForceHashCollision::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        assert_eq!(collided, nested);
        let explained = query(
            &mut db,
            "EXPLAIN SELECT COUNT(*) FROM users u LEFT JOIN orders o ON u.id = o.user_id AND u.name = o.name;",
        );
        assert_eq!(
            explained.rows[0][0],
            Value::Text("HashLeftJoin".to_string())
        );
        assert_eq!(
            explained.rows[0][4],
            Value::Text("u.id = o.user_id".to_string())
        );
        let Value::Text(residual) = &explained.rows[0][5] else {
            panic!("residual {}", explained.rows[0][5]);
        };
        assert!(residual.contains("u.name = o.name"), "{residual}");
    }

    #[test]
    fn a_null_extended_pair_leaves_sum_and_a_later_error_rolls_nothing_back() {
        let aggs = [
            AggFn::CountStar,
            AggFn::CountColumn(0),
            AggFn::CountColumn(1),
            AggFn::CountColumn(3),
            AggFn::Sum(3),
            AggFn::Avg(3),
            AggFn::Min(3),
            AggFn::Max(3),
        ];
        let mut acc = fresh_acc(&aggs);
        let mut seen = fresh_seen(&aggs);
        let ada = [Value::Integer(1), Value::Text("Ada".to_string())];
        let other = [Value::Integer(2), Value::Text("Ada".to_string())];
        let null_name = [Value::Integer(3), Value::Null];
        let nulls = [Value::Null, Value::Null];
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&ada, &[Value::Integer(1), Value::Integer(10)]),
            &aggs,
        )
        .unwrap();
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&ada, &[Value::Integer(1), Value::Integer(20)]),
            &aggs,
        )
        .unwrap();
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&ada, &[Value::Integer(1), Value::Null]),
            &aggs,
        )
        .unwrap();
        update_acc(&mut acc, &mut seen, InputRow::Pair(&other, &nulls), &aggs).unwrap();
        update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&null_name, &nulls),
            &aggs,
        )
        .unwrap();
        match &acc[..] {
            [
                AccValue::Count(5),
                AccValue::Count(5),
                AccValue::Count(4),
                AccValue::Count(2),
                AccValue::Sum(Some(30)),
                AccValue::Avg { sum: 30, count: 2 },
                AccValue::Extreme(Some(Value::Integer(10))),
                AccValue::Extreme(Some(Value::Integer(20))),
            ] => {}
            other => panic!("pair aggregate {other:?}"),
        }
        let saved = acc.clone();
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&ada, &[Value::Integer(1), Value::Integer(i64::MAX)]),
            &aggs,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert_eq!(acc, saved);
        let error = update_acc(
            &mut acc,
            &mut seen,
            InputRow::Pair(&ada, &[Value::Integer(1), Value::Text("later".to_string())]),
            &aggs,
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-integer"), "{error}");
        assert_eq!(acc, saved);
    }

    #[test]
    fn a_mistyped_residual_fails_before_a_left_aggregate_is_published() {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![
                ("user_id".to_string(), DataType::Integer),
                ("amount".to_string(), DataType::Integer),
            ],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "id".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Eq,
                right: Box::new(Expr::Column {
                    name: "user_id".to_string(),
                    qualifier: Some("o".to_string()),
                }),
            }),
            op: crate::ast::BinOp::And,
            right: Box::new(Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "amount".to_string(),
                    qualifier: Some("o".to_string()),
                }),
                op: crate::ast::BinOp::Gt,
                right: Box::new(Expr::Literal(Value::Integer(0))),
            }),
        };
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: Vec::new(),
            output_indexes: Vec::new(),
            output_names: Vec::new(),
            path: JoinAccess::NestedLoop,
        };
        let access = plan::LeftAccess::Hash {
            outer_ordinal: 0,
            inner_ordinal: 0,
            predicate: "u.id = o.user_id".to_string(),
            residual: Some("u.id = o.user_id AND o.amount > 0".to_string()),
        };
        let plan = count_star_plan();
        let mut sink = GroupSink::new(&plan);
        let error = consume_left_join(
            &[vec![Value::Integer(1)]],
            &[
                vec![Value::Integer(1), Value::Integer(5)],
                vec![Value::Integer(1), Value::Text("later".to_string())],
            ],
            &[DataType::Integer],
            &[DataType::Integer, DataType::Integer],
            &access,
            &prepared,
            |outer_row, inner_row| sink.push(InputRow::Pair(outer_row, inner_row)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
        drop(sink);
        let mut sink = GroupSink::new(&plan);
        consume_left_join(
            &[vec![Value::Integer(1)]],
            &[vec![Value::Integer(1), Value::Integer(5)]],
            &[DataType::Integer],
            &[DataType::Integer, DataType::Integer],
            &access,
            &prepared,
            |outer_row, inner_row| sink.push(InputRow::Pair(outer_row, inner_row)),
        )
        .unwrap();
        assert_eq!(sink.finish().unwrap(), vec![vec![Value::Integer(1)]]);
    }

    fn equality_join() -> PreparedJoin {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![("user_id".to_string(), DataType::Integer)],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Column {
                name: "id".to_string(),
                qualifier: Some("u".to_string()),
            }),
            op: crate::ast::BinOp::Eq,
            right: Box::new(Expr::Column {
                name: "user_id".to_string(),
                qualifier: Some("o".to_string()),
            }),
        };
        PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: Vec::new(),
            output_indexes: Vec::new(),
            output_names: Vec::new(),
            path: JoinAccess::NestedLoop,
        }
    }

    fn count_star_plan() -> AggPlan {
        AggPlan {
            group_ordinals: Vec::new(),
            group_labels: Vec::new(),
            aggs: vec![AggFn::CountStar],
            outputs: vec![0],
            output_names: vec!["COUNT(*)".to_string()],
            slot_labels: vec!["COUNT(*)".to_string()],
            alias_labels: Vec::new(),
            aggregate_labels: vec!["COUNT(*)".to_string()],
            sort_keys: Vec::new(),
            filter: None,
            having: None,
            path: AccessPath::SeqScan,
            access: "Aggregate".to_string(),
        }
    }

    fn scan_hash_equal(
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
        limit: Option<u64>,
        sort: bool,
    ) -> Result<StatementResult, DbError> {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![("user_id".to_string(), DataType::Integer)],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Column {
                name: "id".to_string(),
                qualifier: Some("u".to_string()),
            }),
            op: crate::ast::BinOp::Eq,
            right: Box::new(Expr::Column {
                name: "user_id".to_string(),
                qualifier: Some("o".to_string()),
            }),
        };
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: if sort {
                vec![SortKey {
                    index: 0,
                    ascending: true,
                    nulls_first: false,
                }]
            } else {
                Vec::new()
            },
            output_indexes: vec![0, 1],
            output_names: vec!["id".to_string(), "user_id".to_string()],
            path: JoinAccess::NestedLoop,
        };
        scan_hash_left_join(
            &outer,
            &inner,
            &[DataType::Integer],
            &[DataType::Integer],
            HashKeys {
                outer_ordinal: 0,
                inner_ordinal: 0,
            },
            prepared,
            limit,
        )
    }

    fn scan_is_null_amount(
        hash: bool,
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
    ) -> Result<StatementResult, DbError> {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![
                ("user_id".to_string(), DataType::Integer),
                ("amount".to_string(), DataType::Integer),
            ],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "id".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Eq,
                right: Box::new(Expr::Column {
                    name: "user_id".to_string(),
                    qualifier: Some("o".to_string()),
                }),
            }),
            op: crate::ast::BinOp::And,
            right: Box::new(Expr::IsNull(Box::new(Expr::Column {
                name: "amount".to_string(),
                qualifier: Some("o".to_string()),
            }))),
        };
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: Vec::new(),
            output_indexes: vec![0, 2],
            output_names: vec!["id".to_string(), "amount".to_string()],
            path: JoinAccess::NestedLoop,
        };
        if !hash {
            return scan_left_join(&outer, &inner, 2, prepared, None);
        }
        scan_hash_left_join(
            &outer,
            &inner,
            &[DataType::Integer],
            &[DataType::Integer, DataType::Integer],
            HashKeys {
                outer_ordinal: 0,
                inner_ordinal: 0,
            },
            prepared,
            None,
        )
    }

    #[test]
    fn a_forced_collision_matches_the_nested_left_join() {
        let mut db = Database::new();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy'), (NULL, 'Dee');")
            .unwrap();
        db.execute("INSERT INTO orders VALUES (1, 50), (1, 150), (1, NULL), (2, 10), (NULL, 5);")
            .unwrap();
        let sqls = [
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.amount > 100;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 100;",
            "SELECT u.name, o.user_id, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.user_id IS NULL;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > 1000;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount = NULL;",
            "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > 1000;",
        ];
        let nested = {
            let _force = ForceNestedJoin::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        let hashed = sqls
            .iter()
            .map(|sql| query(&mut db, sql).rows)
            .collect::<Vec<_>>();
        assert_eq!(hashed, nested);
        let collided = {
            let _force = ForceHashCollision::new();
            sqls.iter()
                .map(|sql| query(&mut db, sql).rows)
                .collect::<Vec<_>>()
        };
        assert_eq!(collided, nested);
        let explained = query(
            &mut db,
            "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(
            explained.rows[0][0],
            Value::Text("HashLeftJoin".to_string())
        );
        assert_eq!(explained.rows[0][3], Value::Text("o (orders)".to_string()));
        let nested_plan = {
            let _force = ForceNestedJoin::new();
            query(
                &mut db,
                "EXPLAIN SELECT u.name FROM users u LEFT JOIN orders o ON u.id = o.user_id OR o.amount = 5;",
            )
        };
        assert_eq!(
            nested_plan.rows[0][0],
            Value::Text("LeftNestedLoopJoin".to_string())
        );
    }

    fn scan_hash_amount(
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
        limit: Option<u64>,
        sort: bool,
    ) -> Result<StatementResult, DbError> {
        scan_amount(true, outer, inner, limit, sort)
    }

    fn scan_amount(
        hash: bool,
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
        limit: Option<u64>,
        sort: bool,
    ) -> Result<StatementResult, DbError> {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![
                ("user_id".to_string(), DataType::Integer),
                ("amount".to_string(), DataType::Integer),
            ],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "id".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Eq,
                right: Box::new(Expr::Column {
                    name: "user_id".to_string(),
                    qualifier: Some("o".to_string()),
                }),
            }),
            op: crate::ast::BinOp::And,
            right: Box::new(Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "amount".to_string(),
                    qualifier: Some("o".to_string()),
                }),
                op: crate::ast::BinOp::Gt,
                right: Box::new(Expr::Literal(Value::Integer(0))),
            }),
        };
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: None,
            sort_keys: if sort {
                vec![SortKey {
                    index: 0,
                    ascending: true,
                    nulls_first: false,
                }]
            } else {
                Vec::new()
            },
            output_indexes: vec![0, 2],
            output_names: vec!["id".to_string(), "amount".to_string()],
            path: JoinAccess::NestedLoop,
        };
        if !hash {
            return scan_left_join(&outer, &inner, 2, prepared, limit);
        }
        scan_hash_left_join(
            &outer,
            &inner,
            &[DataType::Integer],
            &[DataType::Integer, DataType::Integer],
            HashKeys {
                outer_ordinal: 0,
                inner_ordinal: 0,
            },
            prepared,
            limit,
        )
    }

    fn scan_hash_where_outer(
        outer: Vec<Vec<Value>>,
        inner: Vec<Vec<Value>>,
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        let scope = Scope::try_join(
            "u",
            vec![
                ("id".to_string(), DataType::Integer),
                ("n".to_string(), DataType::Integer),
            ],
            "o",
            vec![
                ("user_id".to_string(), DataType::Integer),
                ("amount".to_string(), DataType::Integer),
            ],
        )
        .unwrap();
        let on = Expr::Binary {
            left: Box::new(Expr::Column {
                name: "id".to_string(),
                qualifier: Some("u".to_string()),
            }),
            op: crate::ast::BinOp::Eq,
            right: Box::new(Expr::Column {
                name: "user_id".to_string(),
                qualifier: Some("o".to_string()),
            }),
        };
        let filter = expr::bind_filter(
            &Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "n".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Gt,
                right: Box::new(Expr::Literal(Value::Integer(0))),
            },
            &scope,
            "WHERE",
        )
        .unwrap();
        let prepared = PreparedJoin {
            on: expr::bind_filter(&on, &scope, "ON").unwrap(),
            filter: Some(filter),
            sort_keys: Vec::new(),
            output_indexes: vec![0, 1],
            output_names: vec!["id".to_string(), "n".to_string()],
            path: JoinAccess::NestedLoop,
        };
        scan_hash_left_join(
            &outer,
            &inner,
            &[DataType::Integer, DataType::Integer],
            &[DataType::Integer, DataType::Integer],
            HashKeys {
                outer_ordinal: 0,
                inner_ordinal: 0,
            },
            prepared,
            limit,
        )
    }

    #[test]
    #[ignore = "release profile of LEFT JOIN"]
    fn profile_milestone18() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone18 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} row_vec_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Vec<Value>>()
        );
        let _nested = ForceNestedJoin::new();
        for rows in [100_i64, 1_000, 4_000] {
            for shape in [
                "matched",
                "none",
                "few",
                "duplicates",
                "where",
                "order_limit",
            ] {
                profile_left_shape(rows, rows, shape);
            }
        }
        for (users, orders) in [
            (100_i64, 4_000),
            (4_000, 100),
            (1_000, 4_000),
            (4_000, 1_000),
        ] {
            for shape in ["matched", "none", "duplicates", "where", "order_limit"] {
                profile_left_shape(users, orders, shape);
            }
        }
    }

    fn profile_left_shape(users: i64, orders: i64, shape: &str) {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        load_left_users(&mut database, users);
        load_left_orders(&mut database, orders, shape);
        let sql = match shape {
            "where" => format!(
                "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > {};",
                orders / 2
            ),
            "order_limit" => "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY o.amount ASC LIMIT 5;".to_string(),
            _ => "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;".to_string(),
        };
        let explained = query(&mut database, &format!("EXPLAIN {sql}"));
        match &explained.rows[0][0] {
            Value::Text(access) => assert_eq!(access, "LeftNestedLoopJoin"),
            other => panic!("access {other:?}"),
        }
        let (matched, extended, returned) = left_counts(users, orders, shape);
        let examined = users.saturating_mul(orders);
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(&mut database, &sql);
            samples.push(started.elapsed().as_micros());
            assert_eq!(result.rows.len(), returned, "{shape} {users}x{orders}");
            checksum = profile_checksum(&result);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        let warm = warm[warm.len() / 2];
        let stored = if shape == "order_limit" {
            matched + extended
        } else {
            returned
        };
        let width: usize = if shape == "order_limit" { 4 } else { 2 };
        let bytes = stored.saturating_mul(
            std::mem::size_of::<Vec<Value>>()
                .saturating_add(width.saturating_mul(std::mem::size_of::<Value>())),
        );
        println!(
            "milestone18 shape={shape} users={users} orders={orders} examined={examined} matched={matched} null_extended={extended} returned={returned} stored_rows={stored} bytes={bytes} checksum={checksum} cold_us={cold} warm_us={warm}"
        );
    }

    /// Counts the same pairs the nested loop emits. This is not the executor.
    fn left_counts(users: i64, orders: i64, shape: &str) -> (usize, usize, usize) {
        let threshold = orders / 2;
        let mut matched = 0_usize;
        let mut extended = 0_usize;
        let mut kept = 0_usize;
        for user in 0..users {
            let mut hit = false;
            for order in 0..orders {
                let user_id = match shape {
                    "none" => -1,
                    "few" => 0,
                    "duplicates" => order % 10,
                    _ => order,
                };
                if user_id != user {
                    continue;
                }
                if shape == "residual" && order <= threshold {
                    continue;
                }
                hit = true;
                matched += 1;
                if shape != "where" || order > threshold {
                    kept += 1;
                }
            }
            if !hit {
                extended += 1;
                if shape != "where" {
                    kept += 1;
                }
            }
        }
        let returned = if shape == "order_limit" {
            kept.min(5)
        } else {
            kept
        };
        (matched, extended, returned)
    }

    fn load_left_users(database: &mut Database, rows: i64) {
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 400).min(rows - 1);
            let mut sql = String::from("INSERT INTO users VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let label = (b'a' + u8::try_from(id % 8).unwrap_or(0)) as char;
                sql.push_str(&format!("({id}, '{label}')"));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    fn load_left_orders(database: &mut Database, rows: i64, shape: &str) {
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 400).min(rows - 1);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let user_id = match shape {
                    "none" => -1,
                    "few" => 0,
                    "duplicates" => id % 10,
                    _ => id,
                };
                sql.push_str(&format!("({user_id}, {id})"));
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    #[test]
    #[ignore = "release profile of hash LEFT JOIN"]
    fn profile_milestone19() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone19 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} row_vec_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Vec<Value>>()
        );
        for rows in [100_i64, 1_000, 4_000, 10_000] {
            for shape in [
                "matched",
                "few",
                "none",
                "duplicates",
                "residual",
                "where",
                "order_limit",
            ] {
                profile_hash_left(rows, rows, shape);
            }
        }
        for (users, orders) in [
            (100_i64, 4_000),
            (4_000, 100),
            (1_000, 10_000),
            (10_000, 1_000),
        ] {
            for shape in ["matched", "none", "duplicates", "where", "order_limit"] {
                profile_hash_left(users, orders, shape);
            }
        }
    }

    fn profile_hash_left(users: i64, orders: i64, shape: &str) {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        load_left_users(&mut database, users);
        load_left_orders(&mut database, orders, shape);
        let sql = match shape {
            "where" => format!(
                "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE o.amount > {};",
                orders / 2
            ),
            "residual" => format!(
                "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id AND o.amount > {};",
                orders / 2
            ),
            "order_limit" => "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id ORDER BY o.amount ASC LIMIT 5;".to_string(),
            _ => "SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON u.id = o.user_id;".to_string(),
        };
        let explained = query(&mut database, &format!("EXPLAIN {sql}"));
        assert_eq!(
            explained.rows[0][0],
            Value::Text("HashLeftJoin".to_string()),
            "{shape}"
        );
        assert_eq!(explained.rows[0][3], Value::Text("o (orders)".to_string()));
        let (matched, extended, returned) = left_counts(users, orders, shape);
        let examined = hash_left_examined(users, orders, shape);
        let (groups, positions) = hash_left_build(orders, shape);
        let (hash_cold, hash_warm, hash_sum) = time_left(&mut database, &sql, returned, shape);
        let (nested_cold, nested_warm, nested_sum) = {
            let _force = ForceNestedJoin::new();
            time_left(&mut database, &sql, returned, shape)
        };
        assert_eq!(hash_sum, nested_sum, "{shape} {users}x{orders}");
        let hash_bytes = hash_table_bytes(groups, positions);
        let copy_rows = if orders <= users { orders } else { users };
        let copy_bytes = usize::try_from(copy_rows)
            .unwrap_or(usize::MAX)
            .saturating_mul(
                std::mem::size_of::<Vec<Value>>()
                    .saturating_add(2usize.saturating_mul(std::mem::size_of::<Value>())),
            );
        let nested_examined = users.saturating_mul(orders);
        println!(
            "milestone19 shape={shape} users={users} orders={orders} examined={examined} nested_examined={nested_examined} matched={matched} null_extended={extended} returned={returned} groups={groups} positions={positions} hash_bytes={hash_bytes} copy_rows={copy_rows} copy_bytes={copy_bytes} hash_cold_us={hash_cold} hash_warm_us={hash_warm} nested_cold_us={nested_cold} nested_warm_us={nested_warm} checksum={hash_sum}"
        );
    }

    fn time_left(
        database: &mut Database,
        sql: &str,
        returned: usize,
        shape: &str,
    ) -> (u128, u128, i64) {
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            assert_eq!(result.rows.len(), returned, "{shape}");
            checksum = profile_checksum(&result);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        (cold, warm[warm.len() / 2], checksum)
    }

    fn hash_left_examined(users: i64, orders: i64, shape: &str) -> usize {
        if users == 0 || orders == 0 {
            return 0;
        }
        match shape {
            "none" => 0,
            "few" => usize::try_from(orders).unwrap_or(usize::MAX),
            "duplicates" => (0..orders).filter(|order| order % 10 < users).count(),
            _ => usize::try_from(users.min(orders)).unwrap_or(usize::MAX),
        }
    }

    fn hash_left_build(orders: i64, shape: &str) -> (usize, usize) {
        if orders == 0 {
            return (0, 0);
        }
        let positions = usize::try_from(orders).unwrap_or(usize::MAX);
        let groups = match shape {
            "none" | "few" => 1,
            "duplicates" => usize::try_from(orders.min(10)).unwrap_or(10),
            _ => positions,
        };
        (groups, positions)
    }

    #[test]
    #[ignore = "release profile of LEFT JOIN aggregation"]
    fn profile_milestone20() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone20 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} acc_bytes={} row_vec_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<AccValue>(),
            std::mem::size_of::<Vec<Value>>()
        );
        for rows in [100_i64, 1_000, 4_000] {
            for shape in [
                "matched",
                "none",
                "duplicates",
                "nulls",
                "where",
                "residual",
            ] {
                for kind in ["plain", "count_star", "count_amount", "group", "multi"] {
                    profile_left_aggregate(rows, rows, shape, kind);
                }
            }
        }
        for rows in [10_000_i64] {
            for shape in ["matched", "none"] {
                for kind in ["count_star", "count_amount", "group", "multi"] {
                    profile_left_aggregate(rows, rows, shape, kind);
                }
            }
        }
        for (users, orders) in [(100_i64, 4_000), (4_000, 100)] {
            for shape in ["matched", "none", "duplicates", "where"] {
                for kind in ["count_star", "group", "multi"] {
                    profile_left_aggregate(users, orders, shape, kind);
                }
            }
        }
    }

    fn profile_left_aggregate(users: i64, orders: i64, shape: &str, kind: &str) {
        let mut database = Database::new();
        database
            .execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        database
            .execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        load_left_users(&mut database, users);
        profile_load_orders(&mut database, orders, shape);
        let threshold = orders / 2;
        let on = if shape == "residual" {
            format!("u.id = o.user_id AND o.amount > {threshold}")
        } else {
            "u.id = o.user_id".to_string()
        };
        let filter = if shape == "where" {
            format!(" WHERE o.amount > {threshold}")
        } else {
            String::new()
        };
        let sql = match kind {
            "plain" => {
                format!("SELECT u.name, o.amount FROM users u LEFT JOIN orders o ON {on}{filter};")
            }
            "count_star" => {
                format!("SELECT COUNT(*) FROM users u LEFT JOIN orders o ON {on}{filter};")
            }
            "count_amount" => {
                format!("SELECT COUNT(o.amount) FROM users u LEFT JOIN orders o ON {on}{filter};")
            }
            "group" => format!(
                "SELECT u.name, COUNT(*) FROM users u LEFT JOIN orders o ON {on}{filter} GROUP BY u.name;"
            ),
            "multi" => format!(
                "SELECT SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount) FROM users u LEFT JOIN orders o ON {on}{filter};"
            ),
            _ => panic!("kind {kind}"),
        };
        let explained = query(&mut database, &format!("EXPLAIN {sql}"));
        let access = match &explained.rows[0][0] {
            Value::Text(access) => access.clone(),
            other => panic!("access {other:?}"),
        };
        assert_eq!(access, "HashLeftJoin");
        let (matched, extended, kept) = left_counts(users, orders, shape);
        let examined = hash_left_examined(users, orders, shape);
        let inputs = kept;
        let aggs: usize = match kind {
            "count_star" | "count_amount" | "group" => 1,
            "multi" => 4,
            _ => 0,
        };
        let (hash_cold, hash_warm, hash_sum, hash_rows) = time_agg(&mut database, &sql);
        let (nested_cold, nested_warm, nested_sum, nested_rows) = {
            let _force = ForceNestedJoin::new();
            time_agg(&mut database, &sql)
        };
        assert_eq!(hash_sum, nested_sum, "{kind} {shape} {users}x{orders}");
        assert_eq!(hash_rows, nested_rows, "{kind} {shape} {users}x{orders}");
        let groups = hash_rows;
        let acc_bytes = if aggs == 0 {
            0
        } else {
            groups.saturating_mul(
                std::mem::size_of::<Vec<Value>>()
                    .saturating_add(std::mem::size_of::<Value>())
                    .saturating_add(aggs.saturating_mul(std::mem::size_of::<AccValue>())),
            )
        };
        println!(
            "milestone20 kind={kind} shape={shape} users={users} orders={orders} access={access} examined={examined} nested_examined={} matched={matched} null_extended={extended} inputs={inputs} groups={groups} acc_bytes={acc_bytes} hash_cold_us={hash_cold} hash_warm_us={hash_warm} nested_cold_us={nested_cold} nested_warm_us={nested_warm} checksum={hash_sum}",
            users.saturating_mul(orders)
        );
    }

    fn profile_load_orders(database: &mut Database, rows: i64, shape: &str) {
        let mut start = 0_i64;
        while start < rows {
            let end = (start + 400).min(rows - 1);
            let mut sql = String::from("INSERT INTO orders VALUES ");
            for id in start..=end {
                if id != start {
                    sql.push(',');
                }
                let user_id = match shape {
                    "none" => -1,
                    "few" => 0,
                    "duplicates" => id % 10,
                    _ => id,
                };
                if shape == "nulls" {
                    sql.push_str(&format!("({user_id}, NULL)"));
                } else {
                    sql.push_str(&format!("({user_id}, {id})"));
                }
            }
            database.execute(&sql).unwrap();
            start = end + 1;
        }
    }

    fn time_agg(database: &mut Database, sql: &str) -> (u128, u128, i64, usize) {
        let mut samples = Vec::with_capacity(6);
        let mut checksum = 0_i64;
        let mut returned = 0_usize;
        for _ in 0..6 {
            let started = std::time::Instant::now();
            let result = query(database, sql);
            samples.push(started.elapsed().as_micros());
            returned = result.rows.len();
            checksum = profile_checksum(&result);
        }
        let cold = samples[0];
        let mut warm = samples[1..].to_vec();
        warm.sort_unstable();
        (cold, warm[warm.len() / 2], checksum, returned)
    }

    fn replace_cell(db: &mut Database, table: &str, row: usize, column: usize, value: Value) {
        let mut rows = db.table(table).unwrap().rows().to_vec();
        rows[row][column] = value;
        db.table_mut(table).unwrap().rewrite_all(rows).unwrap();
    }

    fn join_error(db: &mut Database, sql: &str) -> String {
        db.execute(sql).unwrap_err().to_string()
    }

    fn assert_join_error(db: &mut Database, sql: &str) {
        let hashed = join_error(db, sql);
        assert!(hashed.contains("different types"), "{sql}: {hashed}");
        let nested = {
            let _force = ForceNestedJoin::new();
            join_error(db, sql)
        };
        assert_eq!(hashed, nested, "{sql}");
        let collided = {
            let _force = ForceHashCollision::new();
            join_error(db, sql)
        };
        assert_eq!(collided, nested, "{sql}");
    }

    fn assert_join_rows(db: &mut Database, sql: &str, expected: &[Vec<Value>]) {
        let hashed = query(db, sql).rows;
        let nested = {
            let _force = ForceNestedJoin::new();
            query(db, sql).rows
        };
        let collided = {
            let _force = ForceHashCollision::new();
            query(db, sql).rows
        };
        assert_eq!(nested, expected, "{sql}");
        assert_eq!(hashed, expected, "{sql}");
        assert_eq!(collided, expected, "{sql}");
    }

    fn shop_users(orders: &str) -> Database {
        let mut db = Database::new();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea'), (3, 'Cy'), (NULL, 'Dee');")
            .unwrap();
        db.execute(orders).unwrap();
        db
    }

    #[test]
    fn valid_on_cells_stay_on_the_fast_path() {
        let on = bound_equality();
        let outer = vec![vec![Value::Integer(1)], vec![Value::Null]];
        let inner = vec![vec![Value::Integer(1)]];
        assert!(on_cells_allow_fast_join(
            &[DataType::Integer],
            &[DataType::Integer],
            &outer,
            &inner,
            &on
        ));
        let bad = vec![vec![Value::Text("later".to_string())]];
        assert!(!on_cells_allow_fast_join(
            &[DataType::Integer],
            &[DataType::Integer],
            &outer,
            &bad,
            &on
        ));
        let skipped = {
            let _skip = SkipOnSchemaCheck::new();
            on_cells_allow_fast_join(
                &[DataType::Integer],
                &[DataType::Integer],
                &outer,
                &bad,
                &on,
            )
        };
        assert!(skipped);
        assert!(!on_cells_allow_fast_join(
            &[DataType::Integer],
            &[DataType::Integer],
            &outer,
            &bad,
            &on
        ));
    }

    fn bound_equality() -> BoundExpr {
        let scope = Scope::try_join(
            "u",
            vec![("id".to_string(), DataType::Integer)],
            "o",
            vec![("user_id".to_string(), DataType::Integer)],
        )
        .unwrap();
        expr::bind_filter(
            &Expr::Binary {
                left: Box::new(Expr::Column {
                    name: "id".to_string(),
                    qualifier: Some("u".to_string()),
                }),
                op: crate::ast::BinOp::Eq,
                right: Box::new(Expr::Column {
                    name: "user_id".to_string(),
                    qualifier: Some("o".to_string()),
                }),
            },
            &scope,
            "ON",
        )
        .unwrap()
    }

    #[test]
    fn inner_hash_matches_nested_loop_for_valid_rows() {
        let mut db = shop_users(
            "INSERT INTO orders VALUES (1, 50), (1, 150), (1, NULL), (2, 10), (NULL, 5);",
        );
        let expected = vec![
            vec![Value::Text("Ada".to_string()), Value::Integer(50)],
            vec![Value::Text("Ada".to_string()), Value::Integer(150)],
            vec![Value::Text("Ada".to_string()), Value::Null],
            vec![Value::Text("Bea".to_string()), Value::Integer(10)],
        ];
        assert_join_rows(
            &mut db,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
            &expected,
        );
        assert_join_rows(
            &mut db,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON o.user_id = u.id;",
            &expected,
        );
        assert_join_rows(
            &mut db,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 100;",
            &[vec![Value::Text("Ada".to_string()), Value::Integer(150)]],
        );
        assert_join_rows(
            &mut db,
            "SELECT COUNT(*), COUNT(o.amount), SUM(o.amount), MIN(o.amount), MAX(o.amount) FROM users u JOIN orders o ON u.id = o.user_id;",
            &[vec![
                Value::Integer(4),
                Value::Integer(3),
                Value::Integer(210),
                Value::Integer(10),
                Value::Integer(150),
            ]],
        );
        let explained = query(
            &mut db,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
        assert_eq!(
            explained.rows[0][3],
            Value::Text("u (users)".to_string()),
            "five orders build the smaller outer table"
        );
        let mut fewer = shop_users("INSERT INTO orders VALUES (1, 50), (1, 150);");
        let explained = query(
            &mut fewer,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
        assert_eq!(explained.rows[0][3], Value::Text("o (orders)".to_string()));
        assert_join_rows(
            &mut fewer,
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
            &[
                vec![Value::Text("Ada".to_string()), Value::Integer(50)],
                vec![Value::Text("Ada".to_string()), Value::Integer(150)],
            ],
        );
        let mut empty = shop_users("INSERT INTO orders VALUES (1, 1);");
        empty.execute("DELETE FROM orders;").unwrap();
        empty.execute("DELETE FROM users;").unwrap();
        assert_join_rows(
            &mut empty,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            &[],
        );
        let explained = query(
            &mut empty,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
    }

    #[test]
    fn a_mistyped_inner_key_matches_the_nested_loop_error() {
        let sqls = [
            "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            "SELECT COUNT(*), SUM(o.amount), AVG(o.amount), MIN(o.amount), MAX(o.amount) FROM users u JOIN orders o ON u.id = o.user_id;",
            "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name HAVING COUNT(*) > 0;",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name ASC LIMIT 1;",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id LIMIT 0;",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id LIMIT 1;",
            "SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id;",
        ];
        let mut build_outer = shop_users(
            "INSERT INTO orders VALUES (1, 50), (1, 150), (1, NULL), (2, 10), (NULL, 5);",
        );
        replace_cell(
            &mut build_outer,
            "users",
            0,
            0,
            Value::Text("bad-build".to_string()),
        );
        for sql in sqls {
            assert_join_error(&mut build_outer, sql);
        }
        let explained = query(
            &mut build_outer,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
        replace_cell(&mut build_outer, "users", 0, 0, Value::Integer(1));
        assert_eq!(
            query(
                &mut build_outer,
                "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id;",
            )
            .rows,
            vec![vec![Value::Integer(4)]]
        );
        replace_cell(
            &mut build_outer,
            "orders",
            4,
            0,
            Value::Text("bad-probe".to_string()),
        );
        assert_join_error(
            &mut build_outer,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id ORDER BY u.name ASC LIMIT 1;",
        );

        let mut build_inner = shop_users("INSERT INTO orders VALUES (1, 50), (2, 10);");
        replace_cell(
            &mut build_inner,
            "orders",
            1,
            0,
            Value::Text("bad-probe".to_string()),
        );
        assert_join_error(
            &mut build_inner,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id LIMIT 1;",
        );
        replace_cell(&mut build_inner, "orders", 1, 0, Value::Integer(2));
        replace_cell(
            &mut build_inner,
            "users",
            3,
            0,
            Value::Text("bad-outer-probe".to_string()),
        );
        assert_join_error(
            &mut build_inner,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
    }

    #[test]
    fn a_mistyped_residual_is_not_skipped_by_a_hash_miss() {
        let mut db = shop_users("INSERT INTO orders VALUES (9, 10), (NULL, 5);");
        replace_cell(&mut db, "orders", 0, 1, Value::Text("later".to_string()));
        assert_join_error(
            &mut db,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
        );
        assert_join_error(
            &mut db,
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0 LIMIT 0;",
        );
        let mut matched = shop_users("INSERT INTO orders VALUES (1, 50);");
        replace_cell(
            &mut matched,
            "orders",
            0,
            1,
            Value::Text("later".to_string()),
        );
        assert_join_error(
            &mut matched,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0 ORDER BY u.name ASC LIMIT 1;",
        );
        let mut where_only = shop_users("INSERT INTO orders VALUES (9, 10);");
        replace_cell(
            &mut where_only,
            "orders",
            0,
            1,
            Value::Text("later".to_string()),
        );
        assert_join_rows(
            &mut where_only,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id WHERE o.amount > 0;",
            &[],
        );
        replace_cell(&mut where_only, "orders", 0, 0, Value::Integer(1));
        assert_join_error(
            &mut where_only,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id WHERE o.amount > 0;",
        );
    }

    #[test]
    fn a_mistyped_self_join_key_matches_the_nested_loop() {
        let mut db = Database::new();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
            .unwrap();
        replace_cell(&mut db, "users", 1, 0, Value::Text("later".to_string()));
        assert_join_error(
            &mut db,
            "SELECT a.name, b.name FROM users a JOIN users b ON a.id = b.id LIMIT 1;",
        );
    }

    #[test]
    fn an_empty_input_does_not_compare_a_mistyped_cell() {
        let mut db = shop_users("INSERT INTO orders VALUES (1, 50);");
        db.execute("DELETE FROM orders;").unwrap();
        replace_cell(&mut db, "users", 0, 0, Value::Text("later".to_string()));
        assert_join_rows(
            &mut db,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            &[],
        );
        let mut other = shop_users("INSERT INTO orders VALUES (1, 50);");
        other.execute("DELETE FROM users;").unwrap();
        replace_cell(&mut other, "orders", 0, 0, Value::Text("later".to_string()));
        assert_join_rows(
            &mut other,
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
            &[],
        );
    }

    #[test]
    fn a_failed_inner_join_does_not_publish_rows_or_write_the_file() {
        let dir = crate::test_dir::TestDir::new("m21-atomic");
        let path = dir.join("db");
        let wal = {
            let mut name = path.into_os_string();
            name.push(".wal");
            std::path::PathBuf::from(name)
        };
        let path = dir.join("db");
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id INT, name TEXT);")
            .unwrap();
        db.execute("CREATE TABLE orders (user_id INT, amount INT);")
            .unwrap();
        db.execute("CREATE TABLE nums (n INT);").unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Bea');")
            .unwrap();
        db.execute("INSERT INTO orders VALUES (1, 50), (1, 150);")
            .unwrap();
        db.execute("INSERT INTO nums VALUES (7);").unwrap();
        let before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(&wal).unwrap_or_default();
        replace_cell(&mut db, "orders", 1, 0, Value::Text("later".to_string()));
        let error = db
            .execute("SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id LIMIT 1;")
            .unwrap_err();
        assert!(error.to_string().contains("different types"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&wal).unwrap_or_default(), wal_before);
        let explained = query(
            &mut db,
            "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;",
        );
        assert_eq!(explained.rows[0][0], Value::Text("HashJoin".to_string()));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            query(&mut db, "SELECT n FROM nums;").rows,
            vec![vec![Value::Integer(7)]]
        );
        drop(db);
        let mut reopened = Database::open(&path).unwrap();
        assert_eq!(
            query(
                &mut reopened,
                "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.user_id;",
            )
            .rows,
            vec![
                vec![Value::Text("Ada".to_string()), Value::Integer(50)],
                vec![Value::Text("Ada".to_string()), Value::Integer(150)],
            ]
        );
    }

    fn indexed_pair(label: &str) -> (crate::test_dir::TestDir, Database) {
        let dir = crate::test_dir::TestDir::new(label);
        let path = dir.join("db");
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE users (id INT, name TEXT);")
                .unwrap();
            db.execute("CREATE TABLE orders (user_id INT, amount INT);")
                .unwrap();
            db.execute("INSERT INTO users VALUES (1, 'Ada'), (9, 'No'), (NULL, 'Dee');")
                .unwrap();
            db.execute("INSERT INTO orders VALUES (1, 50), (2, 10);")
                .unwrap();
        }
        Database::migrate(&path).unwrap();
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        (dir, db)
    }

    #[test]
    fn an_index_probe_does_not_skip_a_mistyped_residual() {
        let (_dir, mut db) = indexed_pair("m21-index-residual");
        replace_cell(&mut db, "orders", 1, 1, Value::Text("later".to_string()));
        let joins = [
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
            "SELECT DISTINCT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0 ORDER BY u.name ASC LIMIT 1;",
            "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0 LIMIT 0;",
        ];
        let aggregates = [
            "SELECT COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
            "SELECT u.name, COUNT(*) FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0 GROUP BY u.name;",
        ];
        for sql in joins.iter().copied().chain(aggregates) {
            let indexed = {
                let _force = ForceIndexJoin::new();
                join_error(&mut db, sql)
            };
            let nested = {
                let _force = ForceNestedJoin::new();
                join_error(&mut db, sql)
            };
            assert!(indexed.contains("different types"), "{sql}: {indexed}");
            assert_eq!(indexed, nested, "{sql}");
        }
        for sql in joins {
            let explained = {
                let _force = ForceIndexJoin::new();
                query(&mut db, &format!("EXPLAIN {sql}"))
            };
            assert_eq!(
                explained.rows[0][0],
                Value::Text("IndexNestedLoopJoin".to_string()),
                "{sql}"
            );
        }
        for sql in aggregates {
            let explained = {
                let _force = ForceIndexJoin::new();
                query(&mut db, &format!("EXPLAIN {sql}"))
            };
            assert_eq!(
                explained.rows[0][4],
                Value::Text("IndexNestedLoopJoin".to_string()),
                "{sql}"
            );
        }
        replace_cell(&mut db, "orders", 1, 1, Value::Integer(10));
        let restored = {
            let _force = ForceIndexJoin::new();
            query(
                &mut db,
                "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id AND o.amount > 0;",
            )
            .rows
        };
        assert_eq!(restored, vec![vec![Value::Text("Ada".to_string())]]);
    }

    #[test]
    fn an_index_probe_does_not_skip_a_mistyped_key() {
        let (_dir, mut db) = indexed_pair("m21-index-key");
        replace_cell(&mut db, "users", 1, 0, Value::Text("later".to_string()));
        let sql = "SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id;";
        let indexed = {
            let _force = ForceIndexJoin::new();
            join_error(&mut db, sql)
        };
        let nested = {
            let _force = ForceNestedJoin::new();
            join_error(&mut db, sql)
        };
        assert!(indexed.contains("different types"), "{indexed}");
        assert_eq!(indexed, nested);
    }

    #[test]
    #[ignore = "release profile of the INNER JOIN schema check"]
    fn profile_milestone21() {
        let cpu = profile_command("sysctl", &["-n", "machdep.cpu.brand_string"]);
        let mem = profile_command("sysctl", &["-n", "hw.memsize"]);
        let rustc = profile_command("rustc", &["--version"]);
        println!(
            "milestone21 cpu={cpu} mem_bytes={mem} rustc={rustc} value_bytes={} row_vec_bytes={}",
            std::mem::size_of::<Value>(),
            std::mem::size_of::<Vec<Value>>()
        );
        for rows in [100_i64, 1_000, 4_000, 10_000] {
            profile_guard_hash(rows, rows, rows, "equal");
            profile_guard_hash(rows, rows, 10, "duplicates");
            let small = (rows / 100).max(1);
            profile_guard_hash(small, rows, small, "build_outer");
            profile_guard_hash(rows, small, small, "build_inner");
            profile_guard_index(rows, rows, rows, "equal");
            profile_guard_index(rows, rows, 10, "duplicates");
        }
    }

    fn profile_guard_hash(users: i64, orders: i64, user_mod: i64, shape: &str) {
        let mut database = Database::new();
        load_profile_join(&mut database, users, orders, user_mod);
        profile_guard(
            &mut database,
            "HashJoin",
            users,
            orders,
            user_mod,
            shape,
            false,
        );
    }

    fn profile_guard_index(users: i64, orders: i64, user_mod: i64, shape: &str) {
        let dir = crate::test_dir::TestDir::new("profile-m21");
        let path = dir.join("db");
        {
            let mut database = Database::open(&path).unwrap();
            load_profile_join(&mut database, users, orders, user_mod);
        }
        Database::migrate(&path).unwrap();
        let mut database = Database::open(&path).unwrap();
        database
            .execute("CREATE INDEX idx_orders_user ON orders (user_id);")
            .unwrap();
        let _force = ForceIndexJoin::new();
        profile_guard(
            &mut database,
            "IndexNestedLoopJoin",
            users,
            orders,
            user_mod,
            shape,
            true,
        );
    }

    fn profile_guard(
        database: &mut Database,
        strategy: &str,
        users: i64,
        orders: i64,
        user_mod: i64,
        shape: &str,
        index: bool,
    ) {
        let sql = "SELECT u.id, o.amount FROM users u JOIN orders o ON u.id = o.user_id;";
        let explained = query(database, &format!("EXPLAIN {sql}"));
        assert_eq!(
            explained.rows[0][0],
            Value::Text(strategy.to_string()),
            "{shape} users={users} orders={orders}"
        );
        let (expect, hits) = guard_checksum(users, orders, user_mod);
        let (cold, warm, checksum, returned) = time_agg(database, sql);
        assert_eq!(checksum, expect, "{strategy} {shape}");
        assert_eq!(returned, hits);
        let (skip_cold, skip_warm, skip_checksum, _) = {
            let _skip = SkipOnSchemaCheck::new();
            time_agg(database, sql)
        };
        assert_eq!(skip_checksum, expect);
        let build_inner = orders <= users;
        let (groups, positions) = if index {
            (0, 0)
        } else if build_inner {
            let groups = usize::try_from(user_mod.min(users).min(orders)).unwrap_or(usize::MAX);
            (groups, usize::try_from(orders).unwrap_or(usize::MAX))
        } else {
            let groups = usize::try_from(users).unwrap_or(usize::MAX);
            (groups, groups)
        };
        let bytes = if index {
            0
        } else {
            hash_table_bytes(groups, positions)
        };
        let overhead = i128::from(warm as u64) - i128::from(skip_warm as u64);
        println!(
            "milestone21 strategy={strategy} shape={shape} users={users} orders={orders} hits={hits} groups={groups} positions={positions} hash_bytes={bytes} checksum={checksum} check_cold_us={cold} check_warm_us={warm} skip_cold_us={skip_cold} skip_warm_us={skip_warm} overhead_us={overhead}"
        );
    }

    fn guard_checksum(users: i64, orders: i64, user_mod: i64) -> (i64, usize) {
        let mut checksum = 0_i64;
        let mut hits = 0_usize;
        for id in 0..orders {
            let user = id % user_mod;
            if user >= users {
                continue;
            }
            hits += 1;
            checksum = checksum.wrapping_add(user);
            if id % 2 == 1 {
                checksum = checksum.wrapping_add(id % 5);
            }
        }
        (checksum, hits)
    }
}

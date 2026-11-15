//! Execute parsed statements against the catalog.
//!
//! Filters are bound and type-checked before any row is read. `UPDATE` and
//! `DELETE` build the replacement rows and install them only after every row
//! has been planned, so a failure leaves the table unchanged. A durable
//! database publishes that result only after the statement succeeds.
//!
//! `SELECT` applies `WHERE`, then `ORDER BY`, then `LIMIT`, then projection.
//! An aggregate query groups after `WHERE` and applies `HAVING` before the sort.
//! An indexed integer comparison on one table may supply candidate rows first.
//! The original `WHERE` is still applied to those candidates. Sort keys may name
//! columns that are not in the select list. Equal keys keep the order produced
//! above because the sort is stable. NULL is greater than every non-NULL value
//! unless the query overrides that with `NULLS FIRST` or `NULLS LAST`.
//! `UPDATE` and `DELETE` still read every row of one table.
//!
//! A `SELECT` may group rows from one table or from one `INNER JOIN`. `WHERE`
//! runs first. `COUNT(*)` counts every remaining row, `COUNT(column)` skips
//! NULL, and `SUM` adds non-NULL integers with checked arithmetic. An empty
//! input with no `GROUP BY` returns one row. `GROUP BY` keeps NULL keys in one
//! group and emits groups in first-seen order. A group hash mixes a type tag
//! with each key, and a shared fingerprint still compares the stored cells, so
//! unequal tuples stay apart. The same aggregate call shares one accumulator
//! across the select list, `HAVING`, and `ORDER BY`. `HAVING` then keeps a
//! group only when it evaluates to TRUE. `ORDER BY` and `LIMIT` run after
//! that. A joined aggregate consumes each kept pair as it is produced and does
//! not store the pair. When a hash join builds the outer table, it still
//! buffers match positions so that first-seen order stays outer-then-inner.
//!
//! An `INNER JOIN` walks the outer table in insertion order. For each outer
//! row it reads inner candidates, then keeps a pair only when `ON` is TRUE.
//! `WHERE` then runs on that pair. An indexed integer equality uses the index
//! when the outer table is empty or the inner table has more than 64 times as
//! many rows. Otherwise that equality builds a hash table. Each bucket keeps
//! one group per distinct key, so a fingerprint collision does not mix keys.
//! NULL keys are left out. The original `ON` still runs on each matching key.
//! Equal sizes build the inner table. Matches are emitted in outer insertion
//! order, then inner insertion order. Any other shape reads the inner table
//! from front to back. The catalog stores tables in one map, so a path that
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
    AggregateArg, AggregateCall, AggregateFunction, Assignment, Expr, OrderKey, OrderTarget,
    Projection, QueryFrom, SelectColumn, SelectItem, Statement, TableRef,
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
        if is_aggregate_query(tail.projection, tail.group_by, tail.having, tail.order_by) {
            return match from {
                QueryFrom::Table(source) => self.aggregate_table(source, tail),
                QueryFrom::Join { outer, inner, on } => self.aggregate_join(outer, inner, on, tail),
            };
        }
        match from {
            QueryFrom::Table(source) => self.query_table(source, tail),
            QueryFrom::Join { outer, inner, on } => self.query_join(outer, inner, on, tail),
        }
    }

    fn query_table(
        &mut self,
        source: &TableRef,
        tail: SelectTail<'_>,
    ) -> Result<StatementResult, DbError> {
        let mut prepared =
            self.prepare_select(source, tail.projection, tail.selection, tail.order_by)?;
        prepared.path = self.settle_path(prepared.path)?;
        let positions = match &prepared.path {
            AccessPath::SeqScan => None,
            AccessPath::IndexScan { .. } => {
                Some(self.index_positions(&source.name, &prepared.path)?)
            }
        };

        let table = self.table(&source.name)?;
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
                self.with_copied_side(outer, inner, |outer_rows, inner_rows, build_inner| {
                    visit_hash_pairs(
                        outer_rows,
                        inner_rows,
                        keys,
                        build_inner,
                        prepared,
                        |outer_row, inner_row| visit(outer_row, inner_row),
                    )
                })
            }
            FeedKind::Nested => self.with_copied_side(outer, inner, |outer_rows, inner_rows, _| {
                visit_nested_pairs(outer_rows, inner_rows, prepared, |outer_row, inner_row| {
                    visit(outer_row, inner_row)
                })
            }),
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
        };
        if is_aggregate_query(
            &query.projection,
            &query.group_by,
            query.having.as_ref(),
            &query.order_by,
        ) {
            return match &query.from {
                QueryFrom::Table(source) => self.explain_aggregate(source, tail),
                QueryFrom::Join { outer, inner, on } => {
                    self.explain_aggregate_join(outer, inner, on, tail)
                }
            };
        }
        match &query.from {
            QueryFrom::Table(source) => {
                let mut prepared = self.prepare_select(
                    source,
                    &query.projection,
                    query.selection.as_ref(),
                    &query.order_by,
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
            QueryFrom::Join { outer, inner, on } => {
                let mut prepared = self.prepare_join(
                    outer,
                    inner,
                    on,
                    &query.projection,
                    query.selection.as_ref(),
                    &query.order_by,
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

    fn prepare_select(
        &self,
        source: &TableRef,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
    ) -> Result<PreparedSelect, DbError> {
        let indexes = self.index_images().to_vec();
        let table = self.table(&source.name)?;
        let scope = Scope::from_table(table, &source.alias);
        let outputs = output_columns(&scope, projection)?;
        let filter = bind_optional_where(selection, &scope)?;
        let sort_keys = bind_order(&scope, order_by, &outputs)?;
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

    fn prepare_join(
        &self,
        outer: &TableRef,
        inner: &TableRef,
        on: &Expr,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
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
        let sort_keys = bind_order(&scope, order_by, &outputs)?;
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
        self.with_copied_side(outer, inner, |outer_rows, inner_rows, build_inner| {
            run_hash_join(outer_rows, inner_rows, keys, build_inner, prepared, limit)
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
    Sum(usize),
    Avg(usize),
    Min(usize),
    Max(usize),
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
                );
            }
            SelectItem::Aggregate { call, alias } => {
                note_explicit_alias(&mut explicit_aliases, alias.as_deref())?;
                let (slot, _) = bind_aggregate(call, scope, &mut aggs)?;
                aggregate_labels.push(call.render());
                let index = group_ordinals.len() + slot;
                outputs.push(
                    index,
                    alias.clone().unwrap_or_else(|| call.render()),
                    alias.is_some(),
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
                |column| grouped_column(scope, &group_ordinals, column),
                |call| {
                    let (slot, scalar) = bind_aggregate(call, scope, &mut aggs)?;
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
                let (slot, _) = bind_aggregate(call, scope, &mut aggs)?;
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
    Ok(AggPlan {
        group_ordinals,
        group_labels,
        aggs,
        outputs: outputs.indexes,
        output_names: outputs.names,
        aggregate_labels,
        sort_keys,
        filter,
        having,
        path,
        access: access.to_string(),
    })
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
        ]],
    }
}

fn text_list(labels: &[String]) -> Value {
    if labels.is_empty() {
        Value::Null
    } else {
        Value::Text(labels.join(", "))
    }
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
        Projection::All | Projection::QualifiedAll { .. } => false,
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
            AggregateArg::Star => (AggFn::CountStar, expr::ScalarType::Integer),
            AggregateArg::Column(column) => (
                AggFn::CountColumn(scope.resolve(column)?),
                expr::ScalarType::Integer,
            ),
        },
        AggregateFunction::Sum => (
            integer_aggregate("SUM", call, scope, AggFn::Sum)?,
            expr::ScalarType::Integer,
        ),
        AggregateFunction::Avg => (
            integer_aggregate("AVG", call, scope, AggFn::Avg)?,
            expr::ScalarType::Ratio,
        ),
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
        (AggFn::Sum(left), AggFn::Sum(right)) => left == right,
        (AggFn::Avg(left), AggFn::Avg(right)) => left == right,
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
        Self {
            plan,
            scalar,
            groups: Vec::new(),
            buckets: HashMap::new(),
        }
    }

    fn push(&mut self, row: InputRow<'_>) -> Result<(), DbError> {
        if let Some(acc) = &mut self.scalar {
            return update_acc(acc, row, &self.plan.aggs);
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
        return update_acc(&mut groups[slot].acc, row, &plan.aggs);
    }
    let mut key = Vec::with_capacity(plan.group_ordinals.len());
    for &ordinal in &plan.group_ordinals {
        key.push(cell(row, ordinal)?.clone());
    }
    let mut acc = fresh_acc(&plan.aggs);
    update_acc(&mut acc, row, &plan.aggs)?;
    let slot = groups.len();
    groups.push(AggGroup { key, acc });
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
            AggFn::CountStar | AggFn::CountColumn(_) => AccValue::Count(0),
            AggFn::Sum(_) => AccValue::Sum(None),
            AggFn::Avg(_) => AccValue::Avg { sum: 0, count: 0 },
            AggFn::Min(_) | AggFn::Max(_) => AccValue::Extreme(None),
        })
        .collect()
}

fn update_acc(acc: &mut [AccValue], row: InputRow<'_>, aggs: &[AggFn]) -> Result<(), DbError> {
    if acc.len() != aggs.len() {
        return Err(DbError::Internal(
            "aggregate accumulator has the wrong width".to_string(),
        ));
    }
    // Replacements stay in `pending` until every aggregate of this row succeeds.
    // A text extreme is cloned only when it actually changes. Dropping `pending`
    // leaves the committed slots untouched.
    let mut pending = Vec::new();
    for (slot, agg) in aggs.iter().enumerate() {
        if let Some(next) = next_acc(&acc[slot], row, agg)? {
            pending.push((slot, next));
        }
    }
    for (slot, next) in pending {
        acc[slot] = next;
    }
    Ok(())
}

fn next_acc(
    current: &AccValue,
    row: InputRow<'_>,
    agg: &AggFn,
) -> Result<Option<AccValue>, DbError> {
    match agg {
        AggFn::CountStar => Ok(Some(bumped_count(current, "COUNT")?)),
        AggFn::CountColumn(ordinal) => {
            if cell(row, *ordinal)?.is_null() {
                Ok(None)
            } else {
                Ok(Some(bumped_count(current, "COUNT")?))
            }
        }
        AggFn::Sum(ordinal) => next_sum(current, row, *ordinal),
        AggFn::Avg(ordinal) => next_avg(current, row, *ordinal),
        AggFn::Min(ordinal) => next_extreme(current, row, *ordinal, true),
        AggFn::Max(ordinal) => next_extreme(current, row, *ordinal, false),
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
}

impl OutputPlan {
    fn new() -> Self {
        Self {
            indexes: Vec::new(),
            names: Vec::new(),
            explicit: Vec::new(),
        }
    }

    fn push(&mut self, index: usize, name: String, explicit: bool) {
        self.indexes.push(index);
        self.names.push(name);
        self.explicit.push(explicit);
    }
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
                outputs.push(index, name.to_string(), false);
            }
        }
        Projection::QualifiedAll { qualifier } => {
            for index in scope.columns_of(qualifier)? {
                let Some(name) = scope.name_at(index) else {
                    return Err(DbError::Internal(
                        "select column is outside the table".to_string(),
                    ));
                };
                outputs.push(index, name.to_string(), false);
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
    if column.qualifier.is_none() {
        let explicit = output_hits(outputs, &column.name, true);
        if explicit.len() > 1 {
            return Err(ambiguous_column(&column.name));
        }
        if let [hit] = explicit.as_slice() {
            return Ok(outputs.indexes[*hit]);
        }
        let implicit = output_hits(outputs, &column.name, false);
        if implicit.len() > 1 {
            return Err(ambiguous_column(&column.name));
        }
        if let [hit] = implicit.as_slice() {
            return Ok(outputs.indexes[*hit]);
        }
    }
    fallback(column)
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
    static MATERIALIZE_AGGREGATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

#[cfg(test)]
struct ForceNestedJoin;

#[cfg(test)]
impl ForceNestedJoin {
    fn new() -> Self {
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

#[cfg(test)]
mod tests {
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
        let error = update_acc(
            &mut acc,
            InputRow::One(&row),
            &[AggFn::CountStar, AggFn::Sum(0), AggFn::Sum(1)],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert!(matches!(acc[0], AccValue::Count(3)));
        assert!(matches!(acc[1], AccValue::Sum(Some(value)) if value == i64::MAX - 1));
        assert!(matches!(acc[2], AccValue::Sum(Some(4))));

        update_acc(
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
        let error = update_acc(
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
        update_acc(
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
        let error = update_acc(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert_eq!(acc, original);

        acc[2] = AccValue::Sum(Some(1));
        acc[4] = AccValue::Count(i64::MAX);
        let before_count = acc.clone();
        let error = update_acc(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "COUNT overflows INTEGER");
        assert_eq!(acc, before_count);

        acc[3] = AccValue::Avg {
            sum: 0,
            count: i64::MAX,
        };
        acc[4] = AccValue::Count(1);
        let before_avg = acc.clone();
        let error = update_acc(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows INTEGER");
        assert_eq!(acc, before_avg);

        acc[3] = AccValue::Avg {
            sum: i128::MAX,
            count: 1,
        };
        let before_sum = acc.clone();
        let error = update_acc(&mut acc, InputRow::One(&row), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "AVG overflows");
        assert_eq!(acc, before_sum);

        acc[2] = AccValue::Sum(Some(i64::MAX));
        acc[3] = AccValue::Avg { sum: 1, count: 1 };
        let nulls = [Value::Null, Value::Null, Value::Integer(1)];
        let before_null = acc.clone();
        let error = update_acc(&mut acc, InputRow::One(&nulls), &aggs).unwrap_err();
        assert_eq!(error.to_string(), "SUM overflows INTEGER");
        assert_eq!(acc, before_null);

        acc[2] = AccValue::Sum(Some(1));
        update_acc(
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
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: None,
            }),
        };
        let qualified = AggregateCall {
            function: AggregateFunction::Sum,
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
            argument: AggregateArg::Star,
        };
        assert_eq!(bind_aggregate(&count, &scope, &mut aggs).unwrap().0, 1);
        assert_eq!(aggs.len(), 2);
        let avg = AggregateCall {
            function: AggregateFunction::Avg,
            argument: AggregateArg::Column(SelectColumn {
                name: "n".to_string(),
                qualifier: Some("items".to_string()),
            }),
        };
        let again = AggregateCall {
            function: AggregateFunction::Avg,
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
}

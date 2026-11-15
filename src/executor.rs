//! Execute parsed statements against the catalog.
//!
//! Filters are bound and type-checked before any row is read. `UPDATE` and
//! `DELETE` build the replacement rows and install them only after every row
//! has been planned, so a failure leaves the table unchanged. A durable
//! database publishes that result only after the statement succeeds.
//!
//! `SELECT` applies `WHERE`, then `ORDER BY`, then `LIMIT`, then projection.
//! An indexed integer comparison on one table may supply candidate rows first.
//! The original `WHERE` is still applied to those candidates. Sort keys may name
//! columns that are not in the select list. Equal keys keep the order produced
//! above because the sort is stable. NULL is greater than every non-NULL value
//! unless the query overrides that with `NULLS FIRST` or `NULLS LAST`.
//! `UPDATE` and `DELETE` still read every row of one table.
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

use crate::ast::{Assignment, Expr, OrderKey, Projection, QueryFrom, Statement, TableRef};
use crate::database::{Database, DurableChange};
use crate::error::DbError;
use crate::expr::{self, BoundExpr, Scope};
use crate::parser;
use crate::plan::{self, AccessPath, JoinAccess};
use crate::storage::IndexImage;
use crate::table::Table;
use crate::value::{DataType, Value};

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

struct SelectTail<'a> {
    projection: &'a Projection,
    selection: Option<&'a Expr>,
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
            Statement::Select {
                from,
                projection,
                selection,
                order_by,
                limit,
            } => self.query(&from, &projection, selection.as_ref(), &order_by, limit),
            Statement::Explain(inner) => self.explain(inner.as_ref()),
        }
    }

    fn query(
        &mut self,
        from: &QueryFrom,
        projection: &Projection,
        selection: Option<&Expr>,
        order_by: &[OrderKey],
        limit: Option<u64>,
    ) -> Result<StatementResult, DbError> {
        match from {
            QueryFrom::Table(source) => self.query_table(
                source,
                SelectTail {
                    projection,
                    selection,
                    order_by,
                    limit,
                },
            ),
            QueryFrom::Join { outer, inner, on } => self.query_join(
                outer,
                inner,
                on,
                SelectTail {
                    projection,
                    selection,
                    order_by,
                    limit,
                },
            ),
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
                // `tables` is one map, so two row slices cannot be borrowed together.
                // A self-join is the same entry. Copy the smaller input and borrow the
                // other. The copy is not updated: a join does not write either table.
                let outer_len = self.table(&outer.name)?.rows().len();
                let inner_len = self.table(&inner.name)?.rows().len();
                let copy_inner = outer.name == inner.name || inner_len <= outer_len;
                if copy_inner {
                    let inner_rows = self.table(&inner.name)?.rows().to_vec();
                    let outer_rows = self.table(&outer.name)?.rows();
                    scan_join(outer_rows, &inner_rows, prepared, tail.limit)
                } else {
                    let outer_rows = self.table(&outer.name)?.rows().to_vec();
                    let inner_rows = self.table(&inner.name)?.rows();
                    scan_join(&outer_rows, inner_rows, prepared, tail.limit)
                }
            }
        }
    }

    fn explain(&mut self, statement: &Statement) -> Result<StatementResult, DbError> {
        let Statement::Select {
            from,
            projection,
            selection,
            order_by,
            ..
        } = statement
        else {
            return Err(DbError::unsupported("EXPLAIN is only supported for SELECT"));
        };
        match from {
            QueryFrom::Table(source) => {
                let mut prepared =
                    self.prepare_select(source, projection, selection.as_ref(), order_by)?;
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
                let mut prepared =
                    self.prepare_join(outer, inner, on, projection, selection.as_ref(), order_by)?;
                let outer_len = self.table(&outer.name)?.rows().len();
                let inner_len = self.table(&inner.name)?.rows().len();
                prepared.path = finish_join_path(prepared.path, outer_len, inner_len);
                let filter = selection
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
        let output_indexes = output_columns(&scope, projection)?;
        let output_names = column_names(&scope, &output_indexes)?;
        let filter = bind_optional_where(selection, &scope)?;
        let sort_keys = bind_order(&scope, order_by)?;
        let choices = index_choices(table, &indexes)?;
        let path = plan::choose(&source.alias, selection, &choices);
        Ok(PreparedSelect {
            output_indexes,
            output_names,
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
        let sort_keys = bind_order(&scope, order_by)?;
        let output_indexes = output_columns(&scope, projection)?;
        let output_names = column_names(&scope, &output_indexes)?;
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
            output_indexes,
            output_names,
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
            for outer_row in outer_rows {
                self.visit_inner_matches(inner_name, probe, outer_row, &prepared, |inner_row| {
                    if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                        return Ok(());
                    }
                    projected.push(project_pair(
                        outer_row,
                        inner_row,
                        &prepared.output_indexes,
                    )?);
                    Ok(())
                })?;
            }
            return Ok(StatementResult::Query(QueryResult {
                columns: prepared.output_names,
                rows: projected,
            }));
        }

        let mut joined = Vec::new();
        for outer_row in outer_rows {
            self.visit_inner_matches(inner_name, probe, outer_row, &prepared, |inner_row| {
                let mut row = Vec::with_capacity(outer_row.len().saturating_add(inner_row.len()));
                row.extend_from_slice(outer_row);
                row.extend_from_slice(inner_row);
                joined.push(row);
                Ok(())
            })?;
        }
        finish_rows(
            joined,
            &prepared.sort_keys,
            limit,
            &prepared.output_indexes,
            prepared.output_names,
        )
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
        if outer.name == inner.name {
            let rows = self.table(&outer.name)?.rows().to_vec();
            return run_hash_join(&rows, &rows, keys, true, prepared, limit);
        }
        let outer_len = self.table(&outer.name)?.rows().len();
        let inner_len = self.table(&inner.name)?.rows().len();
        // Equal sizes build the inner table. The probe then walks the outer
        // table and emits each bucket in insertion order.
        if inner_len <= outer_len {
            let inner_rows = self.table(&inner.name)?.rows().to_vec();
            let outer_rows = self.table(&outer.name)?.rows();
            run_hash_join(outer_rows, &inner_rows, keys, true, prepared, limit)
        } else {
            let outer_rows = self.table(&outer.name)?.rows().to_vec();
            let inner_rows = self.table(&inner.name)?.rows();
            run_hash_join(&outer_rows, inner_rows, keys, false, prepared, limit)
        }
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
        Statement::Select { .. } | Statement::Explain(_) => DurableChange::None,
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

fn bind_optional_where(
    selection: Option<&Expr>,
    scope: &Scope,
) -> Result<Option<BoundExpr>, DbError> {
    selection
        .map(|expr| expr::bind_where(expr, scope))
        .transpose()
}

fn output_columns(scope: &Scope, projection: &Projection) -> Result<Vec<usize>, DbError> {
    match projection {
        Projection::All => Ok((0..scope.len()).collect()),
        Projection::QualifiedAll { qualifier } => scope.columns_of(qualifier),
        Projection::Columns(columns) => {
            columns.iter().map(|column| scope.resolve(column)).collect()
        }
    }
}

fn column_names(scope: &Scope, indexes: &[usize]) -> Result<Vec<String>, DbError> {
    let mut names = Vec::with_capacity(indexes.len());
    for index in indexes {
        let Some(name) = scope.name_at(*index) else {
            return Err(DbError::Internal(format!(
                "output column index {index} is outside the row"
            )));
        };
        names.push(name.to_string());
    }
    Ok(names)
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
        *slot = value;
    }
    Ok(updated)
}

fn bind_order(scope: &Scope, keys: &[OrderKey]) -> Result<Vec<SortKey>, DbError> {
    let mut bound = Vec::with_capacity(keys.len());
    for key in keys {
        let index = scope.resolve(&key.column)?;
        bound.push(SortKey {
            index,
            ascending: key.ascending,
            nulls_first: key.nulls_first.unwrap_or(!key.ascending),
        });
    }
    Ok(bound)
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
    if build_inner {
        let table = build_hash(inner_rows, keys.inner_ordinal, hash_key)?;
        if prepared.sort_keys.is_empty() {
            let mut projected = Vec::new();
            for outer_row in outer_rows {
                visit_hash_hits(
                    outer_row,
                    inner_rows,
                    keys.outer_ordinal,
                    &table,
                    keys.inner_ordinal,
                    &prepared,
                    |inner_row| {
                        if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                            return Ok(());
                        }
                        projected.push(project_pair(
                            outer_row,
                            inner_row,
                            &prepared.output_indexes,
                        )?);
                        Ok(())
                    },
                )?;
            }
            return Ok(StatementResult::Query(QueryResult {
                columns: prepared.output_names,
                rows: projected,
            }));
        }
        let mut joined = Vec::new();
        for outer_row in outer_rows {
            visit_hash_hits(
                outer_row,
                inner_rows,
                keys.outer_ordinal,
                &table,
                keys.inner_ordinal,
                &prepared,
                |inner_row| {
                    joined.push(concat_pair(outer_row, inner_row));
                    Ok(())
                },
            )?;
        }
        return finish_rows(
            joined,
            &prepared.sort_keys,
            limit,
            &prepared.output_indexes,
            prepared.output_names,
        );
    }

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
            if !pair_kept(&prepared, outer_row, inner_row)? {
                continue;
            }
            pairs.push((outer_pos, inner_pos));
        }
    }
    pairs.sort_unstable();
    if prepared.sort_keys.is_empty() {
        if let Some(limit_n) = limit_n
            && pairs.len() > limit_n
        {
            pairs.truncate(limit_n);
        }
        let mut projected = Vec::with_capacity(pairs.len());
        for (outer_pos, inner_pos) in pairs {
            projected.push(project_pair(
                &outer_rows[outer_pos],
                &inner_rows[inner_pos],
                &prepared.output_indexes,
            )?);
        }
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }
    let mut joined = Vec::with_capacity(pairs.len());
    for (outer_pos, inner_pos) in pairs {
        joined.push(concat_pair(&outer_rows[outer_pos], &inner_rows[inner_pos]));
    }
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
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
        for outer_row in outer_rows {
            for inner_row in inner_rows {
                if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
                    continue;
                }
                if !expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)? {
                    continue;
                }
                if limit_n.is_some_and(|limit_n| projected.len() >= limit_n) {
                    continue;
                }
                projected.push(project_pair(
                    outer_row,
                    inner_row,
                    &prepared.output_indexes,
                )?);
            }
        }
        return Ok(StatementResult::Query(QueryResult {
            columns: prepared.output_names,
            rows: projected,
        }));
    }

    let mut joined = Vec::new();
    for outer_row in outer_rows {
        for inner_row in inner_rows {
            if !expr::pair_matches(Some(&prepared.on), outer_row, inner_row)? {
                continue;
            }
            if !expr::pair_matches(prepared.filter.as_ref(), outer_row, inner_row)? {
                continue;
            }
            let mut row = Vec::with_capacity(outer_row.len().saturating_add(inner_row.len()));
            row.extend_from_slice(outer_row);
            row.extend_from_slice(inner_row);
            joined.push(row);
        }
    }
    finish_rows(
        joined,
        &prepared.sort_keys,
        limit,
        &prepared.output_indexes,
        prepared.output_names,
    )
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
            let ordering = match (left, right) {
                (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
                (Value::Text(left), Value::Text(right)) => left.cmp(right),
                (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
                _ => Ordering::Equal,
            };
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
}

#[cfg(test)]
fn force_nested_join() -> bool {
    FORCE_NESTED_JOIN.with(std::cell::Cell::get)
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
}

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
//! An `INNER JOIN` walks the outer table in insertion order and, for each outer
//! row, the inner table in insertion order. The catalog stores tables in one
//! map, so the scan copies the smaller input and borrows the other. A self-join
//! copies one side for the same reason. A pair is kept only when `ON` is TRUE.
//! `WHERE` then runs on that pair. Pairs that fail are not concatenated. With
//! no `ORDER BY`, a match is projected immediately and `LIMIT` stores only the
//! rows it will return; later pairs are still evaluated. `ORDER BY` keeps the
//! joined row until the sort. The join does not read an index and does not
//! write the database.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::ast::{Assignment, Expr, OrderKey, Projection, QueryFrom, Statement, TableRef};
use crate::database::{Database, DurableChange};
use crate::error::DbError;
use crate::expr::{self, BoundExpr, Scope};
use crate::parser;
use crate::plan::{self, AccessPath};
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
                self.prepare_join(outer, inner, on, projection, selection.as_ref(), order_by)?;
                let filter = selection
                    .as_ref()
                    .map(|expr| Value::Text(expr.render()))
                    .unwrap_or(Value::Null);
                Ok(StatementResult::Query(QueryResult {
                    columns: vec![
                        "access".to_string(),
                        "outer".to_string(),
                        "inner".to_string(),
                        "condition".to_string(),
                        "filter".to_string(),
                    ],
                    rows: vec![vec![
                        Value::Text("NestedLoopJoin".to_string()),
                        Value::Text(outer.explain_name()),
                        Value::Text(inner.explain_name()),
                        Value::Text(on.render()),
                        filter,
                    ]],
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
        let scope = Scope::try_join(&outer.alias, outer_columns, &inner.alias, inner_columns)?;
        let on = expr::bind_filter(on, &scope, "ON")?;
        let filter = bind_optional_where(selection, &scope)?;
        let sort_keys = bind_order(&scope, order_by)?;
        let output_indexes = output_columns(&scope, projection)?;
        let output_names = column_names(&scope, &output_indexes)?;
        Ok(PreparedJoin {
            on,
            filter,
            sort_keys,
            output_indexes,
            output_names,
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
        let (root, entries) = self.read_indexed_keys(&index_name, equal, low, high)?;

        let table = self.table(table_name)?;
        if root == 0 {
            let stored = table
                .rows()
                .iter()
                .any(|row| matches!(row.get(ordinal), Some(Value::Integer(_))));
            if stored {
                return Err(corrupt_index(&index_name));
            }
            return Ok(Vec::new());
        }

        let mut positions = Vec::with_capacity(entries.len());
        let mut seen = HashSet::with_capacity(entries.len());
        for (key, row_id) in entries {
            if !seen.insert(row_id) {
                return Err(corrupt_index(&index_name));
            }
            let Some(position) = table.position(row_id) else {
                return Err(corrupt_index(&index_name));
            };
            if table.row_id_at(position) != Some(row_id) {
                return Err(corrupt_index(&index_name));
            }
            match table.rows().get(position).and_then(|row| row.get(ordinal)) {
                Some(Value::Integer(stored)) if *stored == key => positions.push(position),
                _ => return Err(corrupt_index(&index_name)),
            }
        }
        positions.sort_unstable();
        Ok(positions)
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
}

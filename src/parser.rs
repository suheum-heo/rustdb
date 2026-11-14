//! Lower `sqlparser` output into the engine's statement tree.
//!
//! Unquoted identifiers are folded to lowercase, matching the usual SQL rule
//! for case-insensitive names. Quoted identifiers keep their case and are
//! matched exactly. Unsupported clauses are rejected here so execution cannot
//! silently ignore `ORDER BY`, constraints, or joins.

use sqlparser::ast::{
    AssignmentTarget, BinaryOperator, CreateTable, CreateTableOptions, DataType as SqlDataType,
    Delete as SqlDelete, Expr as SqlExpr, FromTable, GroupByExpr, HiveDistributionStyle, Ident,
    LimitClause, ObjectName, ObjectType, OrderBy, OrderByKind, OrderBySort, Query, Select,
    SelectFlavor, SelectItem, SetExpr, Statement as SqlStatement, TableFactor, UnaryOperator,
    Update as SqlUpdate, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::ast::{Assignment, BinOp, Expr, OrderKey, Projection, SelectColumn, Statement};
use crate::error::DbError;
use crate::schema::Column;
use crate::value::{DataType, Value};

/// Parse `sql` into statements this engine can execute.
pub(crate) fn parse(sql: &str) -> Result<Vec<Statement>, DbError> {
    if sql.trim().is_empty() {
        return Ok(Vec::new());
    }

    let dialect = GenericDialect {};
    let parsed =
        Parser::parse_sql(&dialect, sql).map_err(|error| DbError::Parse(error.to_string()))?;
    parsed.into_iter().map(convert_statement).collect()
}

fn convert_statement(statement: SqlStatement) -> Result<Statement, DbError> {
    match statement {
        SqlStatement::CreateTable(create) => convert_create(create),
        SqlStatement::CreateIndex(create) => convert_create_index(create),
        SqlStatement::Insert(insert) => convert_insert(insert),
        SqlStatement::Update(update) => convert_update(update),
        SqlStatement::Delete(delete) => convert_delete(delete),
        SqlStatement::Query(query) => convert_query(*query),
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            cascade,
            restrict,
            purge,
            temporary,
            table,
            ..
        } => convert_drop(DropTable {
            object_type,
            if_exists,
            names,
            cascade,
            restrict,
            purge,
            temporary,
            table,
        }),
        other => Err(DbError::unsupported(format!(
            "{} is not supported",
            statement_label(&other)
        ))),
    }
}

fn statement_label(statement: &SqlStatement) -> &'static str {
    match statement {
        SqlStatement::Update(_) => "UPDATE",
        SqlStatement::Delete(_) => "DELETE",
        SqlStatement::CreateIndex(_) => "CREATE INDEX",
        SqlStatement::AlterTable(_) => "ALTER TABLE",
        SqlStatement::StartTransaction { .. } => "START TRANSACTION",
        SqlStatement::Commit { .. } => "COMMIT",
        SqlStatement::Rollback { .. } => "ROLLBACK",
        SqlStatement::Explain { .. } | SqlStatement::ExplainTable { .. } => "EXPLAIN",
        SqlStatement::Truncate(_) => "TRUNCATE",
        _ => "this statement",
    }
}

fn convert_create(create: CreateTable) -> Result<Statement, DbError> {
    reject_extended_create(&create)?;
    let name = object_name(&create.name)?;
    if create.columns.is_empty() {
        return Err(DbError::invalid(
            "CREATE TABLE requires at least one column",
        ));
    }

    let mut columns = Vec::with_capacity(create.columns.len());
    for definition in &create.columns {
        if !definition.options.is_empty() {
            return Err(DbError::unsupported(
                "column constraints and defaults are not supported",
            ));
        }
        columns.push(Column::new(
            normalize_ident(&definition.name),
            convert_data_type(&definition.data_type)?,
        ));
    }

    Ok(Statement::CreateTable { name, columns })
}

fn reject_extended_create(create: &CreateTable) -> Result<(), DbError> {
    if create.if_not_exists {
        return Err(DbError::unsupported(
            "CREATE TABLE IF NOT EXISTS is not supported",
        ));
    }
    if create.or_replace {
        return Err(DbError::unsupported(
            "CREATE OR REPLACE TABLE is not supported",
        ));
    }
    if create.temporary || create.transient || create.volatile {
        return Err(DbError::unsupported("TEMPORARY tables are not supported"));
    }
    if !create.constraints.is_empty() || create.primary_key.is_some() {
        return Err(DbError::unsupported("table constraints are not supported"));
    }
    if create.query.is_some() {
        return Err(DbError::unsupported("CREATE TABLE AS is not supported"));
    }
    if create.like.is_some() {
        return Err(DbError::unsupported("CREATE TABLE LIKE is not supported"));
    }
    if create.clone.is_some() {
        return Err(DbError::unsupported("CREATE TABLE CLONE is not supported"));
    }
    if has_other_create_clauses(create) {
        return Err(DbError::unsupported(
            "this CREATE TABLE clause is not supported",
        ));
    }
    Ok(())
}

fn has_other_create_clauses(create: &CreateTable) -> bool {
    create.unlogged
        || create.external
        || create.dynamic
        || create.global.is_some()
        || create.iceberg
        || create.snapshot
        || !matches!(create.hive_distribution, HiveDistributionStyle::NONE)
        || create.hive_formats.is_some()
        || !matches!(create.table_options, CreateTableOptions::None)
        || create.file_format.is_some()
        || create.location.is_some()
        || create.version.is_some()
        || create.comment.is_some()
        || create.on_commit.is_some()
        || create.on_cluster.is_some()
        || create.order_by.is_some()
        || create.partition_by.is_some()
        || create.cluster_by.is_some()
        || create.clustered_by.is_some()
        || create.inherits.is_some()
        || create.partition_of.is_some()
        || create.for_values.is_some()
        || create.copy_grants
        || create.enable_schema_evolution.is_some()
        || create.change_tracking.is_some()
        || create.data_retention_time_in_days.is_some()
        || create.max_data_extension_time_in_days.is_some()
        || create.default_ddl_collation.is_some()
        || create.with_aggregation_policy.is_some()
        || create.with_row_access_policy.is_some()
        || create.with_storage_lifecycle_policy.is_some()
        || create.with_tags.is_some()
        || create.external_volume.is_some()
        || create.with_connection.is_some()
        || create.base_location.is_some()
        || create.catalog.is_some()
        || create.catalog_sync.is_some()
        || create.storage_serialization_policy.is_some()
        || create.target_lag.is_some()
        || create.warehouse.is_some()
        || create.refresh_mode.is_some()
        || create.initialize.is_some()
        || create.require_user
        || create.diststyle.is_some()
        || create.distkey.is_some()
        || create.sortkey.is_some()
        || create.backup.is_some()
        || create.multiset.is_some()
        || create.fallback.is_some()
        || create.with_data.is_some()
        || !create.sqlite_table_options.options.is_empty()
}

fn convert_data_type(data_type: &SqlDataType) -> Result<DataType, DbError> {
    match data_type {
        SqlDataType::Int(_)
        | SqlDataType::Integer(_)
        | SqlDataType::BigInt(_)
        | SqlDataType::SmallInt(_)
        | SqlDataType::TinyInt(_)
        | SqlDataType::MediumInt(_)
        | SqlDataType::Int4(_)
        | SqlDataType::Int8(_)
        | SqlDataType::Int16
        | SqlDataType::Int32
        | SqlDataType::Int64 => Ok(DataType::Integer),
        SqlDataType::Text
        | SqlDataType::String(_)
        | SqlDataType::Varchar(_)
        | SqlDataType::Char(_)
        | SqlDataType::Character(_)
        | SqlDataType::CharacterVarying(_)
        | SqlDataType::CharVarying(_) => Ok(DataType::Text),
        SqlDataType::Boolean | SqlDataType::Bool => Ok(DataType::Boolean),
        other => Err(DbError::unsupported(format!(
            "data type {other} is not supported"
        ))),
    }
}

fn convert_create_index(create: sqlparser::ast::CreateIndex) -> Result<Statement, DbError> {
    if create.unique {
        return Err(DbError::unsupported("UNIQUE indexes are not supported"));
    }
    if create.if_not_exists || create.concurrently || create.r#async {
        return Err(DbError::unsupported(
            "this CREATE INDEX clause is not supported",
        ));
    }
    if create.using.is_some()
        || !create.include.is_empty()
        || create.nulls_distinct.is_some()
        || !create.with.is_empty()
        || create.predicate.is_some()
        || !create.index_options.is_empty()
        || !create.alter_options.is_empty()
    {
        return Err(DbError::unsupported(
            "this CREATE INDEX clause is not supported",
        ));
    }
    let Some(name) = create.name else {
        return Err(DbError::invalid("CREATE INDEX requires an index name"));
    };
    if create.columns.len() != 1 {
        return Err(DbError::unsupported("indexes must name exactly one column"));
    }
    let column = &create.columns[0];
    if column.operator_class.is_some() || column.column.with_fill.is_some() {
        return Err(DbError::unsupported(
            "this CREATE INDEX clause is not supported",
        ));
    }
    if !matches!(
        column.column.options.sort,
        None | Some(sqlparser::ast::OrderBySort::Asc)
    ) || column.column.options.nulls_first.is_some()
    {
        return Err(DbError::unsupported(
            "index sort order and NULLS ordering are not supported",
        ));
    }
    let sqlparser::ast::Expr::Identifier(ident) = &column.column.expr else {
        return Err(DbError::unsupported("index expressions are not supported"));
    };
    Ok(Statement::CreateIndex {
        name: object_name(&name)?,
        table: object_name(&create.table_name)?,
        column: normalize_ident(ident),
    })
}

fn convert_drop(drop: DropTable) -> Result<Statement, DbError> {
    if drop.object_type == ObjectType::Index {
        return convert_drop_index(drop);
    }
    if drop.object_type != ObjectType::Table {
        return Err(DbError::unsupported(format!(
            "DROP {} is not supported",
            drop.object_type
        )));
    }
    if drop.if_exists {
        return Err(DbError::unsupported(
            "DROP TABLE IF EXISTS is not supported",
        ));
    }
    if drop.cascade || drop.restrict {
        return Err(DbError::unsupported(
            "CASCADE and RESTRICT are not supported",
        ));
    }
    if drop.purge || drop.temporary || drop.table.is_some() {
        return Err(DbError::unsupported(
            "this DROP TABLE clause is not supported",
        ));
    }
    if drop.names.is_empty() {
        return Err(DbError::invalid("DROP TABLE requires a table name"));
    }

    let mut converted = Vec::with_capacity(drop.names.len());
    for name in &drop.names {
        converted.push(object_name(name)?);
    }
    Ok(Statement::DropTable { names: converted })
}

fn convert_drop_index(drop: DropTable) -> Result<Statement, DbError> {
    if drop.if_exists {
        return Err(DbError::unsupported(
            "DROP INDEX IF EXISTS is not supported",
        ));
    }
    if drop.cascade || drop.restrict || drop.purge || drop.temporary || drop.table.is_some() {
        return Err(DbError::unsupported(
            "this DROP INDEX clause is not supported",
        ));
    }
    if drop.names.is_empty() {
        return Err(DbError::invalid("DROP INDEX requires an index name"));
    }
    let mut names = Vec::with_capacity(drop.names.len());
    for name in &drop.names {
        names.push(object_name(name)?);
    }
    Ok(Statement::DropIndex { names })
}

struct DropTable {
    object_type: ObjectType,
    if_exists: bool,
    names: Vec<ObjectName>,
    cascade: bool,
    restrict: bool,
    purge: bool,
    temporary: bool,
    table: Option<ObjectName>,
}

fn convert_insert(insert: sqlparser::ast::Insert) -> Result<Statement, DbError> {
    reject_extended_insert(&insert)?;
    let table = match &insert.table {
        sqlparser::ast::TableObject::TableName(name) => object_name(name)?,
        _ => return Err(DbError::unsupported("this INSERT target is not supported")),
    };

    let mut columns = Vec::with_capacity(insert.columns.len());
    for column in &insert.columns {
        columns.push(object_name(column)?);
    }

    let Some(source) = insert.source else {
        return Err(DbError::invalid("INSERT requires a VALUES clause"));
    };
    let rows = convert_values(&source)?;
    Ok(Statement::Insert {
        table,
        columns,
        rows,
    })
}

fn reject_extended_insert(insert: &sqlparser::ast::Insert) -> Result<(), DbError> {
    if insert.replace_into {
        return Err(DbError::unsupported("REPLACE INTO is not supported"));
    }
    if insert.or.is_some() {
        return Err(DbError::unsupported("INSERT OR is not supported"));
    }
    if insert.ignore {
        return Err(DbError::unsupported("INSERT IGNORE is not supported"));
    }
    if insert.overwrite {
        return Err(DbError::unsupported("INSERT OVERWRITE is not supported"));
    }
    if insert.by_name {
        return Err(DbError::unsupported("INSERT BY NAME is not supported"));
    }
    if !insert.assignments.is_empty() {
        return Err(DbError::unsupported("INSERT ... SET is not supported"));
    }
    if insert.returning.is_some() || insert.output.is_some() {
        return Err(DbError::unsupported(
            "INSERT ... RETURNING is not supported",
        ));
    }
    if insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.table_alias.is_some()
        || !insert.optimizer_hints.is_empty()
        || insert.overriding.is_some()
        || insert.on.is_some()
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return Err(DbError::unsupported("this INSERT clause is not supported"));
    }
    Ok(())
}

fn convert_values(query: &Query) -> Result<Vec<Vec<Value>>, DbError> {
    reject_common_query_clauses(query)?;
    if query.order_by.is_some() {
        return Err(DbError::unsupported("ORDER BY on INSERT is not supported"));
    }
    if query.limit_clause.is_some() || query.fetch.is_some() {
        return Err(DbError::unsupported("LIMIT on INSERT is not supported"));
    }
    let SetExpr::Values(values) = query.body.as_ref() else {
        return Err(DbError::unsupported("INSERT ... SELECT is not supported"));
    };
    if values.rows.is_empty() {
        return Err(DbError::invalid("INSERT requires at least one row"));
    }

    let mut rows = Vec::with_capacity(values.rows.len());
    for row in &values.rows {
        let mut converted = Vec::with_capacity(row.content.len());
        for expr in &row.content {
            converted.push(expect_literal(convert_expr(expr)?)?);
        }
        rows.push(converted);
    }
    Ok(rows)
}

fn expect_literal(expr: Expr) -> Result<Value, DbError> {
    match expr {
        Expr::Literal(value) => Ok(value),
        _ => Err(DbError::unsupported(
            "expressions in VALUES are not supported",
        )),
    }
}

fn convert_query(query: Query) -> Result<Statement, DbError> {
    reject_common_query_clauses(&query)?;
    if query.fetch.is_some() {
        return Err(DbError::unsupported("FETCH is not supported"));
    }
    let order_by = convert_order_by(query.order_by.as_ref())?;
    let limit = convert_limit(query.limit_clause.as_ref())?;
    match *query.body {
        SetExpr::Select(select) => convert_select(*select, order_by, limit),
        SetExpr::SetOperation { .. } => Err(DbError::unsupported(
            "UNION, EXCEPT, and INTERSECT are not supported",
        )),
        SetExpr::Values(_) => Err(DbError::unsupported("VALUES statements are not supported")),
        _ => Err(DbError::unsupported("this query is not supported")),
    }
}

fn reject_common_query_clauses(query: &Query) -> Result<(), DbError> {
    if query.with.is_some() {
        return Err(DbError::unsupported("WITH is not supported"));
    }
    if !query.locks.is_empty() || query.for_clause.is_some() {
        return Err(DbError::unsupported("row locking is not supported"));
    }
    if query.settings.is_some() || query.format_clause.is_some() || !query.pipe_operators.is_empty()
    {
        return Err(DbError::unsupported("this query clause is not supported"));
    }
    Ok(())
}

fn convert_select(
    select: Select,
    order_by: Vec<OrderKey>,
    limit: Option<u64>,
) -> Result<Statement, DbError> {
    reject_select_clauses(&select)?;
    let table = convert_from(&select)?;
    let projection = convert_projection(&select.projection)?;
    let selection = select.selection.as_ref().map(convert_expr).transpose()?;
    Ok(Statement::Select {
        table,
        projection,
        selection,
        order_by,
        limit,
    })
}

fn reject_select_clauses(select: &Select) -> Result<(), DbError> {
    if select.distinct.is_some() {
        return Err(DbError::unsupported("DISTINCT is not supported"));
    }
    if select.top.is_some() {
        return Err(DbError::unsupported("TOP is not supported"));
    }
    if !matches!(select.flavor, SelectFlavor::Standard) {
        return Err(DbError::unsupported("this SELECT syntax is not supported"));
    }
    match &select.group_by {
        GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty() => {}
        _ => return Err(DbError::unsupported("GROUP BY is not supported")),
    }
    if select.having.is_some() {
        return Err(DbError::unsupported("HAVING is not supported"));
    }
    if select.qualify.is_some() {
        return Err(DbError::unsupported("QUALIFY is not supported"));
    }
    if !select.lateral_views.is_empty() {
        return Err(DbError::unsupported("LATERAL VIEW is not supported"));
    }
    if select.prewhere.is_some() {
        return Err(DbError::unsupported("PREWHERE is not supported"));
    }
    if !select.connect_by.is_empty() {
        return Err(DbError::unsupported("CONNECT BY is not supported"));
    }
    if !select.named_window.is_empty() {
        return Err(DbError::unsupported("window functions are not supported"));
    }
    if !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.into.is_some()
        || select.exclude.is_some()
        || select.value_table_mode.is_some()
        || select.select_modifiers.is_some()
        || !select.optimizer_hints.is_empty()
    {
        return Err(DbError::unsupported("this SELECT clause is not supported"));
    }
    Ok(())
}

fn convert_from(select: &Select) -> Result<String, DbError> {
    if select.from.len() != 1 {
        if select.from.is_empty() {
            return Err(DbError::invalid("SELECT requires a FROM clause"));
        }
        return Err(DbError::unsupported("JOIN is not supported"));
    }
    if !select.from[0].joins.is_empty() {
        return Err(DbError::unsupported("JOIN is not supported"));
    }
    table_name(&select.from[0].relation)
}

fn table_name(factor: &TableFactor) -> Result<String, DbError> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args,
            with_hints,
            version,
            with_ordinality,
            partitions,
            json_path,
            sample,
            index_directives,
            inheritance,
        } => {
            if alias.is_some() {
                return Err(DbError::unsupported("table aliases are not supported"));
            }
            if args.is_some()
                || !with_hints.is_empty()
                || version.is_some()
                || *with_ordinality
                || !partitions.is_empty()
                || json_path.is_some()
                || sample.is_some()
                || !index_directives.is_empty()
                || inheritance.is_some()
            {
                return Err(DbError::unsupported("this FROM clause is not supported"));
            }
            object_name(name)
        }
        _ => Err(DbError::unsupported("subqueries in FROM are not supported")),
    }
}

fn convert_projection(items: &[SelectItem]) -> Result<Projection, DbError> {
    if items.is_empty() {
        return Err(DbError::invalid("SELECT requires a column list"));
    }
    if let [SelectItem::Wildcard(options)] = items {
        if options.opt_ilike.is_none()
            && options.opt_exclude.is_none()
            && options.opt_except.is_none()
            && options.opt_replace.is_none()
            && options.opt_rename.is_none()
            && options.opt_alias.is_none()
        {
            return Ok(Projection::All);
        }
        return Err(DbError::unsupported("wildcard options are not supported"));
    }

    let mut columns = Vec::with_capacity(items.len());
    for item in items {
        match item {
            SelectItem::UnnamedExpr(expr) => columns.push(select_column(expr)?),
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                return Err(DbError::unsupported(
                    "mixing * with other select items is not supported",
                ));
            }
            SelectItem::ExprWithAlias { .. } | SelectItem::ExprWithAliases { .. } => {
                return Err(DbError::unsupported("column aliases are not supported"));
            }
        }
    }
    Ok(Projection::Columns(columns))
}

fn select_column(expr: &SqlExpr) -> Result<SelectColumn, DbError> {
    match convert_expr(expr)? {
        Expr::Column { name, qualifier } => Ok(SelectColumn { name, qualifier }),
        _ => Err(DbError::unsupported("SELECT expressions are not supported")),
    }
}

fn convert_expr(expr: &SqlExpr) -> Result<Expr, DbError> {
    match expr {
        SqlExpr::Nested(inner) => convert_expr(inner),
        SqlExpr::Identifier(ident) => Ok(Expr::Column {
            name: normalize_ident(ident),
            qualifier: None,
        }),
        SqlExpr::CompoundIdentifier(parts) => convert_compound(parts),
        SqlExpr::Value(value) => convert_literal(&value.value),
        SqlExpr::UnaryOp { op, expr } => convert_unary(*op, expr),
        SqlExpr::BinaryOp { left, op, right } => Ok(Expr::Binary {
            left: Box::new(convert_expr(left)?),
            op: convert_operator(op)?,
            right: Box::new(convert_expr(right)?),
        }),
        SqlExpr::IsNull(inner) => Ok(Expr::IsNull(Box::new(convert_expr(inner)?))),
        SqlExpr::IsNotNull(inner) => Ok(Expr::IsNotNull(Box::new(convert_expr(inner)?))),
        SqlExpr::Between { .. } => Err(DbError::unsupported("BETWEEN is not supported")),
        SqlExpr::Like { .. } | SqlExpr::ILike { .. } => {
            Err(DbError::unsupported("LIKE is not supported"))
        }
        SqlExpr::In { .. } => Err(DbError::unsupported("IN is not supported")),
        SqlExpr::Function(_) => Err(DbError::unsupported("function calls are not supported")),
        SqlExpr::Subquery(_) => Err(DbError::unsupported("subqueries are not supported")),
        _ => Err(DbError::unsupported("this expression is not supported")),
    }
}

fn convert_compound(parts: &[Ident]) -> Result<Expr, DbError> {
    if parts.len() != 2 {
        return Err(DbError::unsupported(
            "qualified names with more than two parts are not supported",
        ));
    }
    Ok(Expr::Column {
        qualifier: Some(normalize_ident(&parts[0])),
        name: normalize_ident(&parts[1]),
    })
}

fn convert_operator(op: &BinaryOperator) -> Result<BinOp, DbError> {
    match op {
        BinaryOperator::Eq => Ok(BinOp::Eq),
        BinaryOperator::NotEq => Ok(BinOp::NotEq),
        BinaryOperator::Lt => Ok(BinOp::Lt),
        BinaryOperator::Gt => Ok(BinOp::Gt),
        BinaryOperator::LtEq => Ok(BinOp::LtEq),
        BinaryOperator::GtEq => Ok(BinOp::GtEq),
        BinaryOperator::And => Ok(BinOp::And),
        BinaryOperator::Or => Ok(BinOp::Or),
        other => Err(DbError::unsupported(format!(
            "operator {other} is not supported"
        ))),
    }
}

fn convert_unary(op: UnaryOperator, expr: &SqlExpr) -> Result<Expr, DbError> {
    match op {
        UnaryOperator::Plus | UnaryOperator::Minus => {
            if let SqlExpr::Value(value) = expr
                && let SqlValue::Number(text, _) = &value.value
            {
                let negate = op == UnaryOperator::Minus;
                return Ok(Expr::Literal(Value::Integer(parse_integer(text, negate)?)));
            }
            Err(DbError::unsupported(
                "unary operators are only supported on integer literals",
            ))
        }
        UnaryOperator::Not => Ok(Expr::Not(Box::new(convert_expr(expr)?))),
        _ => Err(DbError::unsupported("this unary operator is not supported")),
    }
}

fn convert_literal(value: &SqlValue) -> Result<Expr, DbError> {
    match value {
        SqlValue::Number(text, _) => Ok(Expr::Literal(Value::Integer(parse_integer(text, false)?))),
        SqlValue::Boolean(value) => Ok(Expr::Literal(Value::Boolean(*value))),
        SqlValue::SingleQuotedString(text)
        | SqlValue::EscapedStringLiteral(text)
        | SqlValue::NationalStringLiteral(text) => Ok(Expr::Literal(Value::Text(text.clone()))),
        SqlValue::DollarQuotedString(text) => Ok(Expr::Literal(Value::Text(text.value.clone()))),
        SqlValue::Null => Ok(Expr::Literal(Value::Null)),
        SqlValue::Placeholder(_) => Err(DbError::unsupported("placeholders are not supported")),
        _ => Err(DbError::unsupported("this literal is not supported")),
    }
}

fn parse_integer(text: &str, negate: bool) -> Result<i64, DbError> {
    if text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
        return Err(DbError::unsupported(
            "floating-point literals are not supported",
        ));
    }

    let digits = text.strip_prefix('+').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(DbError::InvalidLiteral(format!(
            "invalid integer literal '{text}'"
        )));
    }
    if negate && digits == "9223372036854775808" {
        return Ok(i64::MIN);
    }

    let value = digits.parse::<i64>().map_err(|_| {
        DbError::InvalidLiteral(format!("integer literal '{text}' is out of range"))
    })?;
    if negate {
        value.checked_neg().ok_or_else(|| {
            DbError::InvalidLiteral(format!("integer literal '-{text}' is out of range"))
        })
    } else {
        Ok(value)
    }
}

fn convert_update(update: SqlUpdate) -> Result<Statement, DbError> {
    if !update.optimizer_hints.is_empty() || update.on_cluster.is_some() {
        return Err(DbError::unsupported("this UPDATE clause is not supported"));
    }
    if update.from.is_some() {
        return Err(DbError::unsupported("UPDATE FROM is not supported"));
    }
    if update.returning.is_some() || update.output.is_some() {
        return Err(DbError::unsupported(
            "UPDATE ... RETURNING is not supported",
        ));
    }
    if update.or.is_some() {
        return Err(DbError::unsupported("UPDATE OR is not supported"));
    }
    if !update.order_by.is_empty() {
        return Err(DbError::unsupported("UPDATE ORDER BY is not supported"));
    }
    if update.limit.is_some() {
        return Err(DbError::unsupported("UPDATE LIMIT is not supported"));
    }
    if update.settings.is_some() {
        return Err(DbError::unsupported("this UPDATE clause is not supported"));
    }
    if !update.table.joins.is_empty() {
        return Err(DbError::unsupported("JOIN is not supported"));
    }
    if update.assignments.is_empty() {
        return Err(DbError::invalid("UPDATE requires at least one assignment"));
    }

    let table = table_name(&update.table.relation)?;
    let mut assignments = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            return Err(DbError::unsupported("tuple assignments are not supported"));
        };
        assignments.push(Assignment {
            column: column_name(name)?,
            value: convert_expr(&assignment.value)?,
        });
    }
    let selection = update.selection.as_ref().map(convert_expr).transpose()?;
    Ok(Statement::Update {
        table,
        assignments,
        selection,
    })
}

fn convert_delete(delete: SqlDelete) -> Result<Statement, DbError> {
    if !delete.optimizer_hints.is_empty()
        || delete.on_cluster.is_some()
        || delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || delete.settings.is_some()
    {
        return Err(DbError::unsupported("this DELETE clause is not supported"));
    }
    if !delete.tables.is_empty() {
        return Err(DbError::unsupported("multi-table DELETE is not supported"));
    }
    if !delete.order_by.is_empty() {
        return Err(DbError::unsupported("DELETE ORDER BY is not supported"));
    }
    if delete.limit.is_some() {
        return Err(DbError::unsupported("DELETE LIMIT is not supported"));
    }

    let tables = match &delete.from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    };
    if tables.len() != 1 {
        return Err(DbError::invalid("DELETE requires exactly one table"));
    }
    if !tables[0].joins.is_empty() {
        return Err(DbError::unsupported("JOIN is not supported"));
    }
    let table = table_name(&tables[0].relation)?;
    let selection = delete.selection.as_ref().map(convert_expr).transpose()?;
    Ok(Statement::Delete { table, selection })
}

fn convert_order_by(order_by: Option<&OrderBy>) -> Result<Vec<OrderKey>, DbError> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    if order_by.interpolate.is_some() {
        return Err(DbError::unsupported(
            "ORDER BY INTERPOLATE is not supported",
        ));
    }
    let OrderByKind::Expressions(exprs) = &order_by.kind else {
        return Err(DbError::unsupported("ORDER BY ALL is not supported"));
    };
    exprs.iter().map(convert_order_key).collect()
}

fn convert_order_key(key: &sqlparser::ast::OrderByExpr) -> Result<OrderKey, DbError> {
    if key.with_fill.is_some() {
        return Err(DbError::unsupported("WITH FILL is not supported"));
    }
    if matches!(key.options.sort, Some(OrderBySort::Using(_))) {
        return Err(DbError::unsupported("ORDER BY USING is not supported"));
    }
    let column = match convert_expr(&key.expr)? {
        Expr::Column { name, qualifier } => SelectColumn { name, qualifier },
        _ => {
            return Err(DbError::unsupported(
                "ORDER BY expressions are not supported",
            ));
        }
    };
    let ascending = match key.options.sort {
        None | Some(OrderBySort::Asc) => true,
        Some(OrderBySort::Desc) => false,
        Some(OrderBySort::Using(_)) => {
            return Err(DbError::unsupported("ORDER BY USING is not supported"));
        }
    };
    Ok(OrderKey {
        column,
        ascending,
        nulls_first: key.options.nulls_first,
    })
}

fn convert_limit(limit: Option<&LimitClause>) -> Result<Option<u64>, DbError> {
    let Some(limit) = limit else {
        return Ok(None);
    };
    match limit {
        LimitClause::OffsetCommaLimit { .. } => {
            Err(DbError::unsupported("LIMIT offset, count is not supported"))
        }
        LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if offset.is_some() {
                return Err(DbError::unsupported("OFFSET is not supported"));
            }
            if !limit_by.is_empty() {
                return Err(DbError::unsupported("LIMIT BY is not supported"));
            }
            let Some(limit) = limit else {
                return Err(DbError::invalid("LIMIT requires a nonnegative integer"));
            };
            Ok(Some(nonnegative_integer(limit)?))
        }
    }
}

fn nonnegative_integer(expr: &SqlExpr) -> Result<u64, DbError> {
    match expr {
        SqlExpr::Nested(inner) => nonnegative_integer(inner),
        SqlExpr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => nonnegative_integer(expr),
        SqlExpr::UnaryOp {
            op: UnaryOperator::Minus,
            ..
        } => Err(DbError::invalid("LIMIT requires a nonnegative integer")),
        SqlExpr::Value(value) => match &value.value {
            SqlValue::Number(text, _) => {
                if text
                    .bytes()
                    .any(|byte| matches!(byte, b'.' | b'e' | b'E' | b'+' | b'-'))
                {
                    return Err(DbError::invalid("LIMIT requires a nonnegative integer"));
                }
                text.parse::<u64>()
                    .map_err(|_| DbError::invalid("LIMIT requires a nonnegative integer"))
            }
            _ => Err(DbError::invalid("LIMIT requires a nonnegative integer")),
        },
        _ => Err(DbError::unsupported("LIMIT expressions are not supported")),
    }
}

fn column_name(name: &ObjectName) -> Result<SelectColumn, DbError> {
    let mut idents = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        let Some(ident) = part.as_ident() else {
            return Err(DbError::unsupported("function names are not supported"));
        };
        idents.push(normalize_ident(ident));
    }
    match idents.as_slice() {
        [column] => Ok(SelectColumn {
            name: column.clone(),
            qualifier: None,
        }),
        [qualifier, column] => Ok(SelectColumn {
            name: column.clone(),
            qualifier: Some(qualifier.clone()),
        }),
        _ => Err(DbError::unsupported(
            "qualified names with more than two parts are not supported",
        )),
    }
}

fn object_name(name: &ObjectName) -> Result<String, DbError> {
    if name.0.len() != 1 {
        return Err(DbError::unsupported("qualified names are not supported"));
    }
    let Some(ident) = name.0[0].as_ident() else {
        return Err(DbError::unsupported("function names are not supported"));
    };
    let normalized = normalize_ident(ident);
    if normalized.is_empty() {
        return Err(DbError::invalid("table name must not be empty"));
    }
    Ok(normalized)
}

fn normalize_ident(ident: &Ident) -> String {
    if ident.quote_style.is_some() {
        ident.value.clone()
    } else {
        ident.value.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_unquoted_names_and_keeps_a_where_expression() {
        let statements =
            parse("SELECT Name FROM Users WHERE Age >= 25 AND active = TRUE;").unwrap();
        assert_eq!(statements.len(), 1);
        match &statements[0] {
            Statement::Select {
                table,
                projection,
                selection,
                ..
            } => {
                assert_eq!(table, "users");
                assert_eq!(
                    projection,
                    &Projection::Columns(vec![SelectColumn {
                        name: "name".to_string(),
                        qualifier: None,
                    }])
                );
                assert!(selection.is_some());
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn preserves_quoted_identifier_case() {
        let statements = parse("CREATE TABLE \"Users\" (\"Name\" TEXT);").unwrap();
        match &statements[0] {
            Statement::CreateTable { name, columns } => {
                assert_eq!(name, "Users");
                assert_eq!(columns[0].name(), "Name");
                assert_eq!(columns[0].data_type(), DataType::Text);
            }
            other => panic!("expected create, got {other:?}"),
        }
    }

    #[test]
    fn folds_a_negative_integer_literal() {
        let statements = parse("INSERT INTO t VALUES (-9223372036854775808);").unwrap();
        match &statements[0] {
            Statement::Insert { rows, .. } => {
                assert_eq!(rows, &vec![vec![Value::Integer(i64::MIN)]]);
            }
            other => panic!("expected insert, got {other:?}"),
        }
    }

    #[test]
    fn parses_order_by_and_limit() {
        let statements =
            parse("SELECT name FROM users ORDER BY age DESC, name ASC LIMIT 5;").unwrap();
        match &statements[0] {
            Statement::Select {
                order_by, limit, ..
            } => {
                assert_eq!(order_by.len(), 2);
                assert_eq!(order_by[0].column.name, "age");
                assert!(!order_by[0].ascending);
                assert!(order_by[1].ascending);
                assert_eq!(order_by[0].nulls_first, None);
                assert_eq!(*limit, Some(5));
            }
            other => panic!("expected select, got {other:?}"),
        }
    }

    #[test]
    fn rejects_order_by_expressions() {
        let error = parse("SELECT * FROM users ORDER BY 1;").unwrap_err();
        assert_eq!(error.to_string(), "ORDER BY expressions are not supported");
    }
}

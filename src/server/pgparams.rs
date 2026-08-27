//! Parameter type inference for the PostgreSQL extended query protocol.
//!
//! Clients that do not declare parameter OIDs expect the server to report
//! types in `ParameterDescription`. Since SQL is re-targeted before execution,
//! we infer each `$n` type from its syntactic position plus the target
//! backend's catalog (column types), with a conservative fallback.

use sqlparser::ast::{
    AssignmentTarget, BinaryOperator, Expr, Insert, ObjectName, SetExpr, Statement,
    TableFactor, TableObject, Value as AstValue,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use pgwire::api::Type;
use sqlx::AnyConnection;

use crate::backend::{self, Value};
use crate::server::common::Session;

/// Infer wire types for every `$n` placeholder in `sql` (1-based). Returns one
/// entry per highest index seen; unresolved slots fall back to TEXT.
pub async fn infer_param_types(
    session: &Session,
    conn: &mut AnyConnection,
    sql: &str,
) -> Vec<Type> {
    let _ = session;
    let Some(mut hints) = parse_hints(sql) else {
        return vec![];
    };

    if !hints.columns.is_empty() {
        let is_sqlite = matches!(catalog_kind(conn).await, CatalogKind::Sqlite);
        let tables: Vec<String> = hints.tables.iter().cloned().collect();
        for hint in hints.columns.iter_mut() {
            if let Some((ref table, ref column)) = hint.column {
                let oid = match table {
                    Some(t) => column_decl(conn, t, column, is_sqlite)
                        .await
                        .and_then(|d| oid_for_declared_type(&d)),
                    None => {
                        let mut found = None;
                        for tbl in &tables {
                            if let Some(d) = column_decl(conn, tbl, column, is_sqlite).await {
                                if let Some(oid) = oid_for_declared_type(&d) {
                                    found = Some(oid);
                                    break;
                                }
                            }
                        }
                        found
                    }
                };
                hint.type_oid = oid.or(hint.type_oid);
            }
        }
    }

    let max = hints.max_index.max(
        hints.columns.iter().filter_map(|h| Some(h.index)).max().unwrap_or(0),
    );
    if max == 0 {
        return vec![];
    }
    let mut out = vec![Type::TEXT; max];
    for hint in &hints.columns {
        if let (idx, Some(oid)) = (hint.index, hint.type_oid.clone()) {
            if let Some(slot) = out.get_mut(idx - 1) {
                *slot = Type::from_oid(oid).unwrap_or(Type::TEXT);
            }
        }
    }
    out
}

struct Hints {
    columns: Vec<ColumnHint>,
    /// Tables referenced anywhere in the statement.
    tables: std::collections::BTreeSet<String>,
    max_index: usize,
}

struct ColumnHint {
    /// `$n`, 1-based.
    index: usize,
    /// (table or None for "any candidate", column name).
    column: Option<(Option<String>, String)>,
    type_oid: Option<u32>,
}

/// Parse `sql` and collect placeholder contexts. Returns None when parsing
/// fails (callers then fall back to defaults).
fn parse_hints(sql: &str) -> Option<Hints> {
    let dialect = PostgreSqlDialect {};
    let stmts = Parser::parse_sql(&dialect, sql).ok()?;
    let mut hints = Hints {
        columns: vec![],
        tables: Default::default(),
        max_index: 0,
    };

    for stmt in stmts {
        match stmt {
            Statement::Insert(ins) => walk_insert(&mut hints, ins),
            Statement::Update(upd) => {
                let tbl = match &upd.table.relation {
                    TableFactor::Table { name, .. } => last_ident(name),
                    _ => continue,
                };
                hints.tables.insert(tbl.clone());
                for a in &upd.assignments {
                    if let AssignmentTarget::ColumnName(colname) = &a.target {
                        note_column_placeholder(
                            &mut hints,
                            &a.value,
                            Some(tbl.as_str()),
                            &colname.to_string(),
                        );
                    }
                }
                if let Some(sel) = &upd.selection {
                    walk_expr(&mut hints, sel, Some(tbl.as_str()));
                }
            }
            Statement::Delete(del) => {
                let tbls: Vec<ObjectName> = match &del.from {
                    sqlparser::ast::FromTable::WithFromKeyword(f) => {
                        f.iter().filter_map(|t| match &t.relation {
                            TableFactor::Table { name, .. } => Some(name.clone()),
                            _ => None,
                        }).collect()
                    }
                    sqlparser::ast::FromTable::WithoutKeyword(_) => del.tables.clone(),
                };
                if let Some(first) = tbls.first() {
                    let tbl = last_ident(first);
                    hints.tables.insert(tbl.clone());
                    if let Some(sel) = &del.selection {
                        walk_expr(&mut hints, sel, Some(tbl.as_str()));
                    }
                }
            }
            Statement::Query(q) => walk_query(&mut hints, q.as_ref()),
            _ => {}
        }
    }
    Some(hints)
}

fn walk_query(hints: &mut Hints, q: &sqlparser::ast::Query) {
    if let SetExpr::Select(select) = q.body.as_ref() {
        // Collect FROM tables and resolve WHERE placeholders against them.
        let mut from_tables: Vec<String> = vec![];
        for item in &select.from {
            collect_from_item(hints, &item.relation, &mut from_tables);
            for j in &item.joins {
                collect_from_item(hints, &j.relation, &mut from_tables);
            }
        }
        if let Some(sel) = &select.selection {
            for t in from_tables.iter() {
                hints.tables.insert(t.clone());
            }
            let default = from_tables.first().map(String::as_str);
            walk_expr(hints, sel, default);
        }
    }
}

fn collect_from_item(hints: &mut Hints, tf: &TableFactor, out: &mut Vec<String>) {
    if let TableFactor::Table { name, .. } = tf {
        let t = last_ident(name);
        out.push(t.clone());
        hints.tables.insert(t);
    }
}

fn walk_insert(hints: &mut Hints, ins: Insert) {
    if let TableObject::TableName(name) = &ins.table {
        let tbl = last_ident(name);
        hints.tables.insert(tbl.clone());
        let cols: Vec<String> = ins
            .columns
            .iter()
            .map(|c| c.to_string().to_ascii_lowercase())
            .collect();
        if let Some(q) = ins.source {
            if let SetExpr::Values(values) = q.body.as_ref() {
                for row in values.rows.iter() {
                    for (pos, e) in row.iter().enumerate() {
                        if let Some(idx) = placeholder_index(e) {
                            let col = cols.get(pos).cloned();
                            hints.max_index = hints.max_index.max(idx);
                            hints.columns.push(ColumnHint {
                                index: idx,
                                column: col.map(|c| (Some(tbl.clone()), c)),
                                type_oid: None,
                            });
                        }
                    }
                }
            }
        }
        // ON DUPLICATE KEY UPDATE assignments may contain placeholders.
        if let Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(assigns)) = &ins.on {
            for a in assigns {
                if let AssignmentTarget::ColumnName(c) = &a.target {
                    note_column_placeholder(hints, &a.value, Some(tbl.as_str()), &c.to_string());
                }
            }
        }
    }
}

/// Record `col <op> $n`-style placeholder context inside an expression tree.
fn walk_expr(hints: &mut Hints, expr: &Expr, default_table: Option<&str>) {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            if matches!(
                op,
                BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
            ) {
                if placeholder_index(right).is_some() {
                    note_column_placeholder(hints, right, default_table, &left.to_string());
                } else if placeholder_index(left).is_some() {
                    note_column_placeholder(hints, left, default_table, &right.to_string());
                }
            }
            walk_expr(hints, left, default_table);
            walk_expr(hints, right, default_table);
        }
        // LIKE / ILIKE are dedicated expression variants in sqlparser 0.62.
        Expr::Like { expr: lhs, pattern, .. } | Expr::ILike { expr: lhs, pattern, .. } => {
            if placeholder_index(pattern).is_some() {
                note_column_placeholder(hints, pattern, default_table, &lhs.to_string());
            }
            walk_expr(hints, lhs, default_table);
            walk_expr(hints, pattern, default_table);
        }
        Expr::Nested(e) => walk_expr(hints, e, default_table),
        Expr::InList { expr: lhs, list, .. } => {
            for item in list {
                note_column_placeholder(hints, item, default_table, &lhs.to_string());
            }
        }
        Expr::Between { expr: b, low, high, .. } => {
            note_column_placeholder(hints, low, default_table, &b.to_string());
            note_column_placeholder(hints, high, default_table, &b.to_string());
        }
        Expr::UnaryOp { expr: e, .. } => walk_expr(hints, e, default_table),
        Expr::Function(f) => {
            if let sqlparser::ast::FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(e),
                    ) = arg
                    {
                        walk_expr(hints, e, default_table);
                    }
                }
            }
        }
        _ => {}
    }
}

/// If `expr` is a `$n` placeholder, associate it with `column_text`.
fn note_column_placeholder(
    hints: &mut Hints,
    expr: &Expr,
    table: Option<&str>,
    column_text: &str,
) {
    let Some(idx) = placeholder_index(expr) else {
        return;
    };
    // Only plain identifiers are resolvable against the catalog.
    let Some(col) = plain_identifier(column_text) else { return; };
    hints.max_index = hints.max_index.max(idx);
    hints.columns.push(ColumnHint {
        index: idx,
        column: Some((table.map(str::to_string), col)),
        type_oid: None,
    });
}

/// Extract a bare identifier string ("users.name" -> "name") or None.
fn plain_identifier(text: &str) -> Option<String> {
    let cleaned = text.trim_matches(|c| c == '"' || c == '`' || c == ' ');
    let last = cleaned.rsplit('.').next()?;
    let valid = !last.is_empty()
        && last.chars().all(|c| c.is_alphanumeric() || c == '_');
    valid.then(|| last.to_ascii_lowercase())
}

fn placeholder_index(expr: &Expr) -> Option<usize> {
    if let Expr::Value(vws) = expr {
        if let AstValue::Placeholder(p) = &vws.value {
            return p.trim_start_matches('$').parse::<usize>().ok();
        }
    }
    None
}

fn last_ident(name: &ObjectName) -> String {
    name.0.last()
        .map(|p| p.to_string().trim_matches(|c| c == '"' || c == '`').to_ascii_lowercase())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Catalog lookups
// ---------------------------------------------------------------------------

enum CatalogKind {
    Sqlite,
    Other,
}

async fn catalog_kind(conn: &mut AnyConnection) -> CatalogKind {
    match backend::fetch(conn, "SELECT count(*) FROM sqlite_master").await {
        Ok(_) => CatalogKind::Sqlite,
        Err(_) => CatalogKind::Other,
    }
}

/// Declared type of `table.column` from the backend catalog.
async fn column_decl(
    conn: &mut AnyConnection,
    table: &str,
    column: &str,
    is_sqlite: bool,
) -> Option<String> {
    let rows: Vec<Vec<Option<Value>>> = if is_sqlite {
        let (_, rows) = backend::fetch(
            conn,
            &format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\"")),
        )
        .await
        .ok()?;
        // cid, name, type, notnull, dflt_value, pk -> keep the type cell (2).
        rows.into_iter()
            .filter(|r| {
                r.get(1)
                    .and_then(|v| v.as_ref())
                    .map(|v| text_of(v) == column)
                    .unwrap_or(false)
            })
            .filter_map(|mut r| {
                if r.len() > 2 {
                    Some(vec![r.swap_remove(2)])
                } else {
                    None
                }
            })
            .collect()
    } else {
        let (_, rows) = backend::fetch(
            conn,
            &format!(
                "SELECT data_type FROM information_schema.columns \
                 WHERE table_name = '{}' AND column_name = '{}'",
                escape(table),
                escape(column)
            ),
        )
        .await
        .ok()?;
        rows
    };
    rows.into_iter()
        .next()?
        .into_iter()
        .next()
        .flatten()
        .map(|v| text_of(&v))
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        other => other.as_mysql_text().unwrap_or_default(),
    }
}

/// Map a declared column type to a PG wire OID. SQLite stores loose affinity
/// strings; MySQL/PG report their own names.
fn oid_for_declared_type(decl: &str) -> Option<u32> {
    let d = decl.trim().to_ascii_uppercase();
    let base = d.split('(').next()?.trim();
    let affinity_int = base.contains("INT");
    let affinity_float =
        base.contains("REAL") || base.contains("FLOA") || base.contains("DOUB");
    Some(match base {
        x if x == "BOOL" || x == "BOOLEAN" => 16,               // bool
        x if affinity_int => {
            if x == "BIGINT" || x == "INT8" || x == "SERIAL8" {
                20                                              // int8
            } else {
                23                                              // int4
            }
        }
        x if x == "NUMERIC" || x == "DECIMAL" || x == "DEC" => 1700,
        _ if affinity_float => 701,                             // float8
        "BLOB" | "BYTEA" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => 17, // bytea
        _ => 25,                                                // text
    })
}

fn escape(s: &str) -> String {
    s.replace('\'', "''")
}

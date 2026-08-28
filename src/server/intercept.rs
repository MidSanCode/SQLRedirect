//! Proxy-side statement interception.
//!
//! Some statements must not be forwarded verbatim to the backend:
//! - MySQL session functions whose state lives in the proxy
//!   (`LAST_INSERT_ID()`).
//! - `SHOW ...` / `DESCRIBE` catalog statements, which are re-emitted as
//!   queries against the *target* dialect's catalog tables.
//! - `SET` / `USE` session state that has no portable meaning across
//!   dialects (accepted and swallowed when front != target).

use sqlx::AnyConnection;
use sqlparser::ast::{
    Expr, FunctionArguments, SelectItem, SetExpr, ShowCreateObject,
    ShowStatementFilter, ShowStatementFilterPosition, ShowStatementOptions, Statement,
};

use crate::backend::{self, FieldMeta, Value};
use crate::error::Result;
use crate::server::common::{Outcome, Session};
use crate::translate::TargetDialect;

/// Inspect one parsed statement and either produce a synthetic `Outcome`
/// or return `None` to let normal translation/forwarding handle it.
pub async fn intercept_statement(
    session: &Session,
    conn: &mut AnyConnection,
    stmt: &Statement,
) -> Result<Option<Outcome>> {
    // SELECT LAST_INSERT_ID() — answered from proxy session state.
    if let Some(outcome) = last_insert_id_outcome(session, stmt) {
        return Ok(Some(outcome));
    }

    let intercepted: Option<Outcome> = match stmt {
        // Session variables: swallow when dialects differ (drivers send many
        // non-portable SETs). When front == target this interception is
        // skipped so real SETs still reach a matching backend.
        Statement::Set(..) => {
            if !session.translator.front_dialect_matches_target() {
                tracing::debug!("swallowed SET for {} target", session.backend.dialect());
                Some(ok_outcome("SET"))
            } else {
                None
            }
        }
        // USE <db>: single-database backends accept and ignore.
        Statement::Use(..) => Some(ok_outcome("USE")),
        // PG-style `SHOW <name>`: answer with generic defaults for non-PG
        // backends; forwarded untouched to a PG backend.
        Statement::ShowVariable { variable }
            if session.backend.dialect() != TargetDialect::Postgres =>
        {
            Some(show_variable_outcome(variable))
        }
        Statement::ShowTables { show_options, .. } => {
            show_tables(session, conn, show_options).await?
        }
        Statement::ShowSchemas { .. } | Statement::ShowDatabases { .. } => {
            show_databases(session, conn).await?
        }
        Statement::ShowColumns { show_options, .. } => {
            show_columns(session, conn, show_options).await?
        }
        Statement::ExplainTable { table_name, .. } => {
            describe_table(session, conn, table_name).await?
        }
        Statement::ShowCreate { obj_type, obj_name } => match obj_type {
            ShowCreateObject::Table => show_create_table(session, conn, obj_name).await?,
            _ => None,
        },
        _ => None,
    };
    Ok(intercepted)
}

fn ok_outcome(command: &str) -> Outcome {
    Outcome::Affected {
        rows_affected: 0,
        last_insert_id: None,
        command: command.to_string(),
    }
}

// ---------------------------------------------------------------------------
// LAST_INSERT_ID()
// ---------------------------------------------------------------------------

/// Recognize a bare `LAST_INSERT_ID()` function call expression.
fn is_last_insert_id(e: &Expr) -> bool {
    if let Expr::Function(f) = e {
        if f.name.to_string().to_ascii_lowercase() == "last_insert_id" {
            return match &f.args {
                FunctionArguments::None => true,
                FunctionArguments::List(l) => l.args.is_empty(),
                FunctionArguments::Subquery(_) => false,
            };
        }
    }
    false
}

fn last_insert_id_outcome(session: &Session, stmt: &Statement) -> Option<Outcome> {
    let Statement::Query(q) = stmt else {
        return None;
    };
    let SetExpr::Select(select) = q.body.as_ref() else {
        return None;
    };
    if select.projection.len() != 1 {
        return None;
    }
    let name = match &select.projection[0] {
        SelectItem::UnnamedExpr(e) if is_last_insert_id(e) => "LAST_INSERT_ID()".to_string(),
        SelectItem::ExprWithAlias { expr, alias } if is_last_insert_id(expr) => {
            alias.value.clone()
        }
        _ => return None,
    };
    Some(Outcome::Rows {
        fields: vec![FieldMeta {
            name,
            kind: sqlx::any::AnyTypeInfoKind::BigInt,
        }],
        rows: vec![vec![Some(Value::BigInt(session.get_last_insert_id()))]],
    })
}

/// Generic answers for PG-style `SHOW <name>` on non-PG backends.
fn show_variable_outcome(variable: &[sqlparser::ast::Ident]) -> Outcome {
    let name = variable
        .iter()
        .map(|i| i.value.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(".");
    let value = match name.as_str() {
        "server_version" | "server_version_num" => "15.0",
        "client_encoding" | "server_encoding" => "UTF8",
        "timezone" | "time_zone" => "UTC",
        "standard_conforming_strings" => "on",
        "integer_datetimes" => "on",
        "max_identifier_length" | "max_index_keys" => "63",
        _ => "",
    };
    Outcome::Rows {
        fields: vec![FieldMeta {
            name: name.to_ascii_uppercase(),
            kind: sqlx::any::AnyTypeInfoKind::Text,
        }],
        rows: vec![vec![Some(Value::Text(value.to_string()))]],
    }
}

// ---------------------------------------------------------------------------
// SHOW TABLES / DATABASES / COLUMNS / CREATE TABLE
// ---------------------------------------------------------------------------

/// Extract a `LIKE 'pattern'` filter from SHOW statement options.
fn like_pattern(opts: &ShowStatementOptions) -> Option<String> {
    match opts.filter_position.as_ref()? {
        ShowStatementFilterPosition::Suffix(f) | ShowStatementFilterPosition::Infix(f) => match f {
            ShowStatementFilter::Like(p) | ShowStatementFilter::ILike(p) => Some(p.clone()),
            _ => None,
        },
    }
}

async fn run_rows(conn: &mut AnyConnection, sql: String) -> Result<Option<Outcome>> {
    let (fields, rows) = backend::fetch(conn, &sql).await?;
    Ok(Some(Outcome::Rows { fields, rows }))
}

fn like_clause(pattern: &Option<String>, column: &str) -> String {
    match pattern {
        Some(p) => format!(
            " AND {} LIKE '{}'",
            column,
            p.replace('\'', "''")
        ),
        None => String::new(),
    }
}

async fn show_tables(
    session: &Session,
    conn: &mut AnyConnection,
    opts: &ShowStatementOptions,
) -> Result<Option<Outcome>> {
    let pattern = like_pattern(opts);
    let sql = match session.backend.dialect() {
        TargetDialect::Sqlite => format!(
            "SELECT name AS \"Tables_in_main\" FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'{} ORDER BY name",
            like_clause(&pattern, "name")
        ),
        TargetDialect::Postgres => format!(
            "SELECT tablename AS \"Tables_in_public\" FROM pg_tables \
             WHERE schemaname = 'public'{} ORDER BY tablename",
            like_clause(&pattern, "tablename")
        ),
        TargetDialect::Mysql => return Ok(None), // pass through untranslated
    };
    run_rows(conn, sql).await
}

async fn show_databases(session: &Session, conn: &mut AnyConnection) -> Result<Option<Outcome>> {
    match session.backend.dialect() {
        TargetDialect::Sqlite => Ok(Some(Outcome::Rows {
            fields: vec![FieldMeta {
                name: "Database".to_string(),
                kind: sqlx::any::AnyTypeInfoKind::Text,
            }],
            rows: vec![vec![Some(Value::Text("main".to_string()))]],
        })),
        TargetDialect::Postgres => {
            run_rows(
                conn,
                "SELECT datname AS \"Database\" FROM pg_database \
                 WHERE datallowconn ORDER BY datname"
                    .to_string(),
            )
            .await
        }
        TargetDialect::Mysql => Ok(None),
    }
}

/// Extract the referenced table name from SHOW options (`IN|FROM tbl`) or
/// fall back to parsing it from the raw text of DESCRIBE-style statements.
fn show_target_table(opts: &ShowStatementOptions) -> Option<String> {
    opts.show_in
        .as_ref()?
        .parent_name
        .as_ref()
        .map(|n| n.to_string())
}

/// Render one SQLite PRAGMA row in MySQL `SHOW COLUMNS` shape.
fn sqlite_column_row(r: &[Option<Value>]) -> Vec<Option<Value>> {
    // cid, name, type, notnull, dflt_value, pk
    let get = |i: usize| r.get(i).cloned().flatten();
    let name = get(1).unwrap_or(Value::Null);
    let ty = match get(2) {
        Some(Value::Text(t)) if !t.is_empty() => Value::Text(t),
        _ => Value::Text("".to_string()),
    };
    // SQLite stores all integers as 64-bit; sqlx decodes them as BigInt.
    let int_value = |idx: usize| -> Option<i64> {
        match get(idx) {
            Some(Value::SmallInt(n)) => Some(n as i64),
            Some(Value::Integer(n)) => Some(n as i64),
            Some(Value::BigInt(n)) => Some(n),
            _ => None,
        }
    };
    let notnull = int_value(3).is_some_and(|n| n != 0);
    let pk = int_value(5).is_some_and(|n| n != 0);
    let dflt = get(4);
    let extra = Value::Text(String::new());
    vec![
        Some(name),
        Some(ty),
        Some(Value::Text(if notnull { "NO" } else { "YES" }.to_string())),
        Some(Value::Text(if pk { "PRI" } else { "" }.to_string())),
        dflt,
        Some(extra),
    ]
}

const SHOW_COLUMNS_FIELDS: [&str; 6] = ["Field", "Type", "Null", "Key", "Default", "Extra"];

fn columns_meta() -> Vec<FieldMeta> {
    SHOW_COLUMNS_FIELDS
        .iter()
        .map(|f| FieldMeta {
            name: f.to_string(),
            kind: sqlx::any::AnyTypeInfoKind::Text,
        })
        .collect()
}

fn text_rows(rows: Vec<Vec<Option<Value>>>) -> Outcome {
    Outcome::Rows {
        fields: columns_meta(),
        rows,
    }
}

/// `SHOW COLUMNS FROM t` — translated to the target catalog.
async fn show_columns(
    session: &Session,
    conn: &mut AnyConnection,
    opts: &ShowStatementOptions,
) -> Result<Option<Outcome>> {
    let table = match show_target_table(opts) {
        Some(t) => t,
        None => {
            // `SHOW COLUMNS` without IN/FROM has no table; nothing sensible.
            tracing::debug!("SHOW COLUMNS without table target");
            return Ok(None);
        }
    };
    column_listing(session, conn, &table).await
}

/// `DESCRIBE t` / `DESC t`.
async fn describe_table(
    session: &Session,
    conn: &mut AnyConnection,
    table_name: &sqlparser::ast::ObjectName,
) -> Result<Option<Outcome>> {
    column_listing(session, conn, &table_name.to_string()).await
}

/// Strip identifier quoting from a (possibly compound) object name and keep
/// the last component only.
fn plain_table_name(name: &str) -> String {
    let last = name.split('.').next_back().unwrap_or(name);
    last.trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == ' ')
        .to_string()
}

async fn column_listing(
    session: &Session,
    conn: &mut AnyConnection,
    raw_table: &str,
) -> Result<Option<Outcome>> {
    let table = plain_table_name(raw_table);
    match session.backend.dialect() {
        TargetDialect::Sqlite => {
            let sql = format!(
                "PRAGMA table_info(\"{}\")",
                table.replace('"', "\"\"")
            );
            let (_, rows) = backend::fetch(conn, &sql).await?;
            let mapped = rows.into_iter().map(|r| sqlite_column_row(&r)).collect();
            Ok(Some(text_rows(mapped)))
        }
        TargetDialect::Postgres => {
            let sql = format!(
                "SELECT column_name AS field, \
                 udt_name AS type, \
                 CASE WHEN is_nullable = 'YES' THEN 'YES' ELSE 'NO' END AS null, \
                 '' AS key, column_default, '' AS extra \
                 FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = '{t}' \
                 ORDER BY ordinal_position",
                t = table.replace('\'', "''")
            );
            let (_, rows) = backend::fetch(conn, &sql).await?;
            Ok(Some(text_rows(rows)))
        }
        TargetDialect::Mysql => Ok(None), // pass through untranslated
    }
}

/// `SHOW CREATE TABLE t` — best-effort reconstruction from catalog tables.
async fn show_create_table(
    session: &Session,
    conn: &mut AnyConnection,
    obj_name: &sqlparser::ast::ObjectName,
) -> Result<Option<Outcome>> {
    let table = plain_table_name(&obj_name.to_string());
    let fields = vec![
        FieldMeta {
            name: "Table".to_string(),
            kind: sqlx::any::AnyTypeInfoKind::Text,
        },
        FieldMeta {
            name: "Create Table".to_string(),
            kind: sqlx::any::AnyTypeInfoKind::Text,
        },
    ];
    let outcome = |ddl: Option<String>| {
        Some(Outcome::Rows {
            fields: fields.clone(),
            rows: ddl
                .map(|s| {
                    vec![
                        Some(Value::Text(table.clone())),
                        Some(Value::Text(s)),
                    ]
                })
                .map(|row| vec![row])
                .unwrap_or_default(),
        })
    };
    let first_text = |rows: Vec<Vec<Option<Value>>>| -> Option<String> {
        rows.into_iter()
            .next()
            .and_then(|r| r.into_iter().next())
            .flatten()
            .map(|v| match v {
                Value::Text(s) => s,
                other => other.as_mysql_text().unwrap_or_default(),
            })
    };
    match session.backend.dialect() {
        TargetDialect::Sqlite => {
            let sql = format!(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = '{t}'",
                t = table.replace('\'', "''")
            );
            let (_, rows) = backend::fetch(conn, &sql).await?;
            Ok(outcome(first_text(rows)))
        }
        TargetDialect::Postgres => {
            let sql = format!(
                "SELECT 'CREATE TABLE \"{t}\" (' || string_agg( \
                 column_name || ' ' || data_type || \
                 CASE WHEN is_nullable = 'NO' THEN ' NOT NULL' ELSE '' END, \
                 ', ' ORDER BY ordinal_position) || ')' \
                 FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = '{q}'",
                t = table.replace('"', "\"\""),
                q = table.replace('\'', "''")
            );
            let (_, rows) = backend::fetch(conn, &sql).await?;
            Ok(outcome(first_text(rows)))
        }
        TargetDialect::Mysql => Ok(None),
    }
}

//! Backend execution layer on top of sqlx::Any (PostgreSQL / MySQL / SQLite).

pub mod rows;

use std::sync::Arc;
use std::time::Duration;

use sqlx::any::{AnyRow, AnyTypeInfoKind};
use sqlx::AnyConnection;
use sqlx::{Column, Row as SqlxRow};
use sqlx::Value as _;
use tokio::sync::Mutex;

use crate::error::{Error, Result};
use crate::translate::TargetDialect;

pub use rows::{FieldMeta, Value};

/// A connection to the real target database.
#[derive(Clone)]
pub struct Backend {
    pool: sqlx::AnyPool,
    pub dialect: TargetDialect,
}

impl Backend {
    pub async fn connect(url: &str, max_connections: u32) -> Result<Backend> {
        let dialect = TargetDialect::from_backend_url(url)?;
        let mut options = sqlx::any::AnyPoolOptions::new();
        options = options.max_connections(max_connections);
        let pool = options
            .connect(url)
            .await
            .map_err(|e| Error::Backend(format!("cannot connect to backend '{url}': {e}")))?;
        Ok(Backend { pool, dialect })
    }

    /// Acquire a dedicated backend connection for one wire-session.
    pub async fn acquire_owned(&self) -> Result<AnyConnection> {
        self.pool
            .acquire()
            .await
            .map(|c| c.detach())
            .map_err(|e| Error::Backend(format!("cannot acquire backend connection: {e}")))
    }

    pub fn dialect(&self) -> TargetDialect {
        self.dialect
    }

    /// Number of currently-open connections in the pool (for diagnostics).
    pub fn size(&self) -> u32 {
        self.pool.size()
    }
}

/// Describe result-set columns from a row's metadata.
pub fn column_meta(row: &AnyRow) -> Vec<FieldMeta> {
    row.columns()
        .iter()
        .map(|c| {
            let kind = c.type_info().kind().clone();
            FieldMeta {
                name: c.name().to_string(),
                kind,
            }
        })
        .collect()
}

/// Decode a single cell based on the value's actual kind (from `try_get_raw`),
/// which reflects the real storage type even when the column metadata reports an
/// unknown kind (e.g. SQLite expression columns).
fn decode_cell(row: &AnyRow, idx: usize) -> Result<Option<Value>> {
    let raw = row
        .try_get_raw(idx)
        .map_err(|e| Error::Backend(e.to_string()))?;
    let v = sqlx::ValueRef::to_owned(&raw);
    if v.is_null() {
        return Ok(None);
    }
    use AnyTypeInfoKind::*;
    let value = match v.type_info().kind() {
        Bool => Value::Bool(v.try_decode_unchecked::<bool>().map_err(backend_err)?),
        SmallInt => Value::SmallInt(v.try_decode_unchecked::<i16>().map_err(backend_err)?),
        Integer => Value::Integer(v.try_decode_unchecked::<i32>().map_err(backend_err)?),
        BigInt => Value::BigInt(v.try_decode_unchecked::<i64>().map_err(backend_err)?),
        Real => Value::Real(v.try_decode_unchecked::<f32>().map_err(backend_err)?),
        Double => Value::Double(v.try_decode_unchecked::<f64>().map_err(backend_err)?),
        Text => Value::Text(v.try_decode_unchecked::<String>().map_err(backend_err)?),
        Blob => Value::Blob(v.try_decode_unchecked::<Vec<u8>>().map_err(backend_err)?),
        Null => Value::Text(v.try_decode_unchecked::<String>().map_err(backend_err)?),
    };
    Ok(Some(value))
}

fn backend_err(e: sqlx::Error) -> Error {
    Error::Backend(e.to_string())
}

/// The stable type-kind implied by a decoded value.
fn value_kind(v: &Option<Value>) -> AnyTypeInfoKind {
    use AnyTypeInfoKind::*;
    match v {
        None => Null,
        Some(Value::Null) => Null,
        Some(Value::Bool(_)) => Bool,
        Some(Value::SmallInt(_)) => SmallInt,
        Some(Value::Integer(_)) => Integer,
        Some(Value::BigInt(_)) => BigInt,
        Some(Value::Real(_)) => Real,
        Some(Value::Double(_)) => Double,
        Some(Value::Text(_)) => Text,
        Some(Value::Blob(_)) => Blob,
    }
}

/// Execute a statement that does not return rows.
pub async fn execute(
    conn: &mut AnyConnection,
    sql: &str,
) -> Result<(u64, Option<i64>)> {
    let res = sqlx::query(sql)
        .execute(&mut *conn)
        .await
        .map_err(|e| Error::Backend(format!("{e} (sql: {})", snippet(sql))))?;
    // sqlx's `AnyQueryResult` discards SQLite's `last_insert_rowid()`.
    // Query it explicitly for the SQLite target so `LAST_INSERT_ID()` works.
    let lid = if matches!(
        crate::translate::TargetDialect::from_conn(conn).await.ok(),
        Some(crate::translate::TargetDialect::Sqlite)
    ) {
        match sqlx::query("SELECT last_insert_rowid() AS id")
            .fetch_one(&mut *conn)
            .await
        {
            Ok(row) => row
                .try_get::<i64, _>("id")
                .ok()
                .filter(|v| *v != 0)
                .or(res.last_insert_id()),
            Err(_) => res.last_insert_id(),
        }
    } else {
        res.last_insert_id()
    };
    Ok((res.rows_affected(), lid))
}

/// Fetch all rows for a query.
pub async fn fetch(
    conn: &mut AnyConnection,
    sql: &str,
) -> Result<(Vec<FieldMeta>, Vec<Vec<Option<Value>>>)> {
    let rows = sqlx::query(sql)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| Error::Backend(format!("{e} (sql: {})", snippet(sql))))?;

    if rows.is_empty() {
        return Ok((vec![], vec![]));
    }

    // For SQLite, sqlx reports every integer column as `BigInt` (i64) because
    // SQLite has no integer width — but the wire dialect needs to distinguish
    // `INTEGER` (→ INT4) from `BIGINT` (→ INT8). Resolve the declared type
    // affinity from the catalog so the result schema matches the source DDL.
    let sqlite_affinity = if matches!(
        crate::translate::TargetDialect::from_conn(conn).await.ok(),
        Some(crate::translate::TargetDialect::Sqlite)
    ) {
        resolve_sqlite_column_kinds(conn, sql, rows[0].columns().len()).await
    } else {
        Vec::new()
    };

    let cols: Vec<FieldMeta> = (0..rows[0].columns().len())
        .map(|idx| {
            let name = rows[0].columns()[idx].name().to_string();
            let base = rows[0].columns()[idx].type_info().kind().clone();
            let kind: AnyTypeInfoKind = match sqlite_affinity.get(idx) {
                // Resolved from PRAGMA table_info (SQLite only).
                Some(resolved) => match *resolved {
                    Some(k) => k,
                    None => {
                        use AnyTypeInfoKind::Null;
                        if base == Null {
                            value_kind(&decode_cell(&rows[0], idx).unwrap_or(None))
                        } else {
                            base
                        }
                    }
                },
                // Non-SQLite backends: use the column metadata when known.
                None => {
                    use AnyTypeInfoKind::Null;
                    if base == Null {
                        value_kind(&decode_cell(&rows[0], idx).unwrap_or(None))
                    } else {
                        base
                    }
                }
            };
            FieldMeta { name, kind }
        })
        .collect();

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let mut vals = Vec::with_capacity(cols.len());
        for idx in 0..cols.len() {
            vals.push(decode_cell(row, idx)?);
        }
        out.push(vals);
    }
    Ok((cols, out))
}

/// Describe a statement's result columns without executing it (works even
/// when the query would return zero rows).
///
/// sqlx's `describe()` is gated behind driver-level `offline` features that
/// do not unify across the `any` facade, so we probe by executing a
/// side-effect-free wrapper instead:
/// 1. `SELECT * FROM (<q>) LIMIT 1` — real values, exact types.
/// 2. If the source query yields no rows, force one all-NULL row with
///    `SELECT t.* FROM (SELECT 1) d LEFT JOIN (<q>) t ON 0 = 1`, which keeps
///    column names while costing nothing.
pub async fn describe(conn: &mut AnyConnection, sql: &str) -> Result<Vec<FieldMeta>> {
    let wrapped = format!("SELECT * FROM ({sql}) AS __sqr_probe LIMIT 1");
    let (mut cols, rows) = fetch(conn, &wrapped).await?;
    // The wrapped subquery hides table names from the SQLite column-type
    // resolver, so re-resolve the kinds from the original SQL.
    reapply_sqlite_kinds(conn, sql, &mut cols).await;
    if !rows.is_empty() {
        return Ok(cols);
    }
    let forced = format!(
        "SELECT t.* FROM (SELECT 1) AS __sqr_dummy LEFT JOIN ({sql}) t ON 0 = 1 LIMIT 1"
    );
    let (mut cols2, rows2) = fetch(conn, &forced).await?;
    reapply_sqlite_kinds(conn, sql, &mut cols2).await;
    if !rows2.is_empty() {
        Ok(cols2)
    } else {
        Ok(cols)
    }
}

/// For SQLite backends, override the result-column kinds with affinities
/// resolved from the *original* (unwrapped) SQL's catalog tables.
async fn reapply_sqlite_kinds(conn: &mut AnyConnection, sql: &str, cols: &mut [FieldMeta]) {
    if !matches!(
        crate::translate::TargetDialect::from_conn(conn).await.ok(),
        Some(crate::translate::TargetDialect::Sqlite)
    ) {
        return;
    }
    let resolved = resolve_sqlite_column_kinds(conn, sql, cols.len()).await;
    for (i, col) in cols.iter_mut().enumerate() {
        if let Some(Some(kind)) = resolved.get(i) {
            col.kind = *kind;
        }
    }
}

fn snippet(sql: &str) -> String {
    if sql.chars().count() > 200 {
        format!("{}...", sql.chars().take(200).collect::<String>())
    } else {
        sql.to_string()
    }
}

/// Resolve the wire-friendly type kind of every result column of `sql` by
/// consulting the SQLite catalog (`PRAGMA table_info`).
///
/// SQLite reports every integer column as `BigInt` at the value layer, but the
/// wire dialect (PostgreSQL/MySQL clients) needs to distinguish `INTEGER`
/// (→ INT4) from `BIGINT` (→ INT8). We extract the tables referenced by the
/// query, read their declared column affinities, and return one kind per
/// result column. Columns we cannot resolve (expressions, literals, joins)
/// yield `None` so the caller keeps the value-derived kind.
async fn resolve_sqlite_column_kinds(
    conn: &mut AnyConnection,
    sql: &str,
    ncols: usize,
) -> Vec<Option<AnyTypeInfoKind>> {
    use sqlx::Row as _;
    let tables = sqlite_tables_in_query(sql);
    if tables.is_empty() {
        return vec![None; ncols];
    }

    // column name (lower-cased) -> declared affinity kind
    let mut affinity: std::collections::HashMap<String, AnyTypeInfoKind> =
        std::collections::HashMap::new();
    for table in &tables {
        let pragma = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
        if let Ok(rows) = sqlx::query(&pragma).fetch_all(&mut *conn).await {
            for r in &rows {
                // cid, name, type, notnull, dflt_value, pk
                let name: Option<String> = r.try_get(1).ok();
                let ty: Option<String> = r.try_get(2).ok();
                if let (Some(name), Some(ty)) = (name, ty) {
                    affinity.insert(name.to_ascii_lowercase(), sqlite_affinity_kind(&ty));
                }
            }
        }
    }

    let result_cols = sqlite_result_column_names(sql);
    result_cols
        .iter()
        .map(|c| c.as_ref().and_then(|n| affinity.get(&n.to_ascii_lowercase()).copied()))
        .collect()
}

/// Map a SQLite declared type string to a wire-friendly kind.
fn sqlite_affinity_kind(declared: &str) -> AnyTypeInfoKind {
    let d = declared.to_ascii_uppercase();
    let base = d.split('(').next().unwrap_or("").trim();
    use AnyTypeInfoKind::*;
    if base.contains("BIGINT") || base.contains("INT8") || base.contains("UNSIGNED BIG") {
        BigInt
    } else if base.contains("INT") {
        Integer
    } else if base.contains("FLOA") || base.contains("DOUB") || base.contains("REAL") {
        Double
    } else if base.contains("TEXT") || base.contains("CHAR") || base.contains("CLOB") {
        Text
    } else if base.contains("BLOB") || base.is_empty() {
        Blob
    } else if base == "BOOL" || base == "BOOLEAN" {
        Bool
    } else {
        Text
    }
}

/// Extract the table names referenced in the `FROM`/`JOIN` clauses of a
/// top-level `SELECT` (best-effort; handles the common `tbl` / `schema.tbl`
/// forms and quoted identifiers).
fn sqlite_tables_in_query(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let mut tables = Vec::new();
    for kw in [" FROM ", " JOIN ", " INNER JOIN ", " LEFT JOIN ", " RIGHT JOIN ", " CROSS JOIN "] {
        let needle = kw.trim();
        let mut start = 0;
        while let Some(pos) = upper[start..].find(needle) {
            let abs = start + pos;
            let after = abs + needle.len();
            let rest = &sql[after..];
            if let Some(t) = first_table_token(rest) {
                tables.push(t);
            }
            start = after;
        }
    }
    tables
}

/// Parse the first table-name token from a fragment that begins right after a
/// `FROM`/`JOIN` keyword.
fn first_table_token(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    // skip whitespace
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    // optional `schema.`
    let mut buf = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '.' || c.is_alphanumeric() || c == '_' {
            buf.push(c);
            i += 1;
        } else if c == '"' || c == '`' {
            // quoted identifier
            let q = c;
            i += 1;
            while i < chars.len() && chars[i] != q {
                buf.push(chars[i]);
                i += 1;
            }
            i += 1; // skip closing quote
            // consume a trailing `.` only if it's part of schema.tbl
            if i < chars.len() && chars[i] == '.' {
                buf.push('.');
                i += 1;
                // read the next identifier
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '"' || chars[i] == '`') {
                    buf.push(chars[i]);
                    i += 1;
                }
            }
            break;
        } else {
            break;
        }
    }
    let last = buf.split('.').next_back()?.to_string();
    if last.is_empty() {
        None
    } else {
        Some(last.trim_matches(|c| c == '"' || c == '`').to_string())
    }
}

/// Extract the result-column names of a top-level `SELECT`, in order. Returns
/// `None` for columns whose name cannot be determined (so the caller falls
/// back to the value-derived kind). Best-effort for the common shapes used by
/// clients (`SELECT a, b FROM ...`, `SELECT t.a, t.b FROM ...`).
fn sqlite_result_column_names(sql: &str) -> Vec<Option<String>> {
    let trimmed = sql.trim();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("SELECT") {
        return Vec::new();
    }
    // Find the select list (between SELECT and FROM).
    let from = upper.find(" FROM ");
    let list = match from {
        Some(p) => &trimmed[6..p],
        None => &trimmed[6..],
    };
    list
        .split(',')
        .map(|part| {
            let part = part.trim();
            // alias: `expr AS name`
            let name = if let Some(pos) = part.to_ascii_uppercase().rfind(" AS ") {
                part[pos + 4..].trim()
            } else {
                // take the last identifier path component (`t.col` -> `col`)
                let cleaned = part
                    .trim_end_matches(|c| c == ' ' || c == ';');
                cleaned.rsplit('.').next().unwrap_or(cleaned).trim()
            };
            let name = name.trim_matches(|c| c == '"' || c == '`' || c == '\'' || c == ' ');
            if name.is_empty() || name.contains('(') || name.contains('*') {
                None
            } else {
                Some(name.to_string())
            }
        })
        .collect()
}

/// Result of a translated query including whether anything odd happened.
#[derive(Default)]
pub struct SessionConfig {
    pub sqlite_busy_timeout_ms: Option<u64>,
}

#[allow(dead_code)]
pub(crate) fn _unused(_: Arc<Mutex<()>>, _: Duration) {}
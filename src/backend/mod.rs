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
    let res = sqlx::query(sqlx::AssertSqlSafe(sql))
        .execute(&mut *conn)
        .await
        .map_err(|e| Error::Backend(format!("{e} (sql: {})", snippet(sql))))?;
    Ok((res.rows_affected(), res.last_insert_id()))
}

/// Fetch all rows for a query.
pub async fn fetch(
    conn: &mut AnyConnection,
    sql: &str,
) -> Result<(Vec<FieldMeta>, Vec<Vec<Option<Value>>>)> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| Error::Backend(format!("{e} (sql: {})", snippet(sql))))?;

    if rows.is_empty() {
        return Ok((vec![], vec![]));
    }

    // Derive column metadata from the decoded values so it matches what we
    // will encode on the wire (SQLite reports unknown kinds for expressions).
    let cols: Vec<FieldMeta> = (0..rows[0].columns().len())
        .map(|idx| {
            let name = rows[0].columns()[idx].name().to_string();
            let kind = value_kind(&decode_cell(&rows[0], idx).unwrap_or(None));
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
    let (cols, rows) = fetch(conn, &wrapped).await?;
    if !rows.is_empty() {
        return Ok(cols);
    }
    let forced = format!(
        "SELECT t.* FROM (SELECT 1) AS __sqr_dummy LEFT JOIN ({sql}) t ON 0 = 1 LIMIT 1"
    );
    let (cols2, rows2) = fetch(conn, &forced).await?;
    if !rows2.is_empty() {
        Ok(cols2)
    } else {
        Ok(cols)
    }
}

fn snippet(sql: &str) -> String {
    if sql.chars().count() > 200 {
        format!("{}...", sql.chars().take(200).collect::<String>())
    } else {
        sql.to_string()
    }
}

/// Result of a translated query including whether anything odd happened.
#[derive(Default)]
pub struct SessionConfig {
    pub sqlite_busy_timeout_ms: Option<u64>,
}

#[allow(dead_code)]
pub(crate) fn _unused(_: Arc<Mutex<()>>, _: Duration) {}
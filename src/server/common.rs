//! Shared per-connection session state and SQL execution for wire handlers.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use sqlx::AnyConnection;
use sqlparser::ast::Statement;
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect};
use sqlparser::parser::Parser;
use tokio::sync::Mutex;

use crate::backend::{self, Backend, FieldMeta, Value};
use crate::error::Result;
use crate::translate::{FrontDialect, TargetDialect, Translator};

use super::intercept;

/// Per-wire-connection session: a dedicated backend connection plus the
/// translator configured for this listener.
#[derive(Clone)]
pub struct Session {
    pub backend: Backend,
    pub conn: Arc<Mutex<Option<AnyConnection>>>,
    pub translator: Translator,
    /// Last auto-generated id for this session (MySQL `LAST_INSERT_ID()`).
    pub last_insert_id: Arc<AtomicI64>,
}

impl Session {
    pub fn new(backend: Backend, translator: Translator) -> Session {
        Session {
            backend,
            conn: Arc::new(Mutex::new(None)),
            translator,
            last_insert_id: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Lazily acquire the dedicated backend connection for this wire session.
    pub async fn connection(&self) -> Result<tokio::sync::MutexGuard<'_, Option<AnyConnection>>> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.backend.acquire_owned().await?);
        }
        Ok(guard)
    }

    pub fn set_last_insert_id(&self, id: i64) {
        if id > 0 {
            self.last_insert_id.store(id, Ordering::Relaxed);
        }
    }

    pub fn get_last_insert_id(&self) -> i64 {
        self.last_insert_id.load(Ordering::Relaxed)
    }
}

/// The result of running one (already-translated) statement.
pub enum Outcome {
    Rows {
        fields: Vec<FieldMeta>,
        rows: Vec<Vec<Option<Value>>>,
    },
    Affected {
        rows_affected: u64,
        last_insert_id: Option<i64>,
        command: String,
    },
}

fn parse_dialect(front: FrontDialect) -> Box<dyn Dialect> {
    match front {
        FrontDialect::Postgres => Box::new(PostgreSqlDialect {}),
        FrontDialect::Mysql => Box::new(MySqlDialect {}),
    }
}

/// True if the statement returns a result set to the client.
fn returns_rows(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Query(_)
            | Statement::ShowVariable { .. }
            | Statement::ShowColumns { .. }
            | Statement::ShowTables { .. }
            | Statement::ShowFunctions { .. }
            | Statement::ShowCollation { .. }
    )
}

/// Parse a query string into its component statements using the front dialect.
pub fn split_statements(front: FrontDialect, sql: &str) -> Result<Vec<Statement>> {
    let dialect = parse_dialect(front);
    Parser::parse_sql(&*dialect, sql)
        .map_err(|e| crate::error::Error::Parse(format!("{e} (sql: {})", snippet(sql))))
}

/// Run one or more statements against the session's backend connection,
/// returning one `Outcome` per statement. Statements that need emulation on
/// the proxy side (`SHOW ...`, `LAST_INSERT_ID()`, swallowed `SET`) are
/// intercepted before hitting the backend.
pub async fn run_script(
    session: &Session,
    front: FrontDialect,
    sql: &str,
) -> Result<Vec<Outcome>> {
    let stmts = split_statements(front, sql)?;
    if stmts.is_empty() {
        return Ok(vec![]);
    }

    let mut conn_guard = session.connection().await?;
    let mut outcomes = Vec::with_capacity(stmts.len());
    for stmt in stmts {
        // Proxy-side interception (synthetic results / no-ops).
        if let Some(outcome) =
            intercept::intercept_statement(session, conn_guard.as_mut().unwrap(), &stmt).await?
        {
            outcomes.push(outcome);
            continue;
        }

        let translated = if session.translator.front_dialect_matches_target() {
            stmt.to_string()
        } else {
            session.translator.translate_statement(&stmt)?
        };

        let outcome = if returns_rows(&stmt) {
            let (fields, rows) = backend::fetch(conn_guard.as_mut().unwrap(), &translated).await?;
            Outcome::Rows { fields, rows }
        } else {
            let (rows_affected, last_insert_id) =
                backend::execute(conn_guard.as_mut().unwrap(), &translated).await?;

            // Track auto-generated ids for INSERT/REPLACE so that MySQL-style
            // `LAST_INSERT_ID()` keeps working across dialects.
            if matches!(stmt, Statement::Insert(..)) {
                match session.backend.dialect() {
                    TargetDialect::Postgres => {
                        // PG does not report generated ids in the OK packet;
                        // query `lastval()` best-effort and ignore failures
                        // (inserts that did not touch a sequence).
                        if let Ok((_, rows)) =
                            backend::fetch(conn_guard.as_mut().unwrap(), "SELECT lastval()")
                                .await
                        {
                            if let Some(Some(Value::BigInt(id))) =
                                rows.into_iter().next().and_then(|mut r| {
                                    if r.len() == 1 {
                                        Some(r.pop().flatten())
                                    } else {
                                        None
                                    }
                                })
                            {
                                session.set_last_insert_id(id);
                            }
                        }
                    }
                    _ => {
                        session.set_last_insert_id(last_insert_id.unwrap_or(0));
                    }
                }
            }

            let command = statement_command_name(&stmt, translated.split(' ').next().unwrap_or(""));
            Outcome::Affected {
                rows_affected,
                last_insert_id,
                command,
            }
        };
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Best-effort command tag name (e.g. "SELECT", "INSERT", "CREATE TABLE").
fn statement_command_name(stmt: &Statement, translated_first_word: &str) -> String {
    match stmt {
        Statement::Insert(..) => "INSERT".to_string(),
        Statement::Update(..) => "UPDATE".to_string(),
        Statement::Delete(..) => "DELETE".to_string(),
        Statement::CreateTable(..) => "CREATE TABLE".to_string(),
        Statement::Drop { .. } => "DROP".to_string(),
        Statement::StartTransaction { .. } => "BEGIN".to_string(),
        Statement::Commit { .. } => "COMMIT".to_string(),
        Statement::Rollback { .. } => "ROLLBACK".to_string(),
        Statement::CreateIndex(..) => "CREATE INDEX".to_string(),
        Statement::Set(..) => "SET".to_string(),
        Statement::Query(_) => "SELECT".to_string(),
        _ => {
            let first = translated_first_word.to_uppercase();
            if first.is_empty() {
                "OK".to_string()
            } else {
                first
            }
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

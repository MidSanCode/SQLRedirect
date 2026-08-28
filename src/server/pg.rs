//! PostgreSQL front-end wire handler.
//!
//! Speaks the PostgreSQL simple and extended query protocols with client apps,
//! translates SQL to the target dialect, and forwards to the real backend via
//! a dedicated per-connection backend connection (so `BEGIN`/`COMMIT` actually
//! map to real transactions on the target).

use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::sink::Sink;
use futures::{Stream, stream};

use pgwire::api::auth::cleartext::CleartextPasswordAuthStartupHandler;
use pgwire::api::auth::{
    AuthSource, DefaultServerParameterProvider, LoginInfo, Password, StartupHandler,
};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireServerHandlers, Type};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;

use crate::backend::rows::kind_to_pg_oid;
use crate::backend::{Backend, FieldMeta, Value};
use crate::error::{Error, Result};
use crate::server::common::{Outcome, Session, run_script};
use crate::server::pgparams::infer_param_types;
use crate::translate::{
    FrontDialect, TargetDialect, Translator, quote_string_literal, substitute_placeholders,
};

/// Handlers for a single PostgreSQL listener.
#[derive(Clone)]
pub struct PgHandlers {
    backend: Backend,
    translator: Translator,
    auth: TokenAuth,
}

/// Cleartext auth source: validates user/password from config.
#[derive(Debug, Clone)]
pub struct TokenAuth {
    user: Option<String>,
    password: Option<String>,
}

#[async_trait]
impl AuthSource for TokenAuth {
    async fn get_password(&self, login: &LoginInfo) -> PgWireResult<Password> {
        if let Some(ref expected) = self.user {
            if login.user() != Some(expected.as_str()) {
                return Err(PgWireError::InvalidPassword(
                    login.user().unwrap_or("").to_owned(),
                ));
            }
        }
        let pwd = self.password.clone().unwrap_or_default();
        Ok(Password::new(None, pwd.into_bytes()))
    }
}

impl PgHandlers {
    pub fn new(
        backend: Backend,
        front: FrontDialect,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<PgHandlers> {
        let translator = Translator::new(front, backend.dialect());
        Ok(PgHandlers {
            backend,
            translator,
            auth: TokenAuth {
                user: username,
                password,
            },
        })
    }

    /// Run the accept loop for this listener.
    pub async fn serve(self: Arc<Self>, addr: std::net::SocketAddr) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        self.serve_on(listener).await
    }

    /// Run the accept loop on an already-bound listener (lets callers bind
    /// ephemeral ports and learn the address first).
    pub async fn serve_on(
        self: Arc<Self>,
        listener: tokio::net::TcpListener,
    ) -> Result<()> {
        let addr = listener.local_addr()?;
        tracing::info!("listening postgres on {addr}");
        loop {
            let (socket, _peer) = listener.accept().await?;
            let handlers = self.clone();
            tokio::spawn(async move {
                if let Err(e) = process_socket(socket, None, handlers).await {
                    tracing::debug!("postgres connection closed: {e}");
                }
            });
        }
    }

    async fn get_session<C>(&self, client: &C) -> PgWireResult<Session>
    where
        C: ClientInfo,
    {
        Ok(match client.session_extensions().get::<Session>() {
            Some(s) => (*s).clone(),
            None => {
                let s = Session::new(self.backend.clone(), self.translator.clone());
                client.session_extensions().insert(s.clone());
                s
            }
        })
    }
}

impl PgWireServerHandlers for PgHandlers {
    fn simple_query_handler(&self) -> Arc<impl SimpleQueryHandler> {
        Arc::new(self.clone())
    }
    fn extended_query_handler(&self) -> Arc<impl ExtendedQueryHandler> {
        Arc::new(self.clone())
    }
    fn startup_handler(&self) -> Arc<impl StartupHandler> {
        Arc::new(CleartextPasswordAuthStartupHandler::new(
            self.auth.clone(),
            DefaultServerParameterProvider::default(),
        ))
    }
}

/// Convert an error into a wire protocol error, sanitizing control characters
/// (NUL bytes would break the ErrorResponse cstring framing).
fn to_pgerr(e: Error) -> PgWireError {
    let sanitize = |s: String| -> String {
        s.chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect()
    };
    match &e {
        Error::Parse(_) | Error::Translate(_) | Error::Config(_) | Error::Unsupported(_) => {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "42000".to_owned(),
                sanitize(e.to_string()),
            )))
        }
        _ => PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "XX000".to_owned(),
            sanitize(e.to_string()),
        ))),
    }
}

/// Derive the result-set schema for extended-protocol Describe responses.
///
/// The translated statement is probed on the backend with the driver's
/// describe API (side-effect free, works for zero-row results). Failures
/// report no columns; clients then rely on execution-time RowDescription.
async fn probe_result_schema(session: &Session, sql: &str) -> Vec<FieldInfo> {
    let stmts = match crate::server::common::split_statements(FrontDialect::Postgres, sql) {
        Ok(s) if s.len() == 1 => s,
        _ => return vec![],
    };
    let stmt = &stmts[0];
    let is_select = matches!(stmt, sqlparser::ast::Statement::Query(_));
    let returning = match stmt {
        sqlparser::ast::Statement::Insert(i) => i.returning.is_some(),
        sqlparser::ast::Statement::Update(u) => u.returning.is_some(),
        sqlparser::ast::Statement::Delete(d) => d.returning.is_some(),
        _ => false,
    };
    if !is_select && !returning {
        return vec![];
    }

    let translated = match session.translator.translate_statement(stmt) {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    // Neutralize $n placeholders so the probe parses on the backend.
    let n_params = max_positional_placeholder(&translated);
    let probe_sql =
        substitute_placeholders(&translated, &vec!["NULL".to_string(); n_params]).unwrap_or(translated);

    let mut conn = match session.connection().await {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    match crate::backend::describe(conn.as_mut().unwrap(), &probe_sql).await {
        Ok(fields) => fields.iter().map(field_info).collect(),
        Err(_) => vec![],
    }
}

/// Highest `$n` index appearing in `sql` (0 when none).
fn max_positional_placeholder(sql: &str) -> usize {
    let bytes = sql.as_bytes();
    let (mut i, n) = (0usize, bytes.len());
    let mut max = 0usize;
    while i < n {
        if bytes[i] == b'$' {
            let mut k = i + 1;
            while k < n && bytes[k].is_ascii_digit() {
                k += 1;
            }
            if k > i + 1 {
                if let Ok(v) = sql[i + 1..k].parse::<usize>() {
                    max = max.max(v);
                }
                i = k;
                continue;
            }
        }
        i += 1;
    }
    max
}

fn field_info(meta: &FieldMeta) -> FieldInfo {
    let oid = kind_to_pg_oid(&meta.kind);
    let ty = Type::from_oid(oid as u32).unwrap_or(Type::TEXT);
    FieldInfo::new(
        meta.name.clone(),
        None,
        None,
        ty,
        FieldFormat::Binary,
    )
}

fn encode_rows(
    schema: Arc<Vec<FieldInfo>>,
    rows: &[Vec<Option<Value>>],
) -> impl Stream<Item = PgWireResult<DataRow>> + use<> {
    let mut results = Vec::with_capacity(rows.len());
    let ncols = schema.len();
    let mut encoder = DataRowEncoder::new(schema.clone());
    for row in rows {
        for idx in 0..ncols {
            // Encode according to the *declared column type* (the wire OID),
            // coercing the portable value to that width so the client decodes
            // it correctly. SQLite stores every integer as i64 even for an
            // `INTEGER` (INT4) column, so we downcast when the wire says int4.
            let field_type = schema[idx].datatype();
            match &row[idx] {
                None | Some(Value::Null) => {
                    // Encode NULL with the field's own type so pgwire does not
                    // reject a type mismatch (e.g. `None::<String>` on an INT4
                    // column).
                    match *field_type {
                        Type::BOOL => encoder
                            .encode_field(&None::<bool>)
                            .expect("encoding null bool"),
                        Type::INT2 => encoder
                            .encode_field(&None::<i16>)
                            .expect("encoding null int2"),
                        Type::INT4 => encoder
                            .encode_field(&None::<i32>)
                            .expect("encoding null int4"),
                        Type::INT8 => encoder
                            .encode_field(&None::<i64>)
                            .expect("encoding null int8"),
                        Type::FLOAT4 => encoder
                            .encode_field(&None::<f32>)
                            .expect("encoding null float4"),
                        Type::FLOAT8 => encoder
                            .encode_field(&None::<f64>)
                            .expect("encoding null float8"),
                        Type::TEXT => encoder
                            .encode_field(&None::<String>)
                            .expect("encoding null text"),
                        Type::BYTEA => encoder
                            .encode_field(&None::<Vec<u8>>)
                            .expect("encoding null bytea"),
                        _ => encoder
                            .encode_field(&None::<String>)
                            .expect("encoding null fallback"),
                    }
                }
                Some(Value::Bool(b)) => encoder.encode_field(b).expect("encoding bool"),
                Some(v) => match *field_type {
                    Type::BOOL => encoder
                        .encode_field(&v.as_bool().unwrap_or(false))
                        .expect("encoding bool"),
                    Type::INT2 => encoder
                        .encode_field(&v.as_i16().unwrap_or(0))
                        .expect("encoding int2"),
                    Type::INT4 => encoder
                        .encode_field(&v.as_i32().unwrap_or(0))
                        .expect("encoding int4"),
                    Type::INT8 => encoder
                        .encode_field(&v.as_i64().unwrap_or(0))
                        .expect("encoding int8"),
                    Type::FLOAT4 => encoder
                        .encode_field(&v.as_f32().unwrap_or(0.0))
                        .expect("encoding float4"),
                    Type::FLOAT8 => encoder
                        .encode_field(&v.as_f64().unwrap_or(0.0))
                        .expect("encoding float8"),
                    Type::TEXT | Type::BYTEA | _ => {
                        let text = v.as_pg_text().unwrap_or_default();
                        encoder.encode_field(&text).expect("encoding text")
                    }
                },
            }
        }
        results.push(Ok(encoder.take_row()));
    }
    stream::iter(results)
}

fn response_for(_session: &Session, outcome: &Outcome) -> Response {
    match outcome {
        Outcome::Rows { fields, rows } => {
            let schema = Arc::new(fields.iter().map(field_info).collect::<Vec<_>>());
            let stream = encode_rows(schema.clone(), rows);
            Response::Query(QueryResponse::new(schema, stream))
        }
        Outcome::Affected {
            rows_affected,
            command,
            ..
        } => {
            // PG command tags: DML carries a row count ("INSERT 0 3"), DDL
            // stands alone ("CREATE TABLE").
            let tag = match command.as_str() {
                "INSERT" => format!("INSERT 0 {rows_affected}"),
                "UPDATE" | "DELETE" | "MOVE" | "FETCH" => {
                    format!("{command} {rows_affected}")
                }
                other => other.to_string(),
            };
            Response::Execution(Tag::new(&tag))
        }
    }
}

#[async_trait]
impl SimpleQueryHandler for PgHandlers {
    async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.get_session(client).await?;
        let outcomes = match run_script(&session, FrontDialect::Postgres, query).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("simple query failed: {e} (sql: {query})");
                return Err(to_pgerr(e));
            }
        };
        Ok(outcomes
            .iter()
            .map(|o| response_for(&session, o))
            .collect())
    }
}

/// Resolve the effective wire types for a portal's parameters: client-declared
/// OIDs when present, otherwise re-run inference (matching what Describe
/// reported so the client encoded values accordingly).
async fn effective_param_types(
    session: &Session,
    portal: &Portal<String>,
) -> PgWireResult<Vec<Type>> {
    let declared = &portal.statement.parameter_types;
    if !declared.iter().all(|t| t.is_none()) {
        return Ok(declared
            .iter()
            .map(|t| t.clone().unwrap_or(Type::UNKNOWN))
            .collect());
    }
    let mut conn = session.connection().await.map_err(to_pgerr)?;
    Ok(
        infer_param_types(&session, conn.as_mut().unwrap(), portal.statement.statement.as_str())
            .await,
    )
}

/// Convert parameter values carried by a portal into front-dialect SQL literals,
/// so the rewritten statement can be executed directly on the backend.
fn params_as_literals(portal: &Portal<String>, types: &[Type]) -> PgWireResult<Vec<String>> {
    let mut out = Vec::with_capacity(portal.parameter_len());
    for i in 0..portal.parameter_len() {
        let declared = types.get(i).cloned().unwrap_or(Type::TEXT);
        let literal = match &declared {
            &Type::BOOL => match portal.parameter::<bool>(i, &Type::BOOL)? {
                Some(true) => "TRUE".to_string(),
                Some(false) => "FALSE".to_string(),
                None => "NULL".to_string(),
            },
            &Type::INT2 => portal
                .parameter::<i16>(i, &Type::INT2)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            &Type::INT4 => portal
                .parameter::<i32>(i, &Type::INT4)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            &Type::INT8 => portal
                .parameter::<i64>(i, &Type::INT8)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            &Type::FLOAT4 => portal
                .parameter::<f32>(i, &Type::FLOAT4)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            &Type::FLOAT8 => portal
                .parameter::<f64>(i, &Type::FLOAT8)?
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".to_string()),
            _ => match portal.parameter::<String>(i, &Type::TEXT)? {
                Some(s) => {
                    quote_string_literal(&s, TargetDialect::Postgres)
                }
                None => "NULL".to_string(),
            },
        };
        out.push(literal);
    }
    Ok(out)
}

#[async_trait]
impl ExtendedQueryHandler for PgHandlers {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        Arc::new(NoopQueryParser)
    }

    async fn do_query<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.get_session(client).await?;
        let sql = portal.statement.statement.as_str();
        let types = effective_param_types(&session, portal).await?;
        let literals = params_as_literals(portal, &types)?;
        let sql = substitute_placeholders(sql, &literals).map_err(to_pgerr)?;
        tracing::debug!("extended query: {sql}");
        let outcomes = match run_script(&session, FrontDialect::Postgres, &sql).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("extended query failed: {e} (sql: {sql})");
                return Err(to_pgerr(e));
            }
        };
        let outcome = outcomes
            .into_iter()
            .next()
            .unwrap_or(Outcome::Affected {
                rows_affected: 0,
                last_insert_id: None,
                command: "OK".to_string(),
            });
        Ok(response_for(&session, &outcome))
    }

    async fn do_describe_statement<C>(
        &self,
        client: &mut C,
        target: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.get_session(client).await?;
        let all_unspecified = target.parameter_types.iter().all(|t| t.is_none());

        let param_types = if all_unspecified {
            // No usable OIDs from the client: infer from SQL context and the
            // target catalog.
            let mut conn = session.connection().await.map_err(to_pgerr)?;
            infer_param_types(&session, conn.as_mut().unwrap(), target.statement.as_str())
                .await
        } else {
            target
                .parameter_types
                .iter()
                .map(|t| t.clone().unwrap_or(Type::UNKNOWN))
                .collect()
        };
        let result_schema = probe_result_schema(&session, target.statement.as_str()).await;
        Ok(DescribeStatementResponse::new(param_types, result_schema))
    }

    async fn do_describe_portal<C>(
        &self,
        client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let session = self.get_session(client).await?;
        let schema = probe_result_schema(&session, portal.statement.statement.as_str()).await;
        Ok(DescribePortalResponse::new(schema))
    }
}

#[allow(unused)]
fn _unused(v: &DataRow) {
    let _ = v;
}
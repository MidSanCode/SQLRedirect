//! MySQL front-end wire handler.
//!
//! Speaks the MySQL wire protocol with client apps (via mysql-mimic), translates
//! SQL to the target dialect, and forwards to the real backend over a dedicated
//! per-connection connection.

use std::sync::Arc;

use mysql_mimic::connection::ConnectionInfo;
use mysql_mimic::identity::{StaticIdentityProvider, User};
use mysql_mimic::result_set::{Column, ColumnType, ResultSet};
use mysql_mimic::{MysqlError, MysqlServer, Session as MySqlSession, SessionFactory};

use crate::backend::rows::kind_to_mysql_type;
use crate::backend::{Backend, FieldMeta};
use crate::error::{Error, Result};
use crate::server::common::{Outcome, Session, run_script};
use crate::translate::{FrontDialect, Translator};

/// Handler state for a single MySQL listener.
pub struct MySqlHandler {
    backend: Backend,
    translator: Translator,
    username: Option<String>,
    password: Option<String>,
}

impl MySqlHandler {
    pub fn new(
        backend: Backend,
        front: FrontDialect,
        username: Option<String>,
        password: Option<String>,
    ) -> MySqlHandler {
        let dialect = backend.dialect();
        MySqlHandler {
            backend,
            translator: Translator::new(front, dialect),
            username,
            password,
        }
    }

    /// Run the accept loop for this listener.
    pub async fn serve(self: Arc<Self>, addr: std::net::SocketAddr) -> Result<()> {
        let factory = MyFactory {
            server: Arc::clone(&self),
        };
        let addr = addr.to_string();
        let result = match (&self.username, &self.password) {
            (Some(name), pwd) => {
                let users = vec![User::with_password(
                    name.clone(),
                    pwd.clone().unwrap_or_default(),
                )];
                MysqlServer::with_identity_provider(factory, StaticIdentityProvider::new(users))
                    .listen(&addr)
                    .await
            }
            (None, _) => MysqlServer::new(factory).listen(&addr).await,
        };
        tracing::info!("listening mysql on {addr}");
        result.map_err(|e| Error::Server(format!("mysql listener failed: {e}")))?;
        Ok(())
    }
}

struct MyFactory {
    server: Arc<MySqlHandler>,
}

impl SessionFactory for MyFactory {
    type S = MySession;

    async fn create_session(&self) -> std::result::Result<Self::S, MysqlError> {
        Ok(MySession {
            session: Session::new(
                self.server.backend.clone(),
                self.server.translator.clone(),
            ),
        })
    }
}

fn to_mysql_err(e: crate::error::Error) -> MysqlError {
    use crate::error::Error::*;
    match e {
        Parse(_) | Translate(_) => MysqlError::server(1064, "42000", e.to_string()),
        Unsupported(_) => MysqlError::server(1235, "42000", e.to_string()),
        Backend(_) => MysqlError::server(1105, "HY000", e.to_string()),
        _ => MysqlError::server(1105, "HY000", e.to_string()),
    }
}

struct MySession {
    session: Session,
}

fn mysql_column_type(meta: &FieldMeta) -> ColumnType {
    match kind_to_mysql_type(&meta.kind) {
        1 => ColumnType::Tiny,
        2 => ColumnType::Short,
        3 => ColumnType::Long,
        4 => ColumnType::Float,
        5 => ColumnType::Double,
        6 => ColumnType::Null,
        8 => ColumnType::LongLong,
        9 => ColumnType::Int24,
        252 => ColumnType::Blob,
        253 => ColumnType::VarString,
        254 => ColumnType::String,
        _ => ColumnType::VarString,
    }
}

impl MySqlSession for MySession {
    fn init(&mut self, _info: &ConnectionInfo) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    async fn handle_query(
        &mut self,
        query: &str,
    ) -> std::result::Result<ResultSet, MysqlError> {
        let outcomes = run_script(&self.session, FrontDialect::Mysql, query)
            .await
            .map_err(to_mysql_err)?;

        let outcome = outcomes
            .into_iter()
            .next()
            .unwrap_or(Outcome::Affected {
                rows_affected: 0,
                last_insert_id: None,
                command: "OK".to_string(),
            });

        match outcome {
            Outcome::Rows { fields, rows } => {
                let columns: Vec<Column> = fields.iter().map(|f| {
                    Column::new(f.name.clone(), mysql_column_type(f))
                }).collect();
                let mut result = ResultSet::new(columns);
                for row in &rows {
                    result.add_row(
                        row.iter()
                            .map(|v| v.as_ref().and_then(|val| val.as_mysql_text()))
                            .collect(),
                    );
                }
                Ok(result)
            }
            Outcome::Affected { .. } => Ok(ResultSet::empty()),
        }
    }
}
//! MySQL server implementation.
//!
//! Provides [`MysqlServer`] which accepts TCP connections and handles
//! the MySQL wire protocol using a user-provided [`SessionFactory`] and
//! [`IdentityProvider`].

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::BufReader;
use tokio::io::BufWriter;
use tokio::net::TcpListener;

use crate::connection::Connection;
use crate::error::MysqlError;
use crate::identity::{IdentityProvider, SimpleIdentityProvider};
use crate::session::SessionFactory;
use crate::variables::{GlobalVariables, SessionVariables};

/// A MySQL-protocol-compatible TCP server.
///
/// # Example
///
/// ```rust,no_run
/// use mysql_mimic::{MysqlServer, Session, SessionFactory, ResultSet};
/// use mysql_mimic::error::MysqlError;
/// use mysql_mimic::connection::ConnectionInfo;
///
/// struct MySession;
/// impl Session for MySession {
///     async fn handle_query(&mut self, _query: &str) -> Result<ResultSet, MysqlError> {
///         Ok(ResultSet::empty())
///     }
/// }
///
/// struct MyFactory;
/// impl SessionFactory for MyFactory {
///     type S = MySession;
///     async fn create_session(&self) -> Result<MySession, MysqlError> {
///         Ok(MySession)
///     }
/// }
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let server = MysqlServer::new(MyFactory);
///     server.listen("127.0.0.1:3306").await?;
///     Ok(())
/// }
/// ```
pub struct MysqlServer<F: SessionFactory, I: IdentityProvider = SimpleIdentityProvider> {
    factory: Arc<F>,
    identity_provider: Arc<I>,
    global_variables: Arc<GlobalVariables>,
}

impl<F: SessionFactory> MysqlServer<F, SimpleIdentityProvider> {
    /// Create a new MySQL server with the given session factory.
    ///
    /// Uses [`SimpleIdentityProvider`] which accepts all connections.
    pub fn new(factory: F) -> Self {
        MysqlServer {
            factory: Arc::new(factory),
            identity_provider: Arc::new(SimpleIdentityProvider),
            global_variables: Arc::new(GlobalVariables::new()),
        }
    }
}

impl<F: SessionFactory, I: IdentityProvider> MysqlServer<F, I> {
    /// Create a new MySQL server with a custom identity provider.
    pub fn with_identity_provider(factory: F, identity_provider: I) -> Self {
        MysqlServer {
            factory: Arc::new(factory),
            identity_provider: Arc::new(identity_provider),
            global_variables: Arc::new(GlobalVariables::new()),
        }
    }

    /// Set the global variables for all connections.
    pub fn set_global_variables(mut self, globals: GlobalVariables) -> Self {
        self.global_variables = Arc::new(globals);
        self
    }

    /// Start listening for MySQL client connections on the given address.
    ///
    /// This function runs forever, accepting connections and spawning a task
    /// for each one.
    pub async fn listen(&self, addr: &str) -> Result<(), MysqlError> {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!("MySQL server listening on {}", addr);

        loop {
            let (stream, peer_addr) = listener.accept().await?;
            tracing::info!("New connection from {}", peer_addr);

            let factory = Arc::clone(&self.factory);
            let identity = Arc::clone(&self.identity_provider);
            let globals = Arc::clone(&self.global_variables);
            tokio::spawn(async move {
                if let Err(e) =
                    handle_connection(factory, identity, globals, stream, peer_addr).await
                {
                    tracing::error!("Connection error from {}: {}", peer_addr, e);
                }
            });
        }
    }
}

/// Handle a single client connection.
async fn handle_connection<F: SessionFactory, I: IdentityProvider>(
    factory: Arc<F>,
    identity_provider: Arc<I>,
    globals: Arc<GlobalVariables>,
    stream: tokio::net::TcpStream,
    _peer_addr: SocketAddr,
) -> Result<(), MysqlError> {
    let session = factory.create_session().await?;
    let variables = SessionVariables::new(globals);

    let (reader, writer) = stream.into_split();
    let reader = BufReader::new(reader);
    let writer = BufWriter::new(writer);

    let mut conn = Connection::new(reader, writer, session, identity_provider, variables);
    conn.run().await
}

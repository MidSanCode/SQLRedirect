//! Session trait for handling client queries.
//!
//! Implement [`Session`] to define how your server handles SQL queries
//! and other MySQL commands.

use crate::connection::ConnectionInfo;
use crate::error::MysqlError;
use crate::result_set::ResultSet;

/// A session represents a single client connection's state and query handler.
///
/// Implement this trait to provide custom query handling logic. The server
/// calls lifecycle methods (`init`, `on_close`, `on_reset`) and dispatch
/// methods (`handle_query`, `handle_init_db`) as commands arrive from the
/// connected MySQL client.
pub trait Session: Send + 'static {
    /// Called after the handshake completes, before the command phase starts.
    ///
    /// Receives the [`ConnectionInfo`] for the connected client so the session
    /// can store the connection ID, username, etc.
    ///
    /// The default implementation does nothing.
    fn init(&mut self, _info: &ConnectionInfo) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Handle a SQL query and return a result set.
    ///
    /// Called for each `COM_QUERY` or interpolated `COM_STMT_EXECUTE` received
    /// from the client, **after** built-in middleware (SET, USE, SHOW, etc.)
    /// has been applied.
    fn handle_query(
        &mut self,
        query: &str,
    ) -> impl std::future::Future<Output = Result<ResultSet, MysqlError>> + Send;

    /// Handle a `COM_INIT_DB` (USE database) command.
    ///
    /// The default implementation always succeeds.
    fn handle_init_db(
        &mut self,
        _database: &str,
    ) -> impl std::future::Future<Output = Result<(), MysqlError>> + Send {
        async { Ok(()) }
    }

    /// Called when the connection is reset (COM_RESET_CONNECTION or COM_CHANGE_USER).
    ///
    /// The default implementation does nothing.
    fn on_reset(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Called when the client disconnects.
    ///
    /// The default implementation does nothing.
    fn on_close(&mut self) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }
}

/// A factory for creating new [`Session`] instances.
///
/// The server calls [`create_session`](SessionFactory::create_session) for each
/// new client connection.
pub trait SessionFactory: Send + Sync + 'static {
    /// The session type this factory creates.
    type S: Session;

    /// Create a new session for an incoming connection.
    fn create_session(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::S, MysqlError>> + Send;
}

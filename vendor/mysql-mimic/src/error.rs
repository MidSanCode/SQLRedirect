//! Error types for the MySQL mimic library.

/// The main error type for the library.
#[derive(Debug, thiserror::Error)]
pub enum MysqlError {
    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A protocol-level error occurred.
    #[error("Protocol error: {0}")]
    Protocol(String),

    /// An error occurred during authentication.
    #[error("Authentication failed: {0}")]
    Auth(String),

    /// The SQL query could not be parsed.
    #[error("SQL parse error: {0}")]
    SqlParse(String),

    /// A user-defined error to send back to the client.
    #[error("MySQL error {code}: {message}")]
    Server {
        /// MySQL error code.
        code: u16,
        /// SQL state (5 chars).
        sql_state: String,
        /// Human-readable error message.
        message: String,
    },

    /// An internal error.
    #[error("Internal error: {0}")]
    Internal(String),
}

impl MysqlError {
    /// Create a new server error with the given code, state, and message.
    pub fn server(code: u16, sql_state: impl Into<String>, message: impl Into<String>) -> Self {
        MysqlError::Server {
            code,
            sql_state: sql_state.into(),
            message: message.into(),
        }
    }

    /// Create an "unknown command" error.
    pub fn unknown_command(cmd: u8) -> Self {
        MysqlError::server(1047, "08S01", format!("Unknown command: {cmd}"))
    }

    /// Returns the MySQL error code for this error.
    pub fn error_code(&self) -> u16 {
        match self {
            MysqlError::Server { code, .. } => *code,
            MysqlError::Auth(_) => 1045,
            MysqlError::SqlParse(_) => 1064,
            _ => 1105, // ER_UNKNOWN_ERROR
        }
    }

    /// Returns the SQL state for this error.
    pub fn sql_state(&self) -> &str {
        match self {
            MysqlError::Server { sql_state, .. } => sql_state,
            MysqlError::Auth(_) => "28000",
            MysqlError::SqlParse(_) => "42000",
            _ => "HY000",
        }
    }

    /// Returns the error message.
    pub fn error_message(&self) -> String {
        match self {
            MysqlError::Server { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }
}

impl From<sqlparser::parser::ParserError> for MysqlError {
    fn from(e: sqlparser::parser::ParserError) -> Self {
        MysqlError::SqlParse(e.to_string())
    }
}

//! Connection management.
//!
//! Manages the lifecycle of a single MySQL client connection, including
//! handshake, authentication, command dispatch, and prepared statements.
//! This mirrors the Python library's `Connection` class.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::MysqlError;
use crate::identity::{AuthResult, IdentityProvider};
use crate::prepared::{self, PreparedStatement};
use crate::protocol::auth;
use crate::protocol::constants::*;
use crate::protocol::packet::{
    build_eof_packet, build_err_packet, build_ok_packet, read_packet, write_packet,
};
use crate::protocol::write_lenenc_int;
use crate::result_set::ResultSet;
use crate::session::Session;
use crate::variables::{SessionVariables, Value};

/// Global connection ID counter.
static CONNECTION_ID_SEQ: AtomicU32 = AtomicU32::new(1);

/// Information about the connected client, available to the session.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// Unique connection ID assigned by the server.
    pub connection_id: u32,
    /// Client username from handshake.
    pub username: String,
    /// Currently selected database.
    pub database: Option<String>,
    /// Client-provided connect attributes.
    pub connect_attrs: HashMap<String, String>,
}

impl ConnectionInfo {
    fn new(connection_id: u32) -> Self {
        ConnectionInfo {
            connection_id,
            username: String::new(),
            database: None,
            connect_attrs: HashMap::new(),
        }
    }
}

/// Manages a single MySQL client connection.
pub struct Connection<S: Session, I: IdentityProvider, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>
{
    reader: R,
    writer: W,
    session: S,
    identity_provider: Arc<I>,
    variables: SessionVariables,
    info: ConnectionInfo,
    scramble: [u8; 20],
    /// Prepared statements keyed by statement ID.
    prepared_stmts: HashMap<u32, PreparedStatement>,
    /// Next prepared statement ID.
    next_stmt_id: u32,
    /// Negotiated capability flags (server & client intersection).
    negotiated_capabilities: u32,
}

impl<S: Session, I: IdentityProvider, R: AsyncRead + Unpin, W: AsyncWrite + Unpin>
    Connection<S, I, R, W>
{
    /// Create a new connection.
    pub fn new(
        reader: R,
        writer: W,
        session: S,
        identity_provider: Arc<I>,
        variables: SessionVariables,
    ) -> Self {
        let connection_id = CONNECTION_ID_SEQ.fetch_add(1, Ordering::Relaxed);
        Connection {
            reader,
            writer,
            session,
            identity_provider,
            variables,
            info: ConnectionInfo::new(connection_id),
            scramble: auth::generate_scramble(),
            prepared_stmts: HashMap::new(),
            next_stmt_id: 1,
            negotiated_capabilities: 0,
        }
    }

    /// Get the connection info.
    pub fn info(&self) -> &ConnectionInfo {
        &self.info
    }

    /// Get the session variables.
    pub fn variables(&self) -> &SessionVariables {
        &self.variables
    }

    /// Get a mutable reference to the session variables.
    pub fn variables_mut(&mut self) -> &mut SessionVariables {
        &mut self.variables
    }

    /// Run the full connection lifecycle: handshake + command phase.
    pub async fn run(&mut self) -> Result<(), MysqlError> {
        self.connection_phase().await?;
        self.session.init(&self.info).await;
        let result = self.command_phase().await;
        self.session.on_close().await;
        result
    }

    /// Perform the MySQL handshake.
    async fn connection_phase(&mut self) -> Result<(), MysqlError> {
        // 1. Send HandshakeV10
        self.send_handshake().await?;

        // 2. Read handshake response
        let response_pkt = read_packet(&mut self.reader).await?;
        let handshake = self.parse_handshake_response(&response_pkt.payload)?;

        self.info.username = handshake.username.clone();
        self.info.database = handshake.database.clone();
        self.info.connect_attrs = handshake.connect_attrs.clone();
        self.negotiated_capabilities = handshake.client_capabilities & DEFAULT_SERVER_CAPABILITIES;

        // 3. Set session variables from handshake
        self.variables
            .set("external_user", Value::String(handshake.username.clone()));
        if let Some(ref db) = handshake.database {
            self.info.database = Some(db.clone());
        }

        // 4. Authenticate
        //    If the client used a different auth plugin (e.g., caching_sha2_password),
        //    send an AuthSwitchRequest to switch to mysql_native_password.
        let mut seq = response_pkt.sequence_id.wrapping_add(1);

        let auth_response = if handshake.client_plugin.as_deref() != Some(auth::AUTH_PLUGIN_NAME) {
            tracing::debug!(
                "Auth plugin mismatch: client={:?}, server={}. Sending AuthSwitchRequest.",
                handshake.client_plugin,
                auth::AUTH_PLUGIN_NAME
            );

            // Build AuthSwitchRequest packet: 0xFE + plugin_name(null) + scramble_data
            let mut switch_buf = BytesMut::new();
            switch_buf.put_u8(0xFE); // AuthSwitchRequest marker
            switch_buf.extend_from_slice(auth::AUTH_PLUGIN_NAME.as_bytes());
            switch_buf.put_u8(0); // null terminator for plugin name
            switch_buf.extend_from_slice(&self.scramble);
            switch_buf.put_u8(0); // null terminator for scramble

            write_packet(&mut self.writer, &mut seq, &switch_buf).await?;

            // Read the client's new auth response
            let switch_response = read_packet(&mut self.reader).await?;
            seq = switch_response.sequence_id.wrapping_add(1);
            switch_response.payload.to_vec()
        } else {
            handshake.auth_response.clone()
        };

        if let Some(user) = self.identity_provider.get_user(&handshake.username).await {
            let result = self
                .identity_provider
                .authenticate(&user, &self.scramble, &auth_response)
                .await;
            match result {
                AuthResult::Success => {}
                AuthResult::Denied(msg) => {
                    let err = build_err_packet(1045, "28000", &msg);
                    write_packet(&mut self.writer, &mut seq, &err).await?;
                    return Err(MysqlError::Auth(msg));
                }
            }
        } else {
            let msg = format!("Access denied for user '{}'", handshake.username);
            let err = build_err_packet(1045, "28000", &msg);
            write_packet(&mut self.writer, &mut seq, &err).await?;
            return Err(MysqlError::Auth(msg));
        }

        // 5. Send OK
        let ok = build_ok_packet(0, 0);
        write_packet(&mut self.writer, &mut seq, &ok).await?;

        tracing::debug!(
            "Handshake complete: connection_id={}, user={}",
            self.info.connection_id,
            self.info.username
        );

        Ok(())
    }

    /// Run the command phase loop.
    async fn command_phase(&mut self) -> Result<(), MysqlError> {
        loop {
            let pkt = match read_packet(&mut self.reader).await {
                Ok(pkt) => pkt,
                Err(MysqlError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    tracing::debug!(
                        "Client disconnected: connection_id={}",
                        self.info.connection_id
                    );
                    break;
                }
                Err(e) => return Err(e),
            };

            let mut seq = pkt.sequence_id.wrapping_add(1);

            if pkt.payload.is_empty() {
                continue;
            }

            let command = pkt.payload[0];
            let args = &pkt.payload[1..];

            let result = match command {
                COM_QUIT => break,
                COM_PING => self.handle_ping(&mut seq).await,
                COM_INIT_DB => self.handle_init_db(args, &mut seq).await,
                COM_QUERY => self.handle_query(args, &mut seq).await,
                COM_FIELD_LIST => self.handle_field_list(args, &mut seq).await,
                COM_STMT_PREPARE => self.handle_stmt_prepare(args, &mut seq).await,
                COM_STMT_EXECUTE => self.handle_stmt_execute(args, &mut seq).await,
                COM_STMT_CLOSE => self.handle_stmt_close(args).await,
                COM_STMT_RESET => self.handle_stmt_reset(args, &mut seq).await,
                COM_STMT_SEND_LONG_DATA => self.handle_stmt_send_long_data(args).await,
                COM_STMT_FETCH => self.handle_stmt_fetch(args, &mut seq).await,
                COM_RESET_CONNECTION => self.handle_reset_connection(&mut seq).await,
                COM_CHANGE_USER => self.handle_change_user(args, &mut seq).await,
                COM_SET_OPTION => self.handle_set_option(args, &mut seq).await,
                COM_DEBUG => self.handle_debug(&mut seq).await,
                _ => {
                    let e = MysqlError::unknown_command(command);
                    self.send_error(&e, &mut seq).await
                }
            };

            if let Err(e) = result {
                // Try to send error to client, but don't fail if we can't
                let _ = self.send_error(&e, &mut seq).await;
            }
        }
        Ok(())
    }

    // --- Command handlers ---

    async fn handle_ping(&mut self, seq: &mut u8) -> Result<(), MysqlError> {
        let ok = build_ok_packet(0, 0);
        write_packet(&mut self.writer, seq, &ok).await
    }

    async fn handle_init_db(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        let db = String::from_utf8_lossy(args).to_string();
        match self.session.handle_init_db(&db).await {
            Ok(()) => {
                self.info.database = Some(db);
                let ok = build_ok_packet(0, 0);
                write_packet(&mut self.writer, seq, &ok).await
            }
            Err(e) => {
                self.send_error(&e, seq).await?;
                Ok(())
            }
        }
    }

    async fn handle_query(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        let query = String::from_utf8_lossy(args).to_string();
        tracing::debug!(
            "COM_QUERY from connection_id={}: {}",
            self.info.connection_id,
            query
        );

        // Support multi-statement: split by `;`
        let statements = split_statements(&query);
        let mut last_result = None;

        for stmt_sql in &statements {
            let trimmed = stmt_sql.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Handle built-in commands before delegating to session
            if let Some(result) = self.handle_builtin_query(trimmed).await? {
                last_result = Some(result);
            } else {
                match self.session.handle_query(trimmed).await {
                    Ok(rs) => last_result = Some(rs),
                    Err(e) => {
                        self.send_error(&e, seq).await?;
                        return Ok(());
                    }
                }
            }
        }

        match last_result {
            Some(result_set) => self.send_result_set(&result_set, seq, false).await,
            None => {
                let ok = build_ok_packet(0, 0);
                write_packet(&mut self.writer, seq, &ok).await
            }
        }
    }

    /// Handle built-in queries that the server intercepts before passing to the session.
    async fn handle_builtin_query(&mut self, sql: &str) -> Result<Option<ResultSet>, MysqlError> {
        // Strip C-style comments (e.g., /* mysql-connector-j-8.2.0 ... */)
        // that JDBC drivers prepend to queries.
        let stripped = strip_c_comments(sql);
        let sql = stripped.trim();
        let upper = sql.to_uppercase();
        let trimmed = upper.trim();

        // SET variable handling
        if trimmed.starts_with("SET ") {
            self.handle_set_statement(sql)?;
            return Ok(Some(ResultSet::empty()));
        }

        // USE database
        if trimmed.starts_with("USE ") {
            let db = sql[4..]
                .trim()
                .trim_matches('`')
                .trim_matches('\'')
                .trim_matches('"');
            self.session.handle_init_db(db).await?;
            self.info.database = Some(db.to_string());
            return Ok(Some(ResultSet::empty()));
        }

        // BEGIN/START TRANSACTION
        if trimmed == "BEGIN" || trimmed.starts_with("START TRANSACTION") {
            return Ok(Some(ResultSet::empty()));
        }

        // COMMIT
        if trimmed == "COMMIT" {
            return Ok(Some(ResultSet::empty()));
        }

        // ROLLBACK
        if trimmed == "ROLLBACK" {
            return Ok(Some(ResultSet::empty()));
        }

        // SHOW VARIABLES
        if trimmed.starts_with("SHOW") && trimmed.contains("VARIABLES") {
            return Ok(Some(self.handle_show_variables(sql)));
        }

        // SHOW WARNINGS
        if trimmed.starts_with("SHOW WARNINGS") {
            return Ok(Some(ResultSet::new(vec![
                crate::result_set::Column::new("Level", crate::result_set::ColumnType::VarString),
                crate::result_set::Column::new("Code", crate::result_set::ColumnType::Long),
                crate::result_set::Column::new("Message", crate::result_set::ColumnType::VarString),
            ])));
        }

        // SHOW ERRORS
        if trimmed.starts_with("SHOW ERRORS") {
            return Ok(Some(ResultSet::new(vec![
                crate::result_set::Column::new("Level", crate::result_set::ColumnType::VarString),
                crate::result_set::Column::new("Code", crate::result_set::ColumnType::Long),
                crate::result_set::Column::new("Message", crate::result_set::ColumnType::VarString),
            ])));
        }

        // SHOW STATUS
        if trimmed.starts_with("SHOW") && trimmed.contains("STATUS") {
            return Ok(Some(ResultSet::new(vec![
                crate::result_set::Column::new(
                    "Variable_name",
                    crate::result_set::ColumnType::VarString,
                ),
                crate::result_set::Column::new("Value", crate::result_set::ColumnType::VarString),
            ])));
        }

        // SELECT @@variable
        if trimmed.starts_with("SELECT") && upper.contains("@@") {
            if let Some(rs) = self.handle_select_variables(sql) {
                return Ok(Some(rs));
            }
        }

        // SELECT DATABASE()
        if trimmed.contains("DATABASE()") || trimmed.contains("SCHEMA()") {
            if let Some(rs) = self.handle_select_functions(sql) {
                return Ok(Some(rs));
            }
        }

        // SELECT CONNECTION_ID()
        if trimmed.contains("CONNECTION_ID()") {
            if let Some(rs) = self.handle_select_functions(sql) {
                return Ok(Some(rs));
            }
        }

        // SELECT USER() / CURRENT_USER()
        if trimmed.contains("USER()") || trimmed.contains("CURRENT_USER()") {
            if let Some(rs) = self.handle_select_functions(sql) {
                return Ok(Some(rs));
            }
        }

        // SELECT VERSION()
        if trimmed.contains("VERSION()") {
            if let Some(rs) = self.handle_select_functions(sql) {
                return Ok(Some(rs));
            }
        }

        // Static SELECT (e.g., SELECT 1, SELECT 'hello')
        if trimmed.starts_with("SELECT ") && !trimmed.contains("FROM") {
            if let Some(rs) = self.handle_static_select(sql) {
                return Ok(Some(rs));
            }
        }

        Ok(None)
    }

    /// Handle SET statements.
    fn handle_set_statement(&mut self, sql: &str) -> Result<(), MysqlError> {
        let upper = sql.to_uppercase();
        let body = &sql[4..].trim();

        // SET NAMES
        if upper[4..].trim().starts_with("NAMES") {
            let parts: Vec<&str> = body[5..].split_whitespace().collect();
            let charset_name = parts
                .first()
                .map(|s| s.trim_matches('\'').trim_matches('"'))
                .unwrap_or("utf8mb4");
            let charset = if charset_name.eq_ignore_ascii_case("DEFAULT") {
                "utf8mb4"
            } else {
                charset_name
            };
            self.variables
                .set("character_set_client", Value::String(charset.into()));
            self.variables
                .set("character_set_connection", Value::String(charset.into()));
            self.variables
                .set("character_set_results", Value::String(charset.into()));
            // Handle optional COLLATE
            if let Some(collate_pos) = upper.find("COLLATE") {
                let collation = sql[collate_pos + 7..]
                    .trim()
                    .trim_matches('\'')
                    .trim_matches('"');
                self.variables
                    .set("collation_connection", Value::String(collation.into()));
            } else if charset.eq_ignore_ascii_case("DEFAULT") {
                self.variables.set(
                    "collation_connection",
                    Value::String("utf8mb4_general_ci".into()),
                );
            }
            return Ok(());
        }

        // SET CHARSET / SET CHARACTER SET
        if upper[4..].trim().starts_with("CHARSET")
            || upper[4..].trim().starts_with("CHARACTER SET")
        {
            let start = if upper[4..].trim().starts_with("CHARACTER SET") {
                body.find("SET").map(|p| p + 3).unwrap_or(0)
            } else {
                7
            };
            let charset_name = body[start..].trim().trim_matches('\'').trim_matches('"');
            let charset = if charset_name.eq_ignore_ascii_case("DEFAULT") {
                "utf8mb4"
            } else {
                charset_name
            };
            self.variables
                .set("character_set_client", Value::String(charset.into()));
            self.variables
                .set("character_set_results", Value::String(charset.into()));
            return Ok(());
        }

        // SET variable = value (with possible multiple assignments)
        self.parse_set_assignments(body)
    }

    /// Parse `var=val, var=val` assignments.
    fn parse_set_assignments(&mut self, body: &str) -> Result<(), MysqlError> {
        // Simple parser: split by commas, then by `=`
        for assignment in body.split(',') {
            let assignment = assignment.trim();
            if assignment.is_empty() {
                continue;
            }

            let parts: Vec<&str> = assignment.splitn(2, '=').collect();
            if parts.len() != 2 {
                continue; // Skip unparseable
            }

            let var_part = parts[0].trim();
            let val_part = parts[1].trim();

            // Strip @@, @@SESSION., @@LOCAL., @@GLOBAL.
            let var_name = var_part
                .trim_start_matches("@@SESSION.")
                .trim_start_matches("@@session.")
                .trim_start_matches("@@LOCAL.")
                .trim_start_matches("@@local.")
                .trim_start_matches("@@GLOBAL.")
                .trim_start_matches("@@global.")
                .trim_start_matches("@@");

            let value = parse_set_value(val_part, &self.variables);
            self.variables.set(var_name.to_string(), value);
        }
        Ok(())
    }

    /// Handle SHOW [SESSION|GLOBAL] VARIABLES [LIKE 'pattern'].
    fn handle_show_variables(&self, sql: &str) -> ResultSet {
        let like_pattern = extract_like_pattern(sql);
        let vars = self.variables.list();

        let mut rs = ResultSet::new(vec![
            crate::result_set::Column::new(
                "Variable_name",
                crate::result_set::ColumnType::VarString,
            ),
            crate::result_set::Column::new("Value", crate::result_set::ColumnType::VarString),
        ]);

        for (name, value) in &vars {
            if let Some(ref pattern) = like_pattern {
                if !like_match(pattern, name) {
                    continue;
                }
            }
            rs.add_row(vec![Some(name.clone()), value.to_mysql_string()]);
        }

        rs
    }

    /// Handle SELECT @@variable_name queries.
    ///
    /// Supports `SELECT @@var`, `SELECT @@session.var`, and
    /// `SELECT @@var AS alias` forms that JDBC drivers commonly use.
    fn handle_select_variables(&self, sql: &str) -> Option<ResultSet> {
        // Extract all @@var references
        let mut columns = Vec::new();
        let mut values = Vec::new();

        // Naive extraction: find @@identifier patterns
        let mut i = 0;
        let bytes = sql.as_bytes();
        while i < bytes.len() - 1 {
            if bytes[i] == b'@' && bytes[i + 1] == b'@' {
                let start = i;
                i += 2;
                // Skip optional scope prefix (SESSION., GLOBAL.)
                let remaining = &sql[i..];
                let skip = if remaining.to_uppercase().starts_with("SESSION.") {
                    8
                } else if remaining.to_uppercase().starts_with("GLOBAL.") {
                    7
                } else if remaining.to_uppercase().starts_with("LOCAL.") {
                    6
                } else {
                    0
                };
                i += skip;

                // Read variable name
                let name_start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let var_name = &sql[name_start..i];
                let full_ref = &sql[start..i];

                // Check for AS alias: skip whitespace, look for AS keyword
                let col_name = {
                    let after = &sql[i..];
                    let trimmed_after = after.trim_start();
                    if trimmed_after.len() >= 3
                        && trimmed_after[..2].eq_ignore_ascii_case("AS")
                        && trimmed_after
                            .as_bytes()
                            .get(2)
                            .is_some_and(|&b| b == b' ' || b == b'\t')
                    {
                        // Skip "AS "
                        let alias_start_str = trimmed_after[2..].trim_start();
                        let alias_end = alias_start_str
                            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                            .unwrap_or(alias_start_str.len());
                        let alias = &alias_start_str[..alias_end];
                        if !alias.is_empty() {
                            alias.to_string()
                        } else {
                            full_ref.to_string()
                        }
                    } else {
                        full_ref.to_string()
                    }
                };

                let value = self.variables.get(var_name);
                columns.push(crate::result_set::Column::new(
                    &col_name,
                    crate::result_set::ColumnType::VarString,
                ));
                values.push(value.and_then(|v| v.to_mysql_string()));
            } else {
                i += 1;
            }
        }

        if columns.is_empty() {
            return None;
        }

        let mut rs = ResultSet::new(columns);
        rs.add_row(values);
        Some(rs)
    }

    /// Handle SELECT with MySQL functions like DATABASE(), USER(), VERSION(), CONNECTION_ID().
    fn handle_select_functions(&self, sql: &str) -> Option<ResultSet> {
        let upper = sql.to_uppercase();
        let mut columns = Vec::new();
        let mut values = Vec::new();

        // Simple function extraction
        #[allow(clippy::type_complexity)]
        let functions: &[(&str, Box<dyn Fn(&Self) -> Option<String>>)] = &[
            (
                "DATABASE()",
                Box::new(|conn: &Self| conn.info.database.clone()),
            ),
            (
                "SCHEMA()",
                Box::new(|conn: &Self| conn.info.database.clone()),
            ),
            (
                "CONNECTION_ID()",
                Box::new(|conn: &Self| Some(conn.info.connection_id.to_string())),
            ),
            (
                "USER()",
                Box::new(|conn: &Self| {
                    Some(
                        conn.variables
                            .get("external_user")
                            .and_then(|v| v.to_mysql_string())
                            .unwrap_or_else(|| conn.info.username.clone()),
                    )
                }),
            ),
            (
                "CURRENT_USER()",
                Box::new(|conn: &Self| Some(conn.info.username.clone())),
            ),
            (
                "VERSION()",
                Box::new(|conn: &Self| {
                    conn.variables
                        .get("version")
                        .and_then(|v| v.to_mysql_string())
                }),
            ),
        ];

        for (func_name, resolver) in functions {
            if upper.contains(func_name) {
                columns.push(crate::result_set::Column::new(
                    *func_name,
                    crate::result_set::ColumnType::VarString,
                ));
                values.push(resolver(self));
            }
        }

        if columns.is_empty() {
            return None;
        }

        let mut rs = ResultSet::new(columns);
        rs.add_row(values);
        Some(rs)
    }

    /// Handle static SELECT queries like `SELECT 1`, `SELECT 'hello' AS greeting`.
    fn handle_static_select(&self, sql: &str) -> Option<ResultSet> {
        let upper = sql.to_uppercase();
        // Only handle simple numeric/string literals
        let select_body = sql[7..].trim();

        // Quick check: if it contains FROM, subqueries, etc., let session handle it
        if upper.contains("FROM") || upper.contains("WHERE") || upper.contains("JOIN") {
            return None;
        }

        // Parse comma-separated expressions
        let exprs: Vec<&str> = select_body.split(',').collect();
        let mut columns = Vec::new();
        let mut values = Vec::new();

        for expr in &exprs {
            let expr = expr.trim();
            // Check for AS alias
            let (value_part, alias) = if let Some(as_pos) = expr.to_uppercase().rfind(" AS ") {
                (expr[..as_pos].trim(), expr[as_pos + 4..].trim().to_string())
            } else {
                (expr, expr.to_string())
            };

            // Parse the value
            if let Ok(num) = value_part.parse::<i64>() {
                columns.push(crate::result_set::Column::new(
                    &alias,
                    crate::result_set::ColumnType::LongLong,
                ));
                values.push(Some(num.to_string()));
            } else if (value_part.starts_with('\'') && value_part.ends_with('\''))
                || (value_part.starts_with('"') && value_part.ends_with('"'))
            {
                let s = &value_part[1..value_part.len() - 1];
                columns.push(crate::result_set::Column::new(
                    &alias,
                    crate::result_set::ColumnType::VarString,
                ));
                values.push(Some(s.to_string()));
            } else if value_part.eq_ignore_ascii_case("NULL") {
                columns.push(crate::result_set::Column::new(
                    &alias,
                    crate::result_set::ColumnType::Null,
                ));
                values.push(None);
            } else if value_part.eq_ignore_ascii_case("TRUE") {
                columns.push(crate::result_set::Column::new(
                    &alias,
                    crate::result_set::ColumnType::Tiny,
                ));
                values.push(Some("1".to_string()));
            } else if value_part.eq_ignore_ascii_case("FALSE") {
                columns.push(crate::result_set::Column::new(
                    &alias,
                    crate::result_set::ColumnType::Tiny,
                ));
                values.push(Some("0".to_string()));
            } else {
                // Can't handle this expression statically
                return None;
            }
        }

        if columns.is_empty() {
            return None;
        }

        let mut rs = ResultSet::new(columns);
        rs.add_row(values);
        Some(rs)
    }

    async fn handle_field_list(&mut self, _args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        // COM_FIELD_LIST is deprecated but some clients use it
        // Send an empty result (EOF)
        let eof = build_eof_packet();
        write_packet(&mut self.writer, seq, &eof).await
    }

    async fn handle_stmt_prepare(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        let sql = String::from_utf8_lossy(args).to_string();
        let stmt_id = self.next_stmt_id;
        self.next_stmt_id = self.next_stmt_id.wrapping_add(1);

        let stmt = PreparedStatement::new(stmt_id, sql);
        let ok_data = prepared::build_stmt_prepare_ok(&stmt);

        write_packet(&mut self.writer, seq, &ok_data).await?;

        // Send parameter column definitions if there are params
        if stmt.num_params > 0 {
            for _ in 0..stmt.num_params {
                let col =
                    crate::result_set::Column::new("?", crate::result_set::ColumnType::VarString);
                let col_data = ResultSet::serialize_column(&col);
                write_packet(&mut self.writer, seq, &col_data).await?;
            }
            let eof = build_eof_packet();
            write_packet(&mut self.writer, seq, &eof).await?;
        }

        self.prepared_stmts.insert(stmt_id, stmt);
        Ok(())
    }

    async fn handle_stmt_execute(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        if args.len() < 4 {
            return Err(MysqlError::Protocol("COM_STMT_EXECUTE too short".into()));
        }

        let stmt_id = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);

        let stmt = self
            .prepared_stmts
            .get(&stmt_id)
            .ok_or_else(|| {
                MysqlError::server(1243, "HY000", format!("Unknown statement: {stmt_id}"))
            })?
            .clone();

        // Parse parameters
        let params = prepared::parse_stmt_execute_data(args, &stmt)?;

        // Interpolate parameters into SQL
        let sql = stmt.interpolate(&params)?;

        tracing::debug!(
            "COM_STMT_EXECUTE from connection_id={}: {}",
            self.info.connection_id,
            sql
        );

        // Handle built-in queries
        if let Some(result) = self.handle_builtin_query(&sql).await? {
            self.send_result_set(&result, seq, true).await?;
        } else {
            match self.session.handle_query(&sql).await {
                Ok(result_set) => {
                    self.send_result_set(&result_set, seq, true).await?;
                }
                Err(e) => {
                    self.send_error(&e, seq).await?;
                }
            }
        }

        // Clear param buffers
        if let Some(stmt) = self.prepared_stmts.get_mut(&stmt_id) {
            stmt.param_buffers = None;
        }

        Ok(())
    }

    async fn handle_stmt_close(&mut self, args: &[u8]) -> Result<(), MysqlError> {
        if args.len() < 4 {
            return Ok(()); // No response for COM_STMT_CLOSE
        }
        let stmt_id = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
        self.prepared_stmts.remove(&stmt_id);
        Ok(())
    }

    async fn handle_stmt_reset(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        if args.len() < 4 {
            return Err(MysqlError::Protocol("COM_STMT_RESET too short".into()));
        }
        let stmt_id = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
        if let Some(stmt) = self.prepared_stmts.get_mut(&stmt_id) {
            stmt.param_buffers = None;
        }
        let ok = build_ok_packet(0, 0);
        write_packet(&mut self.writer, seq, &ok).await
    }

    async fn handle_stmt_send_long_data(&mut self, args: &[u8]) -> Result<(), MysqlError> {
        if args.len() < 6 {
            return Ok(()); // No response
        }
        let stmt_id = u32::from_le_bytes([args[0], args[1], args[2], args[3]]);
        let param_id = u16::from_le_bytes([args[4], args[5]]);
        let data = &args[6..];

        if let Some(stmt) = self.prepared_stmts.get_mut(&stmt_id) {
            let buffers = stmt.param_buffers.get_or_insert_with(HashMap::new);
            buffers
                .entry(param_id)
                .or_insert_with(Vec::new)
                .extend_from_slice(data);
        }
        Ok(()) // No response
    }

    async fn handle_stmt_fetch(&mut self, _args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        // Fetch is for cursor-based reading, which we don't fully support yet.
        // Send end-of-data.
        let eof = build_eof_packet();
        write_packet(&mut self.writer, seq, &eof).await
    }

    async fn handle_reset_connection(&mut self, seq: &mut u8) -> Result<(), MysqlError> {
        self.variables.reset_all();
        self.session.on_reset().await;
        let ok = build_ok_packet(0, 0);
        write_packet(&mut self.writer, seq, &ok).await
    }

    async fn handle_change_user(&mut self, args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        // Parse COM_CHANGE_USER: username (null-terminated), auth_response, database
        let mut pos = 0;

        // Username
        let null_pos = args[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| MysqlError::Protocol("missing null in COM_CHANGE_USER".into()))?;
        let username = String::from_utf8_lossy(&args[pos..pos + null_pos]).to_string();
        pos += null_pos + 1;

        // Auth response length + data
        if pos >= args.len() {
            return Err(MysqlError::Protocol("COM_CHANGE_USER truncated".into()));
        }
        let auth_len = args[pos] as usize;
        pos += 1;
        let auth_response = if pos + auth_len <= args.len() {
            &args[pos..pos + auth_len]
        } else {
            &[]
        };
        pos += auth_len;

        // Database (null-terminated)
        let database = if pos < args.len() {
            let null_pos = args[pos..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(args.len() - pos);
            let db = String::from_utf8_lossy(&args[pos..pos + null_pos]).to_string();
            if db.is_empty() {
                None
            } else {
                Some(db)
            }
        } else {
            None
        };

        // Authenticate
        if let Some(user) = self.identity_provider.get_user(&username).await {
            let result = self
                .identity_provider
                .authenticate(&user, &self.scramble, auth_response)
                .await;
            match result {
                AuthResult::Success => {
                    self.info.username = username;
                    self.info.database = database;
                    self.variables.reset_all();
                    self.session.on_reset().await;
                    let ok = build_ok_packet(0, 0);
                    write_packet(&mut self.writer, seq, &ok).await
                }
                AuthResult::Denied(msg) => {
                    let err = build_err_packet(1045, "28000", &msg);
                    write_packet(&mut self.writer, seq, &err).await
                }
            }
        } else {
            let msg = format!("Access denied for user '{username}'");
            let err = build_err_packet(1045, "28000", &msg);
            write_packet(&mut self.writer, seq, &err).await
        }
    }

    async fn handle_set_option(&mut self, _args: &[u8], seq: &mut u8) -> Result<(), MysqlError> {
        // COM_SET_OPTION: 2-byte option (0 = multi-statements ON, 1 = OFF)
        let eof = build_eof_packet();
        write_packet(&mut self.writer, seq, &eof).await
    }

    async fn handle_debug(&mut self, seq: &mut u8) -> Result<(), MysqlError> {
        let ok = build_ok_packet(0, 0);
        write_packet(&mut self.writer, seq, &ok).await
    }

    // --- Helpers ---

    async fn send_error(&mut self, e: &MysqlError, seq: &mut u8) -> Result<(), MysqlError> {
        let err = build_err_packet(e.error_code(), e.sql_state(), &e.error_message());
        write_packet(&mut self.writer, seq, &err).await
    }

    async fn send_result_set(
        &mut self,
        result_set: &ResultSet,
        seq: &mut u8,
        binary: bool,
    ) -> Result<(), MysqlError> {
        if result_set.is_empty() && result_set.rows.is_empty() {
            let lid = result_set.last_insert_id.unwrap_or(0);
            let ok = build_ok_packet(0, lid);
            return write_packet(&mut self.writer, seq, &ok).await;
        }

        // Column count
        let mut count_buf = BytesMut::new();
        write_lenenc_int(&mut count_buf, result_set.columns.len() as u64);
        write_packet(&mut self.writer, seq, &count_buf).await?;

        // Column definitions
        for col in &result_set.columns {
            let col_data = ResultSet::serialize_column(col);
            write_packet(&mut self.writer, seq, &col_data).await?;
        }

        // EOF after columns (if not deprecating EOF)
        if self.negotiated_capabilities & CLIENT_DEPRECATE_EOF == 0 {
            let eof = build_eof_packet();
            write_packet(&mut self.writer, seq, &eof).await?;
        }

        // Rows
        for row in &result_set.rows {
            let row_data = if binary {
                ResultSet::serialize_row_binary(row, &result_set.columns)
            } else {
                ResultSet::serialize_row(row)
            };
            write_packet(&mut self.writer, seq, &row_data).await?;
        }

        // EOF after rows (or OK if deprecating EOF)
        if self.negotiated_capabilities & CLIENT_DEPRECATE_EOF == 0 {
            let eof = build_eof_packet();
            write_packet(&mut self.writer, seq, &eof).await?;
        } else {
            let ok = build_ok_packet(0, 0);
            write_packet(&mut self.writer, seq, &ok).await?;
        }

        Ok(())
    }

    /// Send the initial handshake packet.
    async fn send_handshake(&mut self) -> Result<(), MysqlError> {
        let version = self
            .variables
            .get("version")
            .and_then(|v| v.to_mysql_string())
            .unwrap_or_else(|| SERVER_VERSION.to_string());

        let mut buf = BytesMut::new();

        // Protocol version
        buf.put_u8(10);

        // Server version (null-terminated)
        buf.extend_from_slice(version.as_bytes());
        buf.put_u8(0);

        // Connection ID
        buf.put_u32_le(self.info.connection_id);

        // Auth plugin data part 1 (first 8 bytes of scramble)
        buf.extend_from_slice(&self.scramble[..8]);
        buf.put_u8(0); // filler

        // Capability flags (lower 2 bytes)
        buf.put_u16_le((DEFAULT_SERVER_CAPABILITIES & 0xFFFF) as u16);

        // Character set
        buf.put_u8(DEFAULT_CHARSET);

        // Status flags
        buf.put_u16_le(SERVER_STATUS_AUTOCOMMIT);

        // Capability flags (upper 2 bytes)
        buf.put_u16_le(((DEFAULT_SERVER_CAPABILITIES >> 16) & 0xFFFF) as u16);

        // Length of auth plugin data (always 21 for mysql_native_password)
        buf.put_u8(21);

        // Reserved (10 zero bytes)
        buf.extend_from_slice(&[0u8; 10]);

        // Auth plugin data part 2 (remaining 12 bytes of scramble + null terminator)
        buf.extend_from_slice(&self.scramble[8..]);
        buf.put_u8(0);

        // Auth plugin name
        buf.extend_from_slice(auth::AUTH_PLUGIN_NAME.as_bytes());
        buf.put_u8(0);

        let mut seq: u8 = 0;
        write_packet(&mut self.writer, &mut seq, &buf).await
    }

    /// Parse the handshake response from the client.
    fn parse_handshake_response(&self, payload: &[u8]) -> Result<HandshakeResponse, MysqlError> {
        if payload.len() < 32 {
            return Err(MysqlError::Protocol("handshake response too short".into()));
        }

        // Client capability flags (4 bytes)
        let client_capabilities =
            u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);

        // Max packet size (4 bytes)
        let _max_packet_size = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);

        // Character set (1 byte)
        let _charset = payload[8];

        // Skip reserved (23 bytes): payload[9..32]
        let mut pos = 32;

        // Username (null-terminated)
        let null_pos = payload[pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| MysqlError::Protocol("missing null terminator in username".into()))?;
        let username = String::from_utf8_lossy(&payload[pos..pos + null_pos]).to_string();
        pos += null_pos + 1;

        // Auth response
        let auth_response = if client_capabilities & CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            // Length-encoded
            if pos >= payload.len() {
                Vec::new()
            } else {
                let len = payload[pos] as usize;
                pos += 1;
                if pos + len <= payload.len() {
                    let data = payload[pos..pos + len].to_vec();
                    pos += len;
                    data
                } else {
                    Vec::new()
                }
            }
        } else if client_capabilities & CLIENT_SECURE_CONNECTION != 0 {
            if pos >= payload.len() {
                Vec::new()
            } else {
                let len = payload[pos] as usize;
                pos += 1;
                if pos + len <= payload.len() {
                    let data = payload[pos..pos + len].to_vec();
                    pos += len;
                    data
                } else {
                    Vec::new()
                }
            }
        } else {
            // Null-terminated
            let end = payload[pos..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len() - pos);
            let data = payload[pos..pos + end].to_vec();
            pos += end + 1;
            data
        };

        // Database (if CLIENT_CONNECT_WITH_DB)
        let database = if client_capabilities & CLIENT_CONNECT_WITH_DB != 0 && pos < payload.len() {
            let null_pos = payload[pos..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len() - pos);
            let db = String::from_utf8_lossy(&payload[pos..pos + null_pos]).to_string();
            pos += null_pos + 1;
            if db.is_empty() {
                None
            } else {
                Some(db)
            }
        } else {
            None
        };

        // Auth plugin name (if CLIENT_PLUGIN_AUTH)
        let client_plugin = if client_capabilities & CLIENT_PLUGIN_AUTH != 0 && pos < payload.len()
        {
            let null_pos = payload[pos..]
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(payload.len() - pos);
            let plugin = String::from_utf8_lossy(&payload[pos..pos + null_pos]).to_string();
            pos += null_pos + 1;
            Some(plugin)
        } else {
            None
        };

        // Connect attributes (if CLIENT_CONNECT_ATTRS)
        let connect_attrs =
            if client_capabilities & CLIENT_CONNECT_ATTRS != 0 && pos < payload.len() {
                parse_connect_attrs(&payload[pos..]).unwrap_or_default()
            } else {
                HashMap::new()
            };

        Ok(HandshakeResponse {
            client_capabilities,
            username,
            auth_response,
            database,
            connect_attrs,
            client_plugin,
        })
    }
}

/// Parsed handshake response data.
#[derive(Debug)]
struct HandshakeResponse {
    client_capabilities: u32,
    username: String,
    auth_response: Vec<u8>,
    database: Option<String>,
    connect_attrs: HashMap<String, String>,
    /// Auth plugin name the client used (if CLIENT_PLUGIN_AUTH).
    client_plugin: Option<String>,
}

/// Parse connect attributes from the handshake response.
fn parse_connect_attrs(data: &[u8]) -> Result<HashMap<String, String>, MysqlError> {
    let mut attrs = HashMap::new();
    if data.is_empty() {
        return Ok(attrs);
    }

    let mut slice: &[u8] = data;

    // Total length of all key-value pairs
    let total_len = match crate::protocol::read_lenenc_int(&mut slice) {
        Ok(len) => len as usize,
        Err(_) => return Ok(attrs),
    };

    let mut consumed = 0;
    while consumed < total_len && !slice.is_empty() {
        // Key
        let key_len = match crate::protocol::read_lenenc_int(&mut slice) {
            Ok(len) => len as usize,
            Err(_) => break,
        };
        if slice.len() < key_len {
            break;
        }
        let key = String::from_utf8_lossy(&slice[..key_len]).to_string();
        slice = &slice[key_len..];

        // Value
        let val_len = match crate::protocol::read_lenenc_int(&mut slice) {
            Ok(len) => len as usize,
            Err(_) => break,
        };
        if slice.len() < val_len {
            break;
        }
        let val = String::from_utf8_lossy(&slice[..val_len]).to_string();
        slice = &slice[val_len..];

        consumed += key_len + val_len + 2; // approximation
        attrs.insert(key, val);
    }

    Ok(attrs)
}

/// Strip leading C-style comments (`/* ... */`) from SQL.
///
/// JDBC drivers like MySQL Connector/J prepend comments such as
/// `/* mysql-connector-j-8.2.0 (...) */SELECT ...` to queries.
/// These must be removed before matching against builtin patterns.
fn strip_c_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    while s.starts_with("/*") {
        if let Some(end) = s.find("*/") {
            s = s[end + 2..].trim_start();
        } else {
            break; // Unterminated comment — leave as-is
        }
    }
    s
}

/// Split SQL by semicolons for multi-statement support.
/// Respects quoted strings.
fn split_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;
    let mut prev_char = '\0';

    for ch in sql.chars() {
        match ch {
            '\'' if !in_double_quote && !in_backtick && prev_char != '\\' => {
                in_single_quote = !in_single_quote;
                current.push(ch);
            }
            '"' if !in_single_quote && !in_backtick && prev_char != '\\' => {
                in_double_quote = !in_double_quote;
                current.push(ch);
            }
            '`' if !in_single_quote && !in_double_quote => {
                in_backtick = !in_backtick;
                current.push(ch);
            }
            ';' if !in_single_quote && !in_double_quote && !in_backtick => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    statements.push(trimmed);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
        prev_char = ch;
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        statements.push(trimmed);
    }

    statements
}

/// Extract the LIKE pattern from a SQL string.
fn extract_like_pattern(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    if let Some(pos) = upper.find("LIKE") {
        let rest = sql[pos + 4..].trim();
        // Extract the quoted pattern
        if rest.starts_with('\'') || rest.starts_with('"') {
            let quote = rest.chars().next().unwrap();
            if let Some(end) = rest[1..].find(quote) {
                return Some(rest[1..1 + end].to_string());
            }
        }
    }
    None
}

/// Match a SQL LIKE pattern against a string.
/// Supports `%` (any sequence) and `_` (any single char).
fn like_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let value = value.to_lowercase();
    like_match_recursive(pattern.as_bytes(), value.as_bytes())
}

fn like_match_recursive(pattern: &[u8], value: &[u8]) -> bool {
    match (pattern.first(), value.first()) {
        (None, None) => true,
        (Some(b'%'), _) => {
            // `%` matches zero or more characters
            // Try matching rest of pattern with current value, or skip one char in value
            like_match_recursive(&pattern[1..], value)
                || (!value.is_empty() && like_match_recursive(pattern, &value[1..]))
        }
        (Some(b'_'), Some(_)) => {
            // `_` matches exactly one character
            like_match_recursive(&pattern[1..], &value[1..])
        }
        (Some(p), Some(v)) if *p == *v => like_match_recursive(&pattern[1..], &value[1..]),
        _ => false,
    }
}

/// Parse a value from a SET statement.
fn parse_set_value(val: &str, variables: &SessionVariables) -> Value {
    let trimmed = val.trim();

    // DEFAULT
    if trimmed.eq_ignore_ascii_case("DEFAULT") {
        return Value::Null;
    }

    // NULL
    if trimmed.eq_ignore_ascii_case("NULL") {
        return Value::Null;
    }

    // @@variable reference
    if trimmed.starts_with("@@") {
        let var_name = trimmed
            .trim_start_matches("@@SESSION.")
            .trim_start_matches("@@session.")
            .trim_start_matches("@@LOCAL.")
            .trim_start_matches("@@local.")
            .trim_start_matches("@@GLOBAL.")
            .trim_start_matches("@@global.")
            .trim_start_matches("@@");
        if let Some(val) = variables.get(var_name) {
            return val.clone();
        }
        return Value::Null;
    }

    // Boolean
    if trimmed.eq_ignore_ascii_case("ON") || trimmed.eq_ignore_ascii_case("TRUE") {
        return Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("OFF") || trimmed.eq_ignore_ascii_case("FALSE") {
        return Value::Bool(false);
    }

    // Integer
    if let Ok(i) = trimmed.parse::<i64>() {
        return Value::Int(i);
    }

    // Quoted string
    if (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('"') && trimmed.ends_with('"'))
    {
        let inner = &trimmed[1..trimmed.len() - 1];
        return Value::String(inner.to_string());
    }

    // Bare identifier (treat as string)
    Value::String(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variables::GlobalVariables;

    #[test]
    fn test_split_statements() {
        let stmts = split_statements("SELECT 1; SELECT 2; SELECT 3");
        assert_eq!(stmts.len(), 3);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "SELECT 2");
        assert_eq!(stmts[2], "SELECT 3");
    }

    #[test]
    fn test_split_statements_quoted() {
        let stmts = split_statements("SELECT ';' FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT ';' FROM t");
    }

    #[test]
    fn test_extract_like_pattern() {
        let pattern = extract_like_pattern("SHOW VARIABLES LIKE 'version%'");
        assert_eq!(pattern, Some("version%".into()));
    }

    #[test]
    fn test_like_match() {
        assert!(like_match("version%", "version_comment"));
        assert!(like_match("version%", "version"));
        assert!(!like_match("version%", "sql_mode"));
        assert!(like_match("%timeout", "wait_timeout"));
        assert!(like_match("_ersion", "version"));
    }

    #[test]
    fn test_parse_set_value() {
        let globals = Arc::new(GlobalVariables::new());
        let vars = SessionVariables::new(globals);

        assert_eq!(parse_set_value("42", &vars), Value::Int(42));
        assert_eq!(
            parse_set_value("'hello'", &vars),
            Value::String("hello".into())
        );
        assert_eq!(parse_set_value("ON", &vars), Value::Bool(true));
        assert_eq!(parse_set_value("OFF", &vars), Value::Bool(false));
        assert_eq!(parse_set_value("NULL", &vars), Value::Null);
        assert_eq!(parse_set_value("DEFAULT", &vars), Value::Null);
    }
}

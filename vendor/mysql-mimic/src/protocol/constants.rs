//! MySQL protocol constants.

/// MySQL server version string.
pub const SERVER_VERSION: &str = "8.0.0-mysql-mimic";

/// Default server character set (utf8mb4).
pub const DEFAULT_CHARSET: u8 = 0x2d; // utf8mb4_general_ci

// -- Capability flags --

/// Use the improved version of Old Password Authentication.
pub const CLIENT_LONG_PASSWORD: u32 = 1;
/// Send found rows instead of affected rows in EOF_Packet.
pub const CLIENT_FOUND_ROWS: u32 = 1 << 1;
/// Longer flags in Protocol::ColumnDefinition320.
pub const CLIENT_LONG_FLAG: u32 = 1 << 2;
/// One can specify db on connect.
pub const CLIENT_CONNECT_WITH_DB: u32 = 1 << 3;
/// Server can send status flags in EOF and OK packets.
pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
/// Interactive client.
pub const CLIENT_INTERACTIVE: u32 = 1 << 10;
/// Client supports SSL.
pub const CLIENT_SSL: u32 = 1 << 11;
/// Client knows about transactions.
pub const CLIENT_TRANSACTIONS: u32 = 1 << 13;
/// 4.1 protocol authentication.
pub const CLIENT_SECURE_CONNECTION: u32 = 1 << 15;
/// Enable/disable multi-statement support.
pub const CLIENT_MULTI_STATEMENTS: u32 = 1 << 16;
/// Enable/disable multi-results.
pub const CLIENT_MULTI_RESULTS: u32 = 1 << 17;
/// Client supports plugin authentication.
pub const CLIENT_PLUGIN_AUTH: u32 = 1 << 19;
/// Client supports connection attributes.
pub const CLIENT_CONNECT_ATTRS: u32 = 1 << 20;
/// Length of auth response data can be encoded as a length-encoded integer.
pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
/// Deprecated EOF packet.
pub const CLIENT_DEPRECATE_EOF: u32 = 1 << 24;

/// Default server capability flags.
///
/// Note: CLIENT_DEPRECATE_EOF is intentionally excluded. Enabling it changes
/// how OK/EOF packets are parsed by clients (e.g., MySQL Connector/J) and
/// can cause "Index N out of bounds for length N" errors when the client
/// expects extended OK-packet fields that the server doesn't send.
pub const DEFAULT_SERVER_CAPABILITIES: u32 = CLIENT_LONG_PASSWORD
    | CLIENT_FOUND_ROWS
    | CLIENT_LONG_FLAG
    | CLIENT_CONNECT_WITH_DB
    | CLIENT_PROTOCOL_41
    | CLIENT_TRANSACTIONS
    | CLIENT_SECURE_CONNECTION
    | CLIENT_MULTI_STATEMENTS
    | CLIENT_MULTI_RESULTS
    | CLIENT_PLUGIN_AUTH
    | CLIENT_CONNECT_ATTRS
    | CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA;

// -- Status flags --

/// Server status: a transaction is active.
pub const SERVER_STATUS_AUTOCOMMIT: u16 = 0x0002;

// -- Command byte values --

/// COM_QUIT
pub const COM_QUIT: u8 = 0x01;
/// COM_INIT_DB
pub const COM_INIT_DB: u8 = 0x02;
/// COM_QUERY
pub const COM_QUERY: u8 = 0x03;
/// COM_FIELD_LIST (deprecated)
pub const COM_FIELD_LIST: u8 = 0x04;
/// COM_REFRESH
pub const COM_REFRESH: u8 = 0x07;
/// COM_STATISTICS
pub const COM_STATISTICS: u8 = 0x09;
/// COM_DEBUG
pub const COM_DEBUG: u8 = 0x0d;
/// COM_PING
pub const COM_PING: u8 = 0x0e;
/// COM_CHANGE_USER
pub const COM_CHANGE_USER: u8 = 0x11;
/// COM_STMT_PREPARE
pub const COM_STMT_PREPARE: u8 = 0x16;
/// COM_STMT_EXECUTE
pub const COM_STMT_EXECUTE: u8 = 0x17;
/// COM_STMT_SEND_LONG_DATA
pub const COM_STMT_SEND_LONG_DATA: u8 = 0x18;
/// COM_STMT_CLOSE
pub const COM_STMT_CLOSE: u8 = 0x19;
/// COM_STMT_RESET
pub const COM_STMT_RESET: u8 = 0x1a;
/// COM_SET_OPTION
pub const COM_SET_OPTION: u8 = 0x1b;
/// COM_STMT_FETCH
pub const COM_STMT_FETCH: u8 = 0x1c;
/// COM_RESET_CONNECTION
pub const COM_RESET_CONNECTION: u8 = 0x1f;

// -- Packet markers --

/// OK packet marker.
pub const OK_MARKER: u8 = 0x00;
/// EOF packet marker.
pub const EOF_MARKER: u8 = 0xfe;
/// ERR packet marker.
pub const ERR_MARKER: u8 = 0xff;

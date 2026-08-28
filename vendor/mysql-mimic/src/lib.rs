//! # mysql-mimic
//!
//! A Rust library that implements the MySQL wire protocol server-side,
//! allowing applications to act as a MySQL server.
//!
//! ## Overview
//!
//! `mysql-mimic` lets you create a TCP server that speaks the MySQL wire protocol.
//! Clients (e.g. `mysql` CLI, MySQL Workbench, application drivers) can connect
//! and issue SQL queries. You provide the query handling logic by implementing the
//! [`Session`] trait.
//!
//! ## Features
//!
//! - Full MySQL handshake with pluggable authentication ([`identity`])
//! - Session variables system with MySQL-compatible defaults ([`variables`])
//! - Prepared statement support (COM_STMT_PREPARE / EXECUTE / CLOSE) ([`prepared`])
//! - Built-in middleware for SET, USE, SHOW VARIABLES, SELECT @@, transaction stubs
//! - Multi-statement query support
//! - Connection management with unique connection IDs ([`connection`])

pub mod connection;
pub mod error;
pub mod identity;
pub mod prepared;
pub mod protocol;
pub mod result_set;
pub mod server;
pub mod session;
pub mod variables;

pub use connection::ConnectionInfo;
pub use error::MysqlError;
pub use identity::{AuthResult, IdentityProvider, SimpleIdentityProvider, User};
pub use result_set::{Column, ColumnType, ResultSet};
pub use server::MysqlServer;
pub use session::{Session, SessionFactory};
pub use variables::{GlobalVariables, SessionVariables, Value};

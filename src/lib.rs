//! sqlredirect: a database compatibility proxy.
//!
//! Each TCP listener simulates one front-end dialect (PostgreSQL or MySQL);
//! SQL coming from applications is parsed, rewritten into the target dialect,
//! and forwarded to a real PostgreSQL / MySQL / SQLite backend.

pub mod backend;
pub mod config;
pub mod error;
pub mod server;
pub mod translate;
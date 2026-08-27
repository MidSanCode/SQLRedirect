//! Front-end protocol server implementations.

pub mod common;
pub mod intercept;
pub mod mysql;
pub mod pg;
pub mod pgparams;

use std::net::SocketAddr;

use crate::config::Listener;

/// Shared listener configuration plus the parsed driver dialect names.
pub struct ListenerConfig {
    pub addr: SocketAddr,
    pub protocol: String,
}

impl ListenerConfig {
    pub fn from_listener(l: &Listener) -> crate::error::Result<Self> {
        let addr: SocketAddr = l
            .addr
            .parse()
            .map_err(|e| crate::error::Error::Config(format!("invalid addr '{}': {e}", l.addr)))?;
        Ok(ListenerConfig {
            addr,
            protocol: l.protocol.clone(),
        })
    }
}
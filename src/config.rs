//! Configuration: one or more simulated database listeners, each backed by a
//! real target database.

use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listeners: Vec<Listener>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Listener {
    /// Front-end protocol to simulate: `postgres` or `mysql`.
    pub protocol: String,
    /// TCP address to bind, e.g. `127.0.0.1:5439`.
    pub addr: String,
    /// Target backend URL: `postgres://`, `mysql://` or `sqlite://`.
    pub backend: String,
    /// Optional credential required of clients (front-end auth).
    pub username: Option<String>,
    /// Optional password required of clients.
    pub password: Option<String>,
    /// Busy timeout for SQLite backends (milliseconds).
    #[serde(default)]
    pub sqlite_busy_timeout_ms: Option<u64>,
    /// Max backend connections per pool.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    128
}

impl Config {
    pub fn load(path: &str) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read '{}': {e}", path)))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| Error::Config(format!("invalid config '{}': {e}", path)))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.listeners.is_empty() {
            return Err(Error::Config("no [listeners] defined".to_string()));
        }
        for l in &self.listeners {
            if !matches!(l.protocol.as_str(), "postgres" | "mysql") {
                return Err(Error::Config(format!(
                    "listener protocol must be 'postgres' or 'mysql', got '{}'",
                    l.protocol
                )));
            }
        }
        Ok(())
    }
}
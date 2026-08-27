//! Central error type for the SQLRedirect binary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("config error: {0}")]
    Config(String),

    #[error("sql parse error: {0}")]
    Parse(String),

    #[error("translation error: {0}")]
    Translate(String),

    #[error("backend error: {0}")]
    Backend(String),

    #[error("unsupported feature: {0}")]
    Unsupported(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("server error: {0}")]
    Server(String),
}

pub type Result<T> = std::result::Result<T, Error>;
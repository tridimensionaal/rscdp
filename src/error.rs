use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An error returned by Chrome, preserving its protocol code and optional data.
#[derive(Debug, Clone, Deserialize, Serialize, thiserror::Error)]
#[error("CDP error {code}: {message}")]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Errors exposed by the client. Remote errors remain distinguishable from I/O.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("connection closed: {0}")]
    ConnectionClosed(String),
    #[error("session closed: {0}")]
    SessionClosed(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("event listener lost {0} events")]
    Lagged(u64),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
}

pub type Result<T> = std::result::Result<T, Error>;

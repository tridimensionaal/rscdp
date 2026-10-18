use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An error returned by Chrome, preserving its protocol code and optional data.
#[derive(Debug, Clone, Deserialize, Serialize, thiserror::Error)]
#[error("CDP error {code}: {message}")]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
    /// Absent data is `None`; explicit JSON null is `Some(Value::Null)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "crate::protocol::present_json")]
    pub data: Option<Value>,
}

/// Errors exposed by the client. Remote errors remain distinguishable from I/O.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// A received command result or event payload failed typed decoding.
    /// The remote command may have succeeded; inspect `payload` without retrying.
    #[error("failed to decode {method}: {source}")]
    Decode {
        method: String,
        payload: Value,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("connection closed: {0}")]
    ConnectionClosed(String),
    /// The connection actor failed. Recipients share the original typed cause;
    /// inspect it through the Arc or traverse `std::error::Error::source`.
    /// Normal closure without a recorded failure uses `ConnectionClosed`.
    #[error("connection failed: {0}")]
    ConnectionFailed(#[source] std::sync::Arc<Error>),
    #[error("session closed: {0}")]
    SessionClosed(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("event listener lost {0} events")]
    Lagged(u64),
    /// The context disappeared or changed while preparing an operation.
    #[error("execution context changed during acquisition")]
    ExecutionContextChanged,
    /// A required element lookup found no matching node.
    #[error("element not found: {0}")]
    ElementNotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("browser launch failed: {0}")]
    Launch(String),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error("JavaScript exception: {0}")]
    JavaScript(Value),
    /// Evaluation succeeded but its result has no JSON representation.
    /// Remote IDs in this metadata are diagnostic, not owned live references.
    #[error("JavaScript result has no JSON value; use remote() to inspect its metadata")]
    NonJsonValue(Box<crate::cdp::runtime::RemoteObject>),
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("proxy authentication failed: {0}")]
    ProxyAuthentication(String),
    #[error("{source}; cleanup also failed: {cleanup}")]
    Cleanup {
        #[source]
        source: Box<Error>,
        cleanup: Box<Error>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn decode<T: serde::de::DeserializeOwned>(method: &str, payload: Value) -> Result<T> {
    T::deserialize(&payload).map_err(|source| Error::Decode {
        method: method.into(),
        payload,
        source,
    })
}

pub(crate) fn with_cleanup<T>(result: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(source), Err(cleanup)) => Err(Error::Cleanup {
            source: Box::new(source),
            cleanup: Box::new(cleanup),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decode_failure_preserves_payload_method_and_source() {
        #[derive(Debug, Deserialize)]
        enum Known {
            Current,
        }

        let error = decode::<Known>("Example.changed", json!("Future")).unwrap_err();
        assert!(std::error::Error::source(&error).is_some());
        let Error::Decode {
            method,
            payload,
            source,
        } = error
        else {
            panic!("expected contextual decode failure");
        };
        assert_eq!(method, "Example.changed");
        assert_eq!(payload, json!("Future"));
        assert!(source.is_data());
        assert_eq!(decode::<String>(&method, payload).unwrap(), "Future");
    }

    #[test]
    fn protocol_error_data_distinguishes_absent_null_and_value() {
        for wire in [
            json!({"code": -1, "message": "failed"}),
            json!({"code": -1, "message": "failed", "data": null}),
            json!({"code": -1, "message": "failed", "data": {"detail": 42}}),
        ] {
            let error: ProtocolError = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(error.data.as_ref(), wire.get("data"));
            assert_eq!(serde_json::to_value(error).unwrap(), wire);
        }
    }
}

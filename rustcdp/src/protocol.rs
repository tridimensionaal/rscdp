use serde::{Serialize, de::DeserializeOwned};

/// A CDP command and its statically associated response payload.
pub trait Command: Serialize {
    type Response: DeserializeOwned;
    const METHOD: &'static str;
}

/// A typed event payload. Subscribe before sending the command that causes it.
pub trait Event: DeserializeOwned {
    const METHOD: &'static str;
}

// Missing `any` fields use serde's default; present null must remain a JSON value.
pub(crate) fn present_json<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

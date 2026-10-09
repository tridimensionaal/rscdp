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

//! Asynchronous Chrome DevTools Protocol client and browser automation.

pub mod connection;
pub mod error;
pub mod launcher;
pub mod protocol;
pub mod session;

pub use connection::Connection;
pub use session::Session;

/// Bindings generated from the pinned Chrome DevTools Protocol schemas.
#[allow(clippy::large_enum_variant)]
pub mod cdp {
    include!(concat!(env!("OUT_DIR"), "/cdp.rs"));
}

pub use error::{Error, Result};

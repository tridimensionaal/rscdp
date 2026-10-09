//! Asynchronous Chrome DevTools Protocol client and browser automation.

#[cfg(feature = "tracing")]
mod diagnostics;

pub mod browser;
pub mod connection;
pub mod error;
pub mod launcher;
pub mod page;
pub mod protocol;
pub mod proxy;
pub mod proxy_auth;
pub mod session;

pub use browser::{Browser, BrowserContext};
pub use page::Page;

pub use connection::Connection;
pub use session::Session;

/// Bindings generated from the pinned Chrome DevTools Protocol schemas.
#[allow(clippy::large_enum_variant)]
pub mod cdp {
    include!(concat!(env!("OUT_DIR"), "/cdp.rs"));
}

pub use error::{Error, Result};

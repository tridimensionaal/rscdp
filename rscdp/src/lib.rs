//! Async Chrome and Chromium automation with typed Chrome DevTools Protocol bindings.
//!
//! Use [`Browser`] and [`Page`] for automation, or [`Connection`] and [`Session`]
//! for typed [`cdp`] commands and events. See [`Browser`] for process lifetime
//! and cleanup behavior.
//!
//! # Example
//!
//! This application opens a Chrome or Chromium window. Set
//! `CHROME_PATH` if the executable is not discoverable. Add Tokio with its `rt`
//! and `macros` features to use the single-thread runtime shown here.
//! For headless mode, add `.headless(true)` before `.launch()`.
//!
//! ```no_run
//! use rscdp::{Browser, Page, Result};
//! use std::time::Duration;
//!
//! async fn print_title(page: &Page) -> Result<()> {
//!     page.navigate("data:text/html,<title>Library demo</title>")
//!         .timeout(Duration::from_secs(10))
//!         .send()
//!         .await?;
//!     println!("{}", page.evaluate("document.title").json::<String>().await?);
//!     Ok(())
//! }
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<()> {
//!     let browser = Browser::builder().launch().await?;
//!     print_title(browser.page()).await?;
//!     Ok(())
//! }
//! ```
//!
//! # Diagnostics
//!
//! Enable the optional `tracing` feature for CDP request diagnostics. The library
//! emits debug events on the `rscdp` target; configure a tracing subscriber in
//! your application to collect them. It does not install a subscriber itself.

#[cfg(feature = "tracing")]
mod diagnostics;

pub mod browser;
pub mod connection;
pub mod error;
pub mod launcher;
pub mod page;
pub mod protocol;
pub mod proxy;
pub use proxy::auth as proxy_auth;
pub mod session;

pub use browser::{Browser, BrowserBuilder, BrowserContext};
pub use page::Page;

pub use connection::Connection;
pub use session::Session;

/// Bindings generated locally, without network access, from the pinned CDP schemas.
/// Each domain contains named types and `commands`/`events` modules. Commands
/// implement [`crate::protocol::Command`] with an associated response type.
///
/// Required fields are explicit; optional fields use `Option`. Types with only
/// optional fields implement `Default`, and identifiers are transparent newtypes.
/// Commands and named objects with schema-defined fields have `new(required_fields)`
/// and chainable `with_*` setters for optional fields. Protocol descriptions and
/// experimental/deprecated status come from the pinned schema.
///
/// Protocol enums are non-exhaustive and preserve unknown wire strings in
/// `Unrecognized(String)`. Their `as_str()` method returns the original wire value.
/// Other schema changes can still cause typed decoding to fail after a successful
/// CDP exchange. [`Error::Decode`] preserves the received payload; use
/// [`Connection::execute_raw`] or [`Session::execute_raw`] for untyped responses.
#[allow(clippy::large_enum_variant)]
pub mod cdp {
    include!(concat!(env!("OUT_DIR"), "/cdp.rs"));
}

pub use error::{Error, Result};

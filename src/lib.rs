//! Asynchronous Chrome DevTools Protocol client and browser automation.
//!
//! Use [`Browser`] and [`Page`] for browser automation, or [`Connection`] and
//! [`Session`] to send typed [`cdp`] commands and subscribe to events.
//! Applications supply a Tokio runtime and keep it running until awaited cleanup
//! finishes. Call `close` or `detach` to observe cleanup errors; Drop cleanup is
//! best-effort.
//!
//! # Example
//!
//! This application launches Chrome or Chromium in headless mode. Set
//! `CHROME_PATH` if the executable is not discoverable. Add Tokio with its `rt`
//! and `macros` features to use the single-thread runtime shown here.
//!
//! ```no_run
//! use rustcdp::{Browser, Page, Result, launcher::LaunchOptions};
//! use std::time::Duration;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<()> {
//!     let mut browser = Browser::launch(LaunchOptions {
//!         args: vec!["--headless=new".into()],
//!         ..Default::default()
//!     }).await?;
//!
//!     let result = print_title(browser.page()).await;
//!     let cleanup = browser.close().await;
//!     result?;
//!     cleanup
//! }
//!
//! async fn print_title(page: &Page) -> Result<()> {
//!     page.navigate_and_wait(
//!         "data:text/html,<title>Library demo</title>",
//!         Duration::from_secs(10),
//!     ).await?;
//!     println!("{}", page.evaluate("document.title").await?);
//!     Ok(())
//! }
//! ```
//!
//! Cleanup is awaited even when the page operation fails. If both operations
//! fail, this example returns the page operation's error.

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

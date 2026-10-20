use crate::{
    Connection, Page, Result,
    error::with_cleanup,
    launcher::{BrowserProcess, LaunchOptions},
};
use serde_json::json;

mod builder;
pub use builder::BrowserBuilder;

mod context;
pub use context::BrowserContext;

mod proxy;
mod resource;
pub(crate) use resource::ContextLease;

/// A browser connection, its initial page, and optionally an owned Chrome process.
#[must_use = "retain the browser while using it; await close() to finish cleanup"]
pub struct Browser {
    connection: Connection,
    page: Page,
    process: Option<BrowserProcess>,
    shutdown: Option<tokio::task::JoinHandle<Result<()>>>,
    shutdown_complete: bool,
}

impl Browser {
    /// Configure a new browser. Chrome remains visible unless headless is requested.
    pub fn builder() -> BrowserBuilder {
        BrowserBuilder::default()
    }

    /// Launch Chrome with the supplied options and attach to its initial page.
    /// Headless mode is opt-in through [`LaunchOptions::args`]. See
    /// [`Self::connect`] for initial-page selection and fallback ownership.
    pub async fn launch(options: LaunchOptions) -> Result<Self> {
        let (mut process, connection) = BrowserProcess::launch(options).await?;
        match Self::initial_page(&connection).await {
            Ok(page) => Ok(Self {
                connection,
                page,
                process: Some(process),
                shutdown: None,
                shutdown_complete: false,
            }),
            Err(error) => {
                connection.close().await;
                with_cleanup(Err(error), process.close().await)
            }
        }
    }

    /// Connect to an existing browser without owning its process.
    ///
    /// Uses an existing page when available, without taking ownership of its
    /// browser context. Otherwise creates a visible page in a fresh isolated
    /// context, separate from the default profile's cookies and storage.
    ///
    /// For an externally connected browser, dropping `Browser` leaves that fallback
    /// context alive while the connection stays open and dependent Page/Frame,
    /// Element, or out-of-process-frame Page handles remain. The last dependent
    /// schedules disposal; explicit [`Self::close`] invalidates all dependents.
    /// Standalone Sessions and listeners do not retain the context. Browser Drop
    /// stops proxy authentication even when dependents keep the context alive.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let connection = Connection::connect(endpoint).await?;
        match Self::initial_page(&connection).await {
            Ok(page) => Ok(Self {
                connection,
                page,
                process: None,
                shutdown: None,
                shutdown_complete: false,
            }),
            Err(error) => {
                connection.close().await;
                Err(error)
            }
        }
    }

    async fn initial_page(connection: &Connection) -> Result<Page> {
        let targets = connection
            .execute(&crate::cdp::target::commands::GetTargets::default())
            .await?;
        if let Some(target) = targets
            .target_infos
            .into_iter()
            .find(|target| target.type_ == "page")
        {
            // Chrome also reports an ID for the default context, but Storage
            // accepts explicit IDs only for contexts returned by this command.
            let contexts = connection
                .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                .await?;
            let context = target
                .browser_context_id
                .filter(|id| contexts.browser_context_ids.contains(id));
            let session = connection.attach(target.target_id).await?;
            Page::from_session(session, context, crate::page::TargetKind::Tab).await
        } else {
            Ok(BrowserContext::create(connection, json!({}))
                .await?
                .into_owned_page())
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn page(&self) -> &Page {
        &self.page
    }
    /// The owned process, or `None` for an external browser or once `close()`
    /// has first been polled. Shutdown takes process ownership immediately;
    /// `None` does not mean that process cleanup has already finished.
    pub fn process(&self) -> Option<&BrowserProcess> {
        self.process.as_ref()
    }

    /// Create an isolated context that explicitly bypasses the browser's proxy.
    /// See [`BrowserContext`] for acquisition and lifetime guarantees, or
    /// [`Self::new_context_with_proxy`] to configure a context-specific proxy.
    pub async fn new_context(&self) -> Result<BrowserContext> {
        BrowserContext::create(
            &self.connection,
            json!({"disposeOnDetach":true,"proxyServer":"","proxyBypassList":"*"}),
        )
        .await
    }

    /// Stop authentication, dispose any owned fallback context, and disconnect.
    /// An owned Chrome process is also reaped; an external process is not killed.
    /// On first poll, an owned task takes over the entire shutdown sequence.
    /// Connection and process cleanup are attempted even if earlier cleanup fails.
    /// Canceling the wait or dropping this Browser does not cancel that task;
    /// another `close()` awaits the same completion. Cleanup errors are returned
    /// once when completion is observed; later calls return `Ok(())`.
    /// Keep the owning Tokio runtime running until shutdown completes. Runtime
    /// shutdown can abandon async cleanup; process Drop still provides its
    /// independent best-effort kill/reap fallback.
    pub async fn close(&mut self) -> Result<()> {
        if self.shutdown_complete {
            return Ok(());
        }
        self.start_shutdown();
        // Await by reference so cancellation leaves the completion handle here.
        let result = self.shutdown.as_mut().expect("shutdown started").await;
        self.shutdown = None;
        self.shutdown_complete = true;
        result?
    }

    fn start_shutdown(&mut self) {
        if self.shutdown.is_some() || self.shutdown_complete {
            return;
        }
        let connection = self.connection.clone();
        // Retain the page's session/domain guards through authentication cleanup,
        // including when the Browser itself is dropped while shutdown is running.
        let page = self.page.clone();
        let mut process = self.process.take();
        let mut auth = self.page.auth.clone();
        self.shutdown = Some(self.connection.runtime().spawn(async move {
            let page_result = if let Some(lease) = &page.context_lease {
                lease
                    .close_with_auth(auth.take())
                    .await
                    .unwrap_or_else(|error| Err(error.into()))
            } else if let Some(auth) = &mut auth {
                auth.stop().await
            } else {
                Ok(())
            };
            connection.close().await;
            let process_result = if let Some(process) = &mut process {
                process.close().await
            } else {
                Ok(())
            };
            // Also force whole-Page capture rather than only context_lease.
            drop(page);
            with_cleanup(page_result, process_result)
        }));
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if self.process.is_some() {
            self.start_shutdown();
        } else if self.shutdown.is_none() && !self.shutdown_complete {
            // Preserve external connections and fallback leases retained by
            // dependent handles when explicit shutdown was never requested.
            // Request cleanup even when dependent pages retain the shared worker.
            // The owned task finishes without aborting Fetch cleanup.
            if let Some(auth) = self.page.auth.clone() {
                self.connection.runtime().spawn(async move {
                    let _ = auth.stop().await;
                });
            }
        }
        // Dropping a JoinHandle detaches the task; it does not abort shutdown.
    }
}

use crate::{
    Connection, Page, Result,
    error::with_cleanup,
    launcher::{BrowserProcess, LaunchOptions},
};
use serde_json::json;

mod context;
pub use context::BrowserContext;

mod proxy;
mod resource;
pub(crate) use resource::ContextLease;

/// A browser connection, its initial page, and optionally an owned Chrome process.
pub struct Browser {
    connection: Connection,
    page: Page,
    process: Option<BrowserProcess>,
    auth: Option<crate::proxy::ProxyAuth>,
}

impl Browser {
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
                auth: None,
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
                auth: None,
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
            Page::from_session(session, context).await
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
    /// Connection and process cleanup are attempted even if earlier cleanup fails.
    /// Keep the owning Tokio runtime running until this completes.
    pub async fn close(&mut self) -> Result<()> {
        let page_result = if let Some(lease) = &self.page.context_lease {
            lease
                .close_with_auth(self.auth.take())
                .await
                .unwrap_or_else(|error| Err(error.into()))
        } else if let Some(auth) = &mut self.auth {
            auth.stop().await
        } else {
            Ok(())
        };
        self.connection.close().await;
        let process_result = if let Some(process) = &mut self.process {
            process.close().await
        } else {
            Ok(())
        };
        with_cleanup(page_result, process_result)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.detach_proxy_auth();
    }
}

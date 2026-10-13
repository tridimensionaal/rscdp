use crate::{
    Connection, Error, Page, Result,
    cdp::browser::BrowserContextId,
    error::with_cleanup,
    launcher::{BrowserProcess, LaunchOptions},
};
use serde_json::{Value, json};

mod proxy;
mod resource;
pub(crate) use resource::ContextLease;

/// A browser connection, its initial page, and optionally an owned Chrome process.
pub struct Browser {
    connection: Connection,
    page: Page,
    process: Option<BrowserProcess>,
    auth: Option<crate::proxy_auth::ProxyAuth>,
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

/// An isolated browser context with its initial page and optional proxy worker.
///
/// Acquisition continues after caller cancellation or its command deadline. One
/// independent 30-second limit covers context creation and page setup, even with
/// command timeouts disabled. Ownership starts when the context ID is decoded.
/// Failed setup disposes a known context; failed disposal or an uncertain creation
/// with no known ID closes the connection. A protocol rejection of creation leaves
/// the connection usable. Contexts use `disposeOnDetach`; unclaimed results are
/// dropped and disposed.
///
/// Closing or dropping this guard disposes its context even while Page clones
/// remain, without closing the parent browser on successful cleanup. Only the
/// fallback page created by [`Browser::connect`] shares context ownership with
/// its dependents. Await [`Self::close`] to observe cleanup errors.
pub struct BrowserContext {
    lease: Option<ContextLease>,
    page: Page,
    auth: Option<crate::proxy_auth::ProxyAuth>,
}

impl BrowserContext {
    pub(crate) async fn create(connection: &Connection, params: Value) -> Result<Self> {
        let timeout = connection.command_timeout();
        let connection = connection.clone();
        // Finish acquiring ownership even if the caller cancels. An unclaimed
        // result is dropped by Tokio and disposes its context.
        let task = connection
            .runtime()
            .spawn(async move { Self::create_owned(&connection, params).await });
        let joined = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("creating browser context".into()))?
        } else {
            task.await
        };
        joined?
    }

    async fn create_owned(connection: &Connection, mut params: Value) -> Result<Self> {
        // Disconnection must resolve ownership even if the creation reply is lost.
        params["disposeOnDetach"] = true.into();
        let commands = connection.clone().with_command_timeout(None);
        let mut lease = None;
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let response = commands
                .execute_raw("Target.createBrowserContext", params)
                .await?;
            let id: BrowserContextId =
                serde_json::from_value(response["browserContextId"].clone())?;
            lease = Some(ContextLease::new(connection.clone(), id.clone()));
            // The context guard owns every resource acquired here. Keep setup in
            // this task so disposal cannot be followed by a nested task's recovery.
            let target = commands
                .execute(
                    &crate::cdp::target::commands::CreateTarget::new("about:blank")
                        .with_browser_context_id(id.clone()),
                )
                .await?
                .target_id;
            let attached = commands
                .execute(
                    &crate::cdp::target::commands::AttachToTarget::new(target.clone())
                        .with_flatten(true),
                )
                .await?;
            let session = commands.session(attached.session_id, target)?;
            Page::from_session_in_context(session, id).await
        })
        .await
        .unwrap_or_else(|_| Err(Error::Timeout("creating browser context".into())));
        match result {
            Ok(page) => Ok(Self {
                lease: Some(lease.expect("context ownership precedes page creation")),
                page: page.with_command_timeout(connection.command_timeout()),
                auth: None,
            }),
            Err(error) => {
                if let Some(lease) = lease {
                    let cleanup = lease.close().await;
                    if cleanup.is_err() {
                        connection.close().await;
                    }
                    with_cleanup(Err(error), cleanup)
                } else {
                    // A protocol rejection confirms that creation failed.
                    // Other failures can leave an unknown context on the server.
                    if !matches!(&error, Error::Protocol(_)) {
                        connection.close().await;
                    }
                    Err(error)
                }
            }
        }
    }

    /// The context ID, or `None` after disposal has been acknowledged.
    pub fn id(&self) -> Option<&BrowserContextId> {
        self.lease.as_ref().and_then(ContextLease::id)
    }
    pub fn page(&self) -> &Page {
        &self.page
    }

    // Only the fallback page takes context ownership. Ordinary context pages
    // deliberately do not keep their context alive after BrowserContext drops.
    fn into_owned_page(mut self) -> Page {
        let mut page = self.page.clone();
        let lease = self.lease.take().expect("context owns its lease");
        lease.register_auth(self.auth.take());
        page.context_lease = Some(std::sync::Arc::new(lease));
        page
    }

    /// Stop authentication and dispose this context in an owned runtime task.
    /// Cancellation does not stop cleanup. Disposal uses the configured command
    /// timeout, capped at 30 seconds for the disposal command. An uncertain
    /// timeout closes the connection; protocol errors remain retryable.
    ///
    /// Concurrent cleanup is serialized. Authentication cleanup precedes disposal,
    /// including after caller cancellation or Drop. [`Self::detach_proxy_auth`]
    /// explicitly skips authentication cleanup.
    /// Acknowledged disposal prevents duplicate commands and makes [`Self::id`]
    /// return `None`, even if the caller canceled its wait. Drop is best-effort;
    /// connection teardown after an uncertain timeout can take additional time.
    pub async fn close(&mut self) -> Result<()> {
        if let Some(lease) = &self.lease {
            lease.close_with_auth(self.auth.take()).await?
        } else {
            Ok(())
        }
    }
}

impl Drop for BrowserContext {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            lease.register_auth(self.auth.take());
        }
    }
}

use crate::cdp::browser::BrowserContextId;
use crate::{
    Connection, Page, Result,
    error::with_cleanup,
    launcher::{BrowserProcess, LaunchOptions},
    proxy::Proxy,
};
use serde_json::json;
use std::sync::Arc;

mod builder;
pub use builder::BrowserBuilder;

mod context;
pub use context::BrowserContext;

mod proxy;
mod resource;
pub(crate) use resource::{ContextLease, PageRegistration};

/// A browser connection, its initial page, and optionally an owned Chrome process.
///
/// # Lifetime
///
/// Before explicit shutdown starts, leaving the scope of a launched browser,
/// including an early return from `?`, synchronously terminates and reaps its
/// process and removes its temporary profile. Fixed profiles are preserved.
/// This can block the calling thread, and Drop cannot return cleanup errors.
/// See [`BrowserProcess::close`] for termination and profile-cleanup failure behavior.
///
/// Use [`Self::close`] to await shutdown without blocking on process cleanup and
/// to observe errors. Dropping or closing an externally connected browser never
/// terminates its process. See [`Self::connect`] for dependent-handle lifetimes.
///
/// Protocol cleanup on Drop is best effort and needs a running Tokio runtime.
/// Explicitly close contexts and sessions when their cleanup must finish before
/// continuing. See [`Self::close`] for cancellation and runtime-shutdown behavior.
#[must_use = "retain the browser while using it; await close() to finish cleanup"]
pub struct Browser {
    connection: Connection,
    page: Page,
    pages: Arc<resource::BrowserPages>,
    process: Option<BrowserProcess>,
    process_proxy: Option<Proxy>,
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
    /// Raw proxy flags configure routing but are not parsed as known credentials;
    /// set [`LaunchOptions::proxy`] to retain credentials for managed pages and
    /// contexts. Typed proxy setup continues in an owned task after cancellation;
    /// an unclaimed browser is shut down without navigating.
    pub async fn launch(mut options: LaunchOptions) -> Result<Self> {
        match options.proxy.take() {
            Some(proxy) => Self::launch_with_configured_proxy(options, proxy).await,
            None => Self::launch_process_and_attach(options).await,
        }
    }

    async fn launch_process_and_attach(options: LaunchOptions) -> Result<Self> {
        let (mut process, connection) = BrowserProcess::launch(options).await?;
        match Self::initial_page(&connection).await {
            Ok((page, context)) => Ok(Self {
                pages: Arc::new(resource::BrowserPages::new(&connection, &page, context)),
                connection,
                page,
                process: Some(process),
                process_proxy: None,
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
    /// Tries existing page targets in target-ID order, without taking ownership
    /// of their browser context. Only confirmed disappearance skips an attachment
    /// failure; operational and domain-setup errors propagate. Selection tries
    /// at most two snapshot batches, attempting each target ID once. Fallback
    /// requires a fresh snapshot with no page targets; continuing churn returns
    /// an attachment error. The fallback page uses a fresh isolated context,
    /// separate from the default profile's cookies and storage.
    ///
    /// For an externally connected browser, dropping `Browser` leaves that fallback
    /// context alive while the connection stays open and dependent Page/Frame,
    /// Element, or out-of-process-frame Page handles remain. The last dependent
    /// schedules disposal; explicit [`Self::close`] invalidates all dependents.
    /// Standalone Sessions and listeners do not retain the context. Browser Drop
    /// stops browser-owned proxy workers even when dependents keep the fallback
    /// alive. Independently owned BrowserContexts and their workers are untouched.
    /// External proxy credentials are unknown to this handle. New contexts inherit
    /// browser routing, but no authentication worker is inferred from that routing.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        Self::connect_with_options(endpoint, crate::connection::ConnectionOptions::default()).await
    }

    /// Connect with transport limits and an initial command timeout.
    /// Uses the same page selection and ownership policy as [`Self::connect`].
    /// Options are validated before network activity. The connect timeout covers
    /// discovery and the WebSocket handshake; initial-page setup has its own
    /// command and acquisition bounds. Initial discovery commands use the configured
    /// command timeout capped at 30 seconds. Attachment/domain setup and fallback
    /// acquisition finish their own bounded recovery in an owned task, even if the
    /// caller cancels. There is no aggregate deadline across candidate targets;
    /// the returned Page retains the configured command timeout.
    pub async fn connect_with_options(
        endpoint: &str,
        options: crate::connection::ConnectionOptions,
    ) -> Result<Self> {
        let connection = options.connect(endpoint).await?;
        match Self::initial_page(&connection).await {
            Ok((page, context)) => Ok(Self {
                pages: Arc::new(resource::BrowserPages::new(&connection, &page, context)),
                connection,
                page,
                process: None,
                process_proxy: None,
                shutdown: None,
                shutdown_complete: false,
            }),
            Err(error) => {
                connection.close().await;
                Err(error)
            }
        }
    }

    async fn initial_page(connection: &Connection) -> Result<(Page, Option<BrowserContextId>)> {
        let owned = connection.clone();
        // Do not abandon a nested attachment/domain owner on a caller deadline.
        // Cancellation drops this join handle, while selection and recovery finish.
        connection
            .runtime()
            .spawn(async move {
                let result = Self::select_initial_page(&owned).await;
                if result.is_err() {
                    owned.close().await;
                }
                result
            })
            .await?
    }

    async fn initial_targets(commands: &Connection) -> Result<Vec<crate::cdp::target::TargetInfo>> {
        let mut targets = commands
            .execute(
                &crate::cdp::target::commands::GetTargets::default().with_filter(
                    crate::cdp::target::TargetFilter(vec![
                        crate::cdp::target::FilterEntry::default(),
                    ]),
                ),
            )
            .await?
            .target_infos;
        targets.sort_by(|a, b| a.target_id.0.cmp(&b.target_id.0));
        Ok(targets)
    }

    async fn select_initial_page(
        connection: &Connection,
    ) -> Result<(Page, Option<BrowserContextId>)> {
        let bound = std::time::Duration::from_secs(30);
        let commands = connection.clone().with_command_timeout(Some(
            connection
                .command_timeout()
                .map_or(bound, |timeout| timeout.min(bound)),
        ));
        let setup = connection.clone().with_command_timeout(None);
        let mut targets = Self::initial_targets(&commands).await?;
        let mut attempted = Vec::new();
        let mut last_error = None;
        // One original batch and at most one refreshed batch. Disappearance
        // checks do not add candidates to the batch currently being attempted.
        for _ in 0..2 {
            if !targets.iter().any(|target| target.type_ == "page") {
                break;
            }
            // Chrome reports a default-profile ID too; only isolated IDs belong
            // in Page's context/Storage routing. Preserve this normalization.
            let contexts = commands
                .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                .await?
                .browser_context_ids;
            for target in targets.iter().filter(|target| target.type_ == "page") {
                if attempted.contains(&target.target_id) {
                    continue;
                }
                attempted.push(target.target_id.clone());
                let session = match setup.attach(target.target_id.clone()).await {
                    Ok(session) => session,
                    Err(error) => {
                        // Check all target kinds, not just page candidates: a
                        // changed kind is not proof that the target disappeared.
                        match Self::initial_targets(&commands).await {
                            Ok(fresh)
                                if !fresh.iter().any(|item| item.target_id == target.target_id) =>
                            {
                                last_error = Some(error);
                                continue;
                            }
                            Ok(_) => return Err(error),
                            Err(reconcile) => return with_cleanup(Err(error), Err(reconcile)),
                        }
                    }
                };
                let context = target
                    .browser_context_id
                    .clone()
                    .filter(|id| contexts.contains(id));
                let page = match Page::from_session(
                    session.clone(),
                    context.clone(),
                    crate::page::TargetKind::Tab,
                )
                .await
                {
                    Ok(page) => page.with_command_timeout(connection.command_timeout()),
                    Err(error) => {
                        // Domain failures are not stale-candidate retries. Finish
                        // releasing this owned attachment before returning.
                        let cleanup = session.detach().await;
                        if cleanup.is_err() && !session.is_closed() {
                            connection.close().await;
                        }
                        return with_cleanup(Err(error), cleanup);
                    }
                };
                return Ok((page, context));
            }
            // Fallback is authorized only by a fresh page-free snapshot, never
            // merely by exhausting a stale list or the finite retry budget.
            targets = Self::initial_targets(&commands).await?;
        }
        if targets.iter().any(|target| target.type_ == "page") {
            return Err(last_error.expect("candidate batches exhausted after attachment failures"));
        }
        // This is already an owned task; await context acquisition and recovery
        // directly, without a nested cancellation-prone caller timeout.
        let context = BrowserContext::create_owned(connection, json!({})).await?;
        let id = context.id().cloned();
        Ok((context.into_owned_page(), id))
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

    /// Create an about:blank tab in the default profile, independent of the
    /// initial page's context. Known process proxy credentials arm this tab's
    /// worker before return; external/raw proxy credentials are not inferred.
    /// Dropping the returned Page leaves the tab open; use Page::close explicitly.
    ///
    /// The command deadline bounds the caller's wait. Owned setup continues after
    /// cancellation and closes a known, unclaimed tab. Gate admission, creation,
    /// attachment and domain setup each have independent 30-second bounds; auth
    /// setup/recovery finishes separately. Failed known-tab cleanup is retained
    /// for reconciliation at browser shutdown, without closing unrelated tabs.
    ///
    /// A lost/undecodable creation reply can leave an unidentified default tab.
    /// This returns the error without guessing ownership or disconnecting a healthy
    /// connection. Disconnecting an external browser cannot guarantee its removal;
    /// an owned browser's eventual process shutdown removes all its tabs.
    pub async fn new_page(&self) -> Result<Page> {
        self.pages.new_page().await
    }

    /// Return page targets across contexts, sorted by target ID.
    /// Reuses managed sessions and authentication. Unknown isolated contexts are
    /// adopted without inferred credentials or context-disposal ownership.
    /// Ordinary context handles keep their original owner lifetime; fallback
    /// Pages retain the fallback lease. Closing scopes reject lookup.
    ///
    /// Like [`BrowserContext::pages`], enumeration is an owned operation: a caller
    /// timeout/cancellation does not cancel setup or its bounded recovery. A
    /// successful enumeration publishes pending-created tabs before its task
    /// result is received. Failed lists return no partial result. Snapshots do not
    /// guarantee that every returned tab remains open.
    /// Gate admission and scope discovery have 30-second bounds; setup helpers
    /// finish their own recovery. There is no total work deadline across targets.
    /// Acquisition failure detaches its session; known-credential auth fatal
    /// recovery can close its target/connection, and failed detach can disconnect.
    pub async fn pages(&self) -> Result<Vec<Page>> {
        self.pages.lookup(None).await
    }

    /// Find a page target by ID, reusing its managed handle when possible.
    /// Missing targets and non-page targets return `None`. Other errors propagate.
    /// Ownership, cancellation, and recovery follow [`Self::pages`].
    pub async fn page_by_id(&self, id: &crate::cdp::target::TargetId) -> Result<Option<Page>> {
        Ok(self
            .pages
            .lookup(Some(id.clone()))
            .await?
            .into_iter()
            .next())
    }

    /// Create an isolated context inheriting the browser's proxy routing and bypass rules.
    /// Credentials retained by [`Self::launch_with_proxy`] arm authentication on
    /// each managed page, including [`BrowserContext::new_page`]. Credentials from
    /// external browsers or raw launch flags are unknown and are not inferred.
    /// Popups and separate child sessions do not automatically receive workers.
    /// See [`BrowserContext`] for acquisition and lifetime guarantees. The caller's
    /// command deadline limits waiting; owned setup and authentication recovery
    /// continue after cancellation, disposing any unclaimed context.
    /// Use [`Self::new_context_with_proxy`] to override routing or
    /// [`Self::new_context_direct`] to explicitly bypass the process proxy.
    pub async fn new_context(&self) -> Result<BrowserContext> {
        self.create_context_with_auth(json!({"disposeOnDetach":true}), self.process_proxy.clone())
            .await
    }

    /// Create an isolated context that explicitly bypasses the browser's proxy.
    /// No proxy authentication worker is started. Acquisition and cancellation
    /// follow the same ownership policy as [`Self::new_context`].
    pub async fn new_context_direct(&self) -> Result<BrowserContext> {
        self.create_context_with_auth(
            json!({"disposeOnDetach":true,"proxyServer":"","proxyBypassList":"*"}),
            None,
        )
        .await
    }

    /// Wait for managed setup, stop all managed authentication workers, dispose
    /// any owned fallback context, and disconnect.
    /// An owned Chrome process is also reaped; an external process is not killed.
    /// On first poll, an owned task takes over the entire shutdown sequence.
    /// Connection and process cleanup are attempted even if earlier cleanup fails.
    /// Canceling the wait or dropping this Browser does not cancel that task;
    /// another `close()` awaits the same completion. Cleanup errors are returned
    /// once when completion is observed; later calls return `Ok(())`.
    /// Keep the owning Tokio runtime running until shutdown completes. Runtime
    /// shutdown can abandon protocol cleanup; process Drop synchronously finishes
    /// process termination, reaping, and temporary-profile cleanup instead.
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
        let scopes = self.pages.begin_shutdown(true);
        let pages = self.pages.clone();
        let connection = self.connection.clone();
        // Retain the initial Page and any fallback lease through shutdown.
        let page = self.page.clone();
        let mut process = self.process.take();
        self.shutdown = Some(self.connection.runtime().spawn(async move {
            // Setup that has not reached registration must finish and dispose
            // its rejected late result before connection teardown can proceed.
            let _acquisition = pages.acquisition_gate.lock().await;
            let mut cleanup = resource::BrowserPages::stop_scopes(&scopes).await;
            if let Some(lease) = pages.fallback() {
                cleanup = with_cleanup(cleanup, lease.close().await);
            }
            connection.close().await;
            pages.clear_owned();
            let process_result = if let Some(process) = &mut process {
                process.close().await
            } else {
                Ok(())
            };
            drop(page);
            with_cleanup(cleanup, process_result)
        }));
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            // Keep process ownership out of the async task: ordinary scope exit
            // must finish process cleanup even if Tokio is about to shut down.
            self.start_shutdown();
            drop(process);
        } else if self.shutdown.is_none() && !self.shutdown_complete {
            // External Drop must not stop independent contexts or their workers.
            // Their weak registrations never transfer ownership to this Browser.
            let scopes = self.pages.begin_shutdown(false);
            let pages = self.pages.clone();
            self.connection.runtime().spawn(async move {
                let _ = resource::BrowserPages::stop_scopes(&scopes).await;
                pages.clear_owned();
            });
        }
        // Dropping a JoinHandle detaches the task; it does not abort shutdown.
    }
}

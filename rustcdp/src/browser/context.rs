use super::ContextLease;
use crate::{
    Connection, Error, Page, Result,
    cdp::{
        browser::BrowserContextId,
        network::{Cookie, CookieParam},
        storage::commands::ClearCookies,
    },
    error::with_cleanup,
};
use serde_json::Value;

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
/// fallback page created by [`crate::Browser::connect`] shares context ownership with
/// its dependents. Await [`Self::close`] to observe cleanup errors.
#[must_use = "retain this context while using its pages; dropping it schedules disposal"]
pub struct BrowserContext {
    pub(super) lease: Option<ContextLease>,
    pub(super) page: Page,
}

impl BrowserContext {
    pub(super) async fn create_owned(connection: &Connection, mut params: Value) -> Result<Self> {
        // Disconnection must resolve ownership even if the creation reply is lost.
        params["disposeOnDetach"] = true.into();
        let commands = connection.clone().with_command_timeout(None);
        let mut lease = None;
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let response = commands
                .execute_raw("Target.createBrowserContext", params)
                .await?;
            let created: crate::cdp::target::commands::CreateBrowserContextResponse =
                crate::error::decode_ref("Target.createBrowserContext", &response)?;
            let id = created.browser_context_id;
            if id.0.is_empty() {
                return Err(Error::UnexpectedResponse {
                    method: "Target.createBrowserContext".into(),
                    payload: response,
                });
            }
            lease = Some(ContextLease::new(connection.clone(), id.clone()));
            // The context guard owns every resource acquired here. Keep setup in
            // this task so disposal cannot be followed by a nested task's recovery.
            let create = crate::cdp::target::commands::CreateTarget::new("about:blank")
                .with_browser_context_id(id.clone());
            let response = commands
                .execute_raw("Target.createTarget", serde_json::to_value(create)?)
                .await?;
            let created: crate::cdp::target::commands::CreateTargetResponse =
                crate::error::decode_ref("Target.createTarget", &response)?;
            let target = created.target_id;
            if target.0.is_empty() {
                return Err(Error::UnexpectedResponse {
                    method: "Target.createTarget".into(),
                    payload: response,
                });
            }
            let attach = crate::cdp::target::commands::AttachToTarget::new(target.clone())
                .with_flatten(true);
            let response = commands
                .execute_raw("Target.attachToTarget", serde_json::to_value(attach)?)
                .await?;
            let attached: crate::cdp::target::commands::AttachToTargetResponse =
                crate::error::decode_ref("Target.attachToTarget", &response)?;
            if attached.session_id.0.is_empty() {
                return Err(Error::UnexpectedResponse {
                    method: "Target.attachToTarget".into(),
                    payload: response,
                });
            }
            let session = commands.session(attached.session_id, target)?;
            Page::from_session_for_created_target(session, Some(id)).await
        })
        .await
        .unwrap_or_else(|_| Err(Error::Timeout("creating browser context".into())));
        match result {
            Ok(page) => {
                let lease = lease.expect("context ownership precedes page creation");
                let page = page.with_command_timeout(connection.command_timeout());
                lease.register_page(&page, None);
                Ok(Self {
                    lease: Some(lease),
                    page,
                })
            }
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

    /// Create an about:blank page with this context's proxy configuration.
    /// The context retains its managed pages until disposal; dropping a returned
    /// Page does not close its tab. Authentication covers each managed page's
    /// session, not popups or separate child sessions.
    ///
    /// The command deadline limits the caller's wait. Acquisition continues in
    /// an owned task after cancellation; an unclaimed page is closed. Gate admission
    /// and target/session setup each have a 30-second bound. Authentication setup
    /// and recovery finish separately before any failed acquisition is cleaned up.
    /// An uncertain creation without a target ID disposes this entire context;
    /// a protocol rejection leaves it usable. Failed known-target cleanup also
    /// disposes the context, closing the connection if disposal cannot be confirmed.
    pub async fn new_page(&self) -> Result<Page> {
        self.lease
            .as_ref()
            .ok_or_else(|| Error::InvalidArgument("browser context is closed".into()))?
            .new_page()
            .await
    }

    /// Return a snapshot of this context's page targets, sorted by target ID.
    /// Reuses managed sessions/auth workers and adopts previously unmanaged tabs.
    /// Closing/closed contexts reject the operation. These handles do not extend
    /// the context's lifetime; dropping the context still disposes all its tabs.
    /// A tab can disappear after the snapshot. Only confirmed disappearance after
    /// failed attachment is skipped; other errors fail the whole operation.
    ///
    /// The handle deadline bounds waiting, not the owned enumeration task. After
    /// cancellation, completed adoptions remain managed. A successful enumeration
    /// also publishes any selected pending-created tabs before returning its task
    /// result, so their original canceled acquisitions no longer close them.
    /// Gate admission and discovery commands have 30-second bounds; attachment,
    /// domain and authentication helpers finish their own bounded recovery.
    /// There is no single total work deadline across all targets.
    ///
    /// Acquisition cleanup detaches its owned session, never closes an adopted
    /// tab. Exception: with known credentials, the existing ProxyAuth fatal-recovery
    /// policy can close its target or connection. Uncertain attachment/domain or
    /// detach recovery can also disconnect, disposing disposeOnDetach contexts.
    /// Unknown or credentialless configuration starts no authentication worker.
    pub async fn pages(&self) -> Result<Vec<Page>> {
        self.lease
            .as_ref()
            .ok_or_else(|| Error::InvalidArgument("browser context is closed".into()))?
            .pages()
            .await
    }

    /// Read all cookies in this browser context using Storage, across all its pages.
    /// After acknowledged disposal, returns an error locally; it never selects the
    /// default context. External disposal may instead produce a remote error.
    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        self.id()
            .ok_or_else(|| Error::InvalidArgument("browser context is closed".into()))?;
        self.page.cookies().await
    }

    /// Add or update the supplied cookies across this browser context using Storage.
    /// This does not replace the whole cookie jar and is not limited to the initial page.
    /// After acknowledged disposal, returns an error locally; it never selects the
    /// default context. External disposal may instead produce a remote error.
    pub async fn set_cookies(&self, cookies: &[CookieParam]) -> Result<()> {
        self.id()
            .ok_or_else(|| Error::InvalidArgument("browser context is closed".into()))?;
        self.page.set_cookies(cookies).await
    }

    /// Clear all cookies in this browser context using Storage, across all its pages.
    /// Other contexts are unaffected. After acknowledged disposal, returns an error
    /// locally; it never selects the default context. External disposal may instead
    /// produce a remote error.
    pub async fn clear_cookies(&self) -> Result<()> {
        let id = self
            .id()
            .ok_or_else(|| Error::InvalidArgument("browser context is closed".into()))?
            .clone();
        self.page
            .session()
            .connection()
            .execute(&ClearCookies::default().with_browser_context_id(id))
            .await?;
        Ok(())
    }

    // Only the fallback page takes context ownership. Ordinary context pages
    // deliberately do not keep their context alive after BrowserContext drops.
    pub(super) fn into_owned_page(mut self) -> Page {
        let mut page = self.page.clone();
        let lease = self.lease.take().expect("context owns its lease");
        lease.register_auth(self.page.auth.clone());
        page.context_lease = Some(std::sync::Arc::new(lease));
        page
    }

    /// Stop authentication and dispose this context in an owned runtime task.
    /// Cancellation does not stop cleanup. Disposal uses the configured command
    /// timeout, capped at 30 seconds for the disposal command. An uncertain
    /// timeout closes the connection; protocol errors remain retryable.
    ///
    /// Concurrent cleanup is serialized. Authentication cleanup precedes disposal,
    /// including after caller cancellation or Drop. Use [`Self::stop_proxy_auth`]
    /// to stop authentication without disposing the context.
    /// Acknowledged disposal prevents duplicate commands and makes [`Self::id`]
    /// return `None`, even if the caller canceled its wait. Drop is best-effort;
    /// connection teardown after an uncertain timeout can take additional time.
    pub async fn close(&mut self) -> Result<()> {
        if let Some(lease) = &self.lease {
            lease.close_with_auth(self.page.auth.clone()).await?
        } else {
            Ok(())
        }
    }
}

impl Drop for BrowserContext {
    fn drop(&mut self) {
        if let Some(lease) = &self.lease {
            lease.register_auth(self.page.auth.clone());
        }
    }
}

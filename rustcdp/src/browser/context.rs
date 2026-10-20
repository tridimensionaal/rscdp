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
    lease: Option<ContextLease>,
    pub(super) page: Page,
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

    pub(super) async fn create_owned(connection: &Connection, mut params: Value) -> Result<Self> {
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

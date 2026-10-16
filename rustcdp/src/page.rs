use crate::{
    Error, Result, Session,
    cdp::{
        browser::BrowserContextId,
        network::{Cookie, CookieParam},
        page::{
            commands::{GetFrameTree, NavigateResponse},
            events::LifecycleEvent,
        },
        runtime::{ExecutionContextId, RemoteObject},
    },
};
use serde_json::{Value, json};
use std::time::Duration;

mod capture;
mod dom;
mod element;
mod execution_context;
mod frame;
mod input;
mod pdf;
mod selectors;

pub use capture::{ScreenshotBuilder, ScreenshotFormat};
pub use element::{ClickMethod, ClickOptions, Element, ElementPosition};
pub use frame::Frame;
pub use input::KeyModifiers;
pub use pdf::PdfBuilder;
pub use selectors::WaitOptions;

/// Page operations are shared by the initial browser page and isolated contexts.
/// A Page retains shared Page- and Runtime-domain guards for its lifetime and
/// subscribes to context events before enabling them. See [`Self::evaluate`] for
/// isolated-world behavior and [`crate::Browser::connect`] for fallback lifetime.
#[derive(Clone)]
pub struct Page {
    session: Session,
    _events: std::sync::Arc<[crate::session::DomainGuard; 2]>,
    worlds: execution_context::Worlds,
    context: Option<BrowserContextId>,
    pub(crate) context_lease: Option<std::sync::Arc<crate::browser::ContextLease>>,
}

impl Page {
    pub(crate) async fn from_session(
        session: Session,
        context: Option<BrowserContextId>,
    ) -> Result<Self> {
        let worlds = execution_context::Worlds::new(&session);
        let page_events = session.enable_page().await?;
        let runtime_events = session.enable_runtime().await?;
        Ok(Self {
            session,
            _events: std::sync::Arc::new([page_events, runtime_events]),
            worlds,
            context,
            context_lease: None,
        })
    }

    /// Only for setup under an enclosing ContextLease that disposes the context
    /// on failure or cancellation. The session must have command timeouts
    /// disabled; the enclosing acquisition owns the setup deadline and recovery.
    pub(crate) async fn from_session_in_context(
        session: Session,
        context: BrowserContextId,
    ) -> Result<Self> {
        let worlds = execution_context::Worlds::new(&session);
        let events = session.enable_page_runtime_in_context().await?;
        Ok(Self {
            session,
            _events: std::sync::Arc::new(events),
            worlds,
            context: Some(context),
            context_lease: None,
        })
    }

    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.session = self.session.with_command_timeout(timeout);
        self
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Start a navigation. Use `navigate_and_wait` when completion is required.
    pub async fn navigate(&self, url: &str) -> Result<NavigateResponse> {
        let response = self
            .session
            .execute_raw("Page.navigate", json!({"url":url,"transitionType":"typed"}))
            .await?;
        let response: NavigateResponse = serde_json::from_value(response)?;
        if let Some(error) = &response.error_text {
            return Err(Error::Navigation(error.clone()));
        }
        if response.is_download == Some(true) {
            return Err(Error::Navigation("navigation became a download".into()));
        }
        Ok(response)
    }

    /// Navigate and wait for the returned loader's load event within `timeout`.
    /// A same-document navigation with no new loader completes immediately.
    /// Unlike listening for an arbitrary load event, this matches the requested
    /// navigation. Downloads and reported navigation failures are errors.
    pub async fn navigate_and_wait(&self, url: &str, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            self.session
                .execute_raw("Page.setLifecycleEventsEnabled", json!({"enabled":true}))
                .await?;
            let mut events = self.session.listen::<LifecycleEvent>();
            let response = self.navigate(url).await?;
            let Some(loader) = response.loader_id else {
                return Ok(());
            };
            loop {
                let event = events.recv().await?;
                if event.name == "load" && event.loader_id == loader {
                    return Ok(());
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout(format!("navigating to {url}")))?
    }

    pub async fn go_back(&self) -> Result<bool> {
        let history = self
            .session
            .execute(&crate::cdp::page::commands::GetNavigationHistory::default())
            .await?;
        let Some(index) = history
            .current_index
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Ok(false);
        };
        let Some(entry) = history.entries.get(index) else {
            return Ok(false);
        };
        self.session
            .execute_raw("Page.navigateToHistoryEntry", json!({"entryId":entry.id}))
            .await?;
        Ok(true)
    }

    /// Return the main frame's current URL, including its fragment.
    pub async fn current_url(&self) -> Result<String> {
        let frame = self
            .session
            .execute(&GetFrameTree::default())
            .await?
            .frame_tree
            .frame;
        Ok(format!(
            "{}{}",
            frame.url,
            frame.url_fragment.unwrap_or_default()
        ))
    }

    /// Return a raw numeric context ID, which Chrome may reuse after navigation.
    /// Prefer `evaluate` for evaluation protected against context-ID reuse.
    pub async fn create_isolated_world(&self) -> Result<ExecutionContextId> {
        let frame = self
            .session
            .execute(&GetFrameTree::default())
            .await?
            .frame_tree
            .frame
            .id;
        self.create_isolated_world_for(&frame).await
    }

    async fn create_isolated_world_for(
        &self,
        frame: &crate::cdp::page::FrameId,
    ) -> Result<ExecutionContextId> {
        let response = self
            .session
            .execute_raw(
                "Page.createIsolatedWorld",
                json!({"frameId":frame,"worldName":format!("rustcdp-{}",self.session.id().0)}),
            )
            .await?;
        serde_json::from_value(response["executionContextId"].clone()).map_err(Into::into)
    }

    /// Evaluate in the current isolated world and return its JSON value.
    ///
    /// Page/Frame evaluation uses a unique execution-context identity, so reused
    /// numeric IDs cannot redirect an expression to another document. Expressions
    /// are never automatically retried; navigation races and JavaScript exceptions
    /// are errors. Worlds are reused within a document, but recovery after lost or
    /// inconsistent context events can create a fresh world without prior globals.
    ///
    /// A cache miss retains the Page in an owned setup task capped at 30 seconds.
    /// The handle timeout bounds the caller's combined world-lock and setup wait;
    /// canceling that wait does not abandon setup.
    ///
    /// Use [`Self::evaluate_remote`] for object IDs, `undefined`, BigInt, or
    /// non-finite numbers. This value-only method schedules best-effort release of
    /// any received object ID, including when no JSON value is available.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        self.json_value(self.evaluate_remote(expression, true).await?)
    }

    /// Evaluate with caller-owned remote objects on success.
    /// Release returned object IDs with `Runtime.releaseObject` when finished.
    /// On errors, received result and exception handles are released best-effort;
    /// IDs in JavaScript error details are diagnostic, not owned live references.
    /// Result and exception IDs are deduplicated before release. Cancellation
    /// before receiving a response cannot release unseen IDs. The world identity
    /// and setup behavior are the same as [`Self::evaluate`].
    pub async fn evaluate_remote(
        &self,
        expression: &str,
        return_by_value: bool,
    ) -> Result<RemoteObject> {
        let context = self.isolated_context().await?;
        self.evaluate_with_context(context.unique, expression, return_by_value)
            .await
    }

    async fn evaluate_with_context(
        &self,
        context: String,
        expression: &str,
        return_by_value: bool,
    ) -> Result<RemoteObject> {
        let response = self.session.execute_raw("Runtime.evaluate", json!({"expression":expression,"uniqueContextId":context,"silent":true,"returnByValue":return_by_value,"awaitPromise":true})).await?;
        if let Some(error) = response.get("exceptionDetails") {
            self.release_response_objects(&response);
            return Err(Error::JavaScript(error.clone()));
        }
        match serde_json::from_value(response["result"].clone()) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.release_response_objects(&response);
                Err(error.into())
            }
        }
    }

    fn json_value(&self, result: RemoteObject) -> Result<Value> {
        let _object = result.object_id.filter(|id| !id.0.is_empty()).map(|id| {
            Element::from_object_id(self.session.clone(), id, self.context_lease.clone())
        });
        result.value.ok_or_else(|| Error::InvalidArgument("JavaScript result has no JSON value; use evaluate_remote for undefined, BigInt and non-finite numbers".into()))
    }

    // Resolve uncertain resource ownership. DOM transactions retain their gate
    // until this completes; other acquisitions retain their Page owner.
    async fn recover_session(&self) {
        if self.session.is_closed() {
            return;
        }
        let session = self.session.clone().with_command_timeout(None);
        let _ = tokio::time::timeout(Duration::from_secs(1), session.detach()).await;
        // A detach event is conclusive even if the response was lost.
        if !session.is_closed() {
            session.connection().close().await;
        }
    }

    /// Read cookies from this page's browser context.
    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        let mut params = json!({});
        if let Some(id) = &self.context {
            params["browserContextId"] = serde_json::to_value(id)?;
        }
        let response = self
            .session
            .connection()
            .execute_raw("Storage.getCookies", params)
            .await?;
        serde_json::from_value(response["cookies"].clone()).map_err(Into::into)
    }

    /// Set cookies in this page's browser context.
    pub async fn set_cookies(&self, cookies: &[CookieParam]) -> Result<()> {
        let mut params = json!({"cookies":cookies});
        if let Some(id) = &self.context {
            params["browserContextId"] = serde_json::to_value(id)?;
        }
        self.session
            .connection()
            .execute_raw("Storage.setCookies", params)
            .await?;
        Ok(())
    }
}

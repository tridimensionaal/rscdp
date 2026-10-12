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
    error::with_cleanup,
};
use serde_json::{Value, json};
use std::time::Duration;

mod capture;
mod context;
mod frame;
mod input;
pub use frame::Frame;
mod interaction;
pub use input::KeyModifiers;
pub use interaction::{ClickMethod, Element, ElementPosition, WaitOptions};

/// Page operations are shared by the initial browser page and isolated contexts.
#[derive(Clone)]
pub struct Page {
    session: Session,
    _events: std::sync::Arc<[crate::session::DomainGuard; 2]>,
    worlds: context::Worlds,
    context: Option<BrowserContextId>,
    pub(crate) context_lease: Option<std::sync::Arc<crate::browser::ContextLease>>,
}

impl Page {
    pub(crate) async fn from_session(
        session: Session,
        context: Option<BrowserContextId>,
    ) -> Result<Self> {
        let worlds = context::Worlds::new(&session);
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
        let worlds = context::Worlds::new(&session);
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

    /// Evaluate in the current isolated world using its unique context identity.
    /// Navigation races fail without retrying the expression; JavaScript exceptions are errors.
    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        json_value(self.evaluate_remote(expression, true).await?)
    }

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
            return Err(Error::JavaScript(error.clone()));
        }
        serde_json::from_value(response["result"].clone()).map_err(Into::into)
    }

    pub async fn html(&self) -> Result<String> {
        let domain = self.session.enable_dom().await?;
        let result = self.document_html().await;
        with_cleanup(result, domain.close().await)
    }

    async fn document_html(&self) -> Result<String> {
        let response = self
            .session
            .execute_raw("DOM.getDocument", json!({}))
            .await?;
        let response = self
            .session
            .execute_raw(
                "DOM.getOuterHTML",
                json!({"nodeId":response["root"]["nodeId"]}),
            )
            .await?;
        serde_json::from_value(response["outerHTML"].clone()).map_err(Into::into)
    }

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

fn json_value(result: RemoteObject) -> Result<Value> {
    result.value.ok_or_else(|| Error::InvalidArgument("JavaScript result has no JSON value; use evaluate_remote for undefined, BigInt and non-finite numbers".into()))
}

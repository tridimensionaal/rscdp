use crate::{
    Result, Session,
    cdp::{
        browser::BrowserContextId,
        network::{Cookie, CookieParam},
        page::commands::GetFrameTree,
    },
};
use serde_json::json;
use std::time::Duration;

mod capture;
mod dom;
mod element;
mod evaluation;
mod execution_context;
mod frame;
mod input;
mod navigation;
mod pdf;
mod selectors;

pub use capture::{ScreenshotBuilder, ScreenshotFormat};
pub use dom::DomSearchOptions;
pub use element::{ClickMethod, ClickOptions, Element, ElementPosition};
pub use evaluation::{EvaluationBuilder, EvaluationWorld, RemoteValue};
pub use frame::Frame;
pub use input::KeyModifiers;
pub use navigation::{NavigationBuilder, WaitUntil};
pub use pdf::PdfBuilder;
pub use selectors::CountWaitOptions;

/// Page operations are shared by the initial browser page and isolated contexts.
/// A Page retains shared Page- and Runtime-domain guards for its lifetime and
/// subscribes to context events before enabling them. See [`Self::evaluate`] for
/// execution-world behavior and [`crate::Browser::connect`] for fallback lifetime.
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

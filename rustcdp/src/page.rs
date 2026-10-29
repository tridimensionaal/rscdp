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
mod function;
mod input;
mod lifecycle;
mod locator;
pub(crate) use lifecycle::TargetKind;
mod navigation;
mod pdf;
mod proxy;
mod response;
mod selectors;
pub use response::{PreparedResponse, Response, ResponseObserverBuilder};

pub use capture::{ScreenshotBuilder, ScreenshotFormat};
pub use dom::DomSearchOptions;
pub use element::{ClickMethod, ClickOptions, Element, ElementPosition};
pub use evaluation::{EvaluationBuilder, EvaluationWorld, RemoteValue};
pub use frame::Frame;
pub use function::{ElementCallBuilder, FunctionCallBuilder};
pub use input::KeyModifiers;
pub use locator::{ElementState, Locator};
pub use navigation::{
    NavigationBuilder, NavigationObserverBuilder, ObservedNavigation, PreparedNavigation, WaitUntil,
};
pub use pdf::PdfBuilder;
pub use selectors::CountWaitOptions;

/// Page operations are shared by the initial browser page and isolated contexts.
/// A Page retains shared Page- and Runtime-domain guards for its lifetime and
/// subscribes to context events before enabling them. See [`Self::evaluate`] for
/// execution-world behavior and [`crate::Browser::connect`] for fallback lifetime.
/// Dropping a Page does not close its tab. Use [`Self::close`] to close it explicitly;
/// the owning browser or context can still dispose it independently.
#[derive(Clone)]
pub struct Page {
    session: Session,
    target_kind: TargetKind,
    pub(crate) auth: Option<std::sync::Arc<crate::proxy::ProxyAuth>>,
    close_state: std::sync::Arc<tokio::sync::Mutex<lifecycle::CloseState>>,
    pub(crate) registration: std::sync::Arc<crate::browser::PageRegistration>,
    _events: std::sync::Arc<[crate::session::DomainGuard; 2]>,
    worlds: execution_context::Worlds,
    context: Option<BrowserContextId>,
    pub(crate) context_lease: Option<std::sync::Arc<crate::browser::ContextLease>>,
}

impl Page {
    pub(crate) async fn from_session(
        session: Session,
        context: Option<BrowserContextId>,
        target_kind: TargetKind,
    ) -> Result<Self> {
        let worlds = execution_context::Worlds::new(&session);
        let page_events = session.enable_page().await?;
        let runtime_events = session.enable_runtime().await?;
        Ok(Self {
            session,
            target_kind,
            close_state: Default::default(),
            registration: Default::default(),
            auth: None,
            _events: std::sync::Arc::new([page_events, runtime_events]),
            worlds,
            context,
            context_lease: None,
        })
    }

    /// Only for a newly created target whose enclosing owner closes the target
    /// (or disposes its context) on setup failure. Command timeouts must be disabled;
    /// the owner bounds setup and completes recovery before releasing its gate.
    pub(crate) async fn from_session_for_created_target(
        session: Session,
        context: Option<BrowserContextId>,
    ) -> Result<Self> {
        let worlds = execution_context::Worlds::new(&session);
        let events = session.enable_page_runtime_for_created_target().await?;
        Ok(Self {
            session,
            target_kind: TargetKind::Tab,
            close_state: Default::default(),
            registration: Default::default(),
            auth: None,
            _events: std::sync::Arc::new(events),
            worlds,
            context,
            context_lease: None,
        })
    }

    /// Set the command deadline on this handle, leaving existing clones unchanged.
    /// New clones inherit this setting. `None` disables caller command deadlines,
    /// but owned acquisition and cleanup keep their independent bounds. Shared
    /// sessions, domain guards, and authentication workers are not reconfigured.
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
        recover_session(&self.session).await;
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
        let decoded: crate::cdp::storage::commands::GetCookiesResponse =
            crate::error::decode_ref("Storage.getCookies", &response)?;
        Ok(decoded.cookies)
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

// Shared uncertain-acquisition recovery also serves element calls, which retain
// a Session and context lease without needing a Page or resolving a new world.
async fn recover_session(session: &Session) {
    if session.is_closed() {
        return;
    }
    let session = session.clone().with_command_timeout(None);
    let _ = tokio::time::timeout(Duration::from_secs(1), session.detach()).await;
    // A detach event is conclusive even if the response was lost.
    if !session.is_closed() {
        session.connection().close().await;
    }
}

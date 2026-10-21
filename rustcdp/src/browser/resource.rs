use crate::{
    Connection, Error, Page, Result,
    cdp::{browser::BrowserContextId, target::TargetId},
    error::with_cleanup,
    proxy::{Proxy, ProxyAuth},
};
use serde_json::json;
use std::{
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle};

struct Entry {
    page: Page,
    delivered: bool,
}

#[derive(Default)]
struct State {
    closing: bool,
    entries: Vec<Entry>,
    unclaimed: Vec<TargetId>,
    proxy: Option<Proxy>,
}

#[derive(PartialEq, Eq)]
enum Scope {
    Default,
    Context(BrowserContextId),
}

// Shared page management has no context-disposal authority or Drop cleanup.
pub(super) struct ManagedPages {
    connection: Connection,
    scope: Scope,
    gate: Mutex<()>,
    state: std::sync::Mutex<State>,
}

impl ManagedPages {
    fn new(connection: Connection, scope: Scope) -> Self {
        Self {
            connection,
            scope,
            gate: Mutex::new(()),
            state: Default::default(),
        }
    }

    async fn new_page(self: &Arc<Self>, cleanup: Option<Arc<Cleanup>>) -> Result<Page> {
        let pages = self.clone();
        let task = self
            .connection
            .runtime()
            .spawn(async move { acquire_page(pages, cleanup).await });
        let pending = if let Some(timeout) = self.connection.command_timeout() {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("creating page".into()))??
        } else {
            task.await?
        }?;
        pending.publish()
    }

    fn mark_closing(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closing = true;
    }

    fn set_proxy(&self, proxy: Option<Proxy>) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).proxy = proxy;
    }

    async fn stop_auth(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        let pages: Vec<_> = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .map(|entry| entry.page.clone())
            .collect();
        let mut result = Ok(());
        for page in &pages {
            result = with_cleanup(result, page.stop_proxy_auth().await);
        }
        // begin_shutdown marks closing under the publication mutex before this
        // gate is acquired. Successful publication removes its ID under that same
        // mutex, so this snapshot cannot select a delivered tab. Default tabs
        // survive disconnect: reconcile unclaimed IDs before draining the cache.
        if self.scope == Scope::Default {
            let ids = self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unclaimed
                .clone();
            for id in ids {
                if self.is_pending(&id) {
                    result = with_cleanup(result, cleanup_target(self, None, &id).await);
                }
            }
        }
        result
    }

    fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.clear();
        state.proxy = None;
    }

    fn register(&self, page: &Page, delivered: bool) {
        // Never introduce a cache -> Page -> ContextLease -> cache cycle.
        let mut page = page.clone();
        page.context_lease = None;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|entry| entry.page.id() != page.id());
        state.entries.push(Entry { page, delivered });
    }

    // The pending guard calls this synchronously on receipt. Future enumeration
    // must also hold the gate so publication cannot race unclaimed-tab cleanup.
    fn publish(&self, id: &TargetId) -> Result<Page> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("page scope is closing".into()));
        }
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| entry.page.id() == id)
            .ok_or_else(|| Error::InvalidArgument("pending page is no longer managed".into()))?;
        entry.delivered = true;
        let page = entry.page.clone();
        state.unclaimed.retain(|pending| pending != id);
        Ok(page)
    }

    fn is_pending(&self, id: &TargetId) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.unclaimed.contains(id)
            && !state
                .entries
                .iter()
                .any(|entry| entry.page.id() == id && entry.delivered)
    }
}

struct KnownContext {
    id: BrowserContextId,
    pages: Weak<ManagedPages>,
    // Only fallback Pages regain a lease when returned by future lookup APIs.
    fallback: Option<Weak<ContextLease>>,
}

#[derive(Default)]
struct BrowserState {
    closing: bool,
    // Default and observed foreign scopes are owned; explicit contexts are not.
    owned: Vec<Arc<ManagedPages>>,
    known: Vec<KnownContext>,
}

pub(super) struct BrowserPages {
    pub(super) acquisition_gate: Mutex<()>,
    state: std::sync::Mutex<BrowserState>,
    default: Arc<ManagedPages>,
    initial: Weak<ManagedPages>,
}

impl BrowserPages {
    pub(super) fn new(
        connection: &Connection,
        page: &Page,
        context: Option<BrowserContextId>,
    ) -> Self {
        let default = Arc::new(ManagedPages::new(connection.clone(), Scope::Default));
        let mut state = BrowserState::default();
        state.owned.push(default.clone());
        let initial = if let Some(lease) = &page.context_lease {
            let pages = lease.cleanup.pages.clone();
            state.known.push(KnownContext {
                id: lease.id.clone(),
                pages: Arc::downgrade(&pages),
                fallback: Some(Arc::downgrade(lease)),
            });
            pages
        } else if let Some(id) = context {
            let pages = Arc::new(ManagedPages::new(connection.clone(), Scope::Context(id)));
            state.owned.push(pages.clone());
            pages
        } else {
            default.clone()
        };
        initial.register(page, true);
        Self {
            acquisition_gate: Mutex::new(()),
            state: std::sync::Mutex::new(state),
            default,
            initial: Arc::downgrade(&initial),
        }
    }

    pub(super) async fn new_page(&self) -> Result<Page> {
        self.ensure_open()?;
        self.default.new_page(None).await
    }

    pub(super) fn ensure_open(&self) -> Result<()> {
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).closing {
            return Err(Error::InvalidArgument("browser is closing".into()));
        }
        Ok(())
    }

    pub(super) fn register_context(&self, lease: &ContextLease) -> Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("browser is closing".into()));
        }
        state.known.retain(|entry| entry.id != lease.id);
        state.known.push(KnownContext {
            id: lease.id.clone(),
            pages: Arc::downgrade(&lease.cleanup.pages),
            fallback: None,
        });
        Ok(())
    }

    pub(super) fn initial_uses_process_proxy(&self) -> bool {
        self.initial.upgrade().is_some_and(|pages| {
            if pages.scope == Scope::Default {
                return true;
            }
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .known
                .iter()
                .any(|entry| {
                    entry.fallback.is_some() && entry.pages.ptr_eq(&Arc::downgrade(&pages))
                })
        })
    }

    pub(super) fn set_process_proxy(&self, page: &Page, proxy: Proxy) {
        self.default.set_proxy(Some(proxy.clone()));
        if self.initial_uses_process_proxy()
            && let Some(initial) = self.initial.upgrade()
        {
            initial.register(page, true);
            initial.set_proxy(Some(proxy));
        }
    }

    // Browser-state -> scope-state is the only nested state-lock order. No await.
    pub(super) fn begin_shutdown(&self, include_independent: bool) -> Vec<Arc<ManagedPages>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut scopes = state.owned.clone();
        for entry in &state.known {
            if (include_independent || entry.fallback.is_some())
                && let Some(pages) = entry.pages.upgrade()
            {
                scopes.push(pages);
            }
        }
        for pages in &scopes {
            pages.mark_closing();
        }
        // Registration is excluded by this mutex. Once browser closing is
        // published, every affected scope already rejects page publication.
        state.closing = true;
        scopes
    }

    // Called after the acquisition barrier for explicit shutdown; before it for
    // external Drop, where independent context setup is left to finish/reject.
    pub(super) async fn stop_scopes(scopes: &[Arc<ManagedPages>]) -> Result<()> {
        let mut result = Ok(());
        for pages in scopes {
            result = with_cleanup(result, pages.stop_auth().await);
        }
        result
    }

    pub(super) fn clear_owned(&self) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for pages in &state.owned {
            pages.clear();
        }
        // Fallback cache belongs to its lease: retain it until disposal. Do not
        // drain independent contexts, even on external Browser Drop.
    }

    pub(super) fn fallback(&self) -> Option<Arc<ContextLease>> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .known
            .iter()
            .find_map(|entry| entry.fallback.as_ref().and_then(Weak::upgrade))
    }
}

// Context-specific cleanup stays separate from the reusable page collection.
// Only ContextLease and the tasks it starts carry this disposal authority.
struct Cleanup {
    pages: Arc<ManagedPages>,
    closed: AtomicBool,
    pending_auth: std::sync::Mutex<Option<Arc<ProxyAuth>>>,
}

pub(crate) struct ContextLease {
    id: BrowserContextId,
    cleanup: Arc<Cleanup>,
}

impl ContextLease {
    pub(super) fn new(connection: Connection, id: BrowserContextId) -> Self {
        Self {
            cleanup: Arc::new(Cleanup {
                pages: Arc::new(ManagedPages::new(connection, Scope::Context(id.clone()))),
                closed: AtomicBool::new(false),
                pending_auth: Default::default(),
            }),
            id,
        }
    }

    pub(super) fn id(&self) -> Option<&BrowserContextId> {
        (!self.cleanup.closed.load(Ordering::Acquire)).then_some(&self.id)
    }

    pub(super) fn register_page(&self, page: &Page, proxy: Option<Proxy>) {
        self.cleanup.pages.register(page, true);
        self.cleanup
            .pages
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .proxy = proxy;
    }

    pub(super) async fn new_page(&self) -> Result<Page> {
        self.cleanup
            .pages
            .new_page(Some(self.cleanup.clone()))
            .await
    }

    pub(super) async fn close(&self) -> Result<()> {
        self.close_with_auth(None).await?
    }

    pub(super) fn close_with_auth(&self, auth: Option<Arc<ProxyAuth>>) -> JoinHandle<Result<()>> {
        {
            let mut state = self
                .cleanup
                .pages
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.closing = true;
            // Register while holding publication state, as disposal snapshots
            // both under this same lock order before it starts stopping workers.
            self.register_auth(auth);
        }
        let connection = self.cleanup.pages.connection.clone();
        let cleanup = self.cleanup.clone();
        let id = self.id.clone();
        self.cleanup.pages.connection.runtime().spawn(async move {
            let _gate = cleanup.pages.gate.lock().await;
            dispose_context(&connection, &cleanup, &id).await
        })
    }

    pub(super) fn register_auth(&self, auth: Option<Arc<ProxyAuth>>) {
        if let Some(auth) = auth {
            *self
                .cleanup
                .pending_auth
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(auth);
        }
    }
}

impl Drop for ContextLease {
    fn drop(&mut self) {
        if !self.cleanup.closed.load(Ordering::Acquire) {
            self.close_with_auth(None);
        }
    }
}

struct PendingPage {
    pages: Arc<ManagedPages>,
    cleanup: Option<Arc<Cleanup>>,
    page: Option<Page>,
}

impl PendingPage {
    fn publish(mut self) -> Result<Page> {
        let page = self.pages.publish(
            self.page
                .as_ref()
                .expect("pending page has not been delivered")
                .id(),
        )?;
        self.page = None;
        Ok(page)
    }
}

impl Drop for PendingPage {
    fn drop(&mut self) {
        let Some(page) = self.page.take() else {
            return;
        };
        let pages = self.pages.clone();
        let cleanup = self.cleanup.clone();
        self.pages.connection.runtime().spawn(async move {
            let _gate = pages.gate.lock().await;
            if !cleanup
                .as_ref()
                .is_some_and(|cleanup| cleanup.closed.load(Ordering::Acquire))
                && pages.is_pending(page.id())
            {
                let _ = cleanup_target(&pages, cleanup.as_deref(), page.id()).await;
            }
            drop(page);
        });
    }
}

async fn acquire_page(
    pages: Arc<ManagedPages>,
    cleanup: Option<Arc<Cleanup>>,
) -> Result<PendingPage> {
    let _gate = tokio::time::timeout(Duration::from_secs(30), pages.gate.lock())
        .await
        .map_err(|_| Error::Timeout("waiting to create page".into()))?;
    let proxy = {
        let state = pages.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("page scope is closing".into()));
        }
        state.proxy.clone()
    };
    let connection = &pages.connection;
    let commands = connection.clone().with_command_timeout(None);
    let context = match &pages.scope {
        Scope::Default => None,
        Scope::Context(id) => Some(id.clone()),
    };
    let mut target = None;
    let result = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut create = crate::cdp::target::commands::CreateTarget::new("about:blank");
        if let Some(id) = &context {
            create = create.with_browser_context_id(id.clone());
        }
        let response = tokio::time::timeout_at(
            deadline,
            commands.execute_raw("Target.createTarget", serde_json::to_value(&create)?),
        )
        .await
        .map_err(|_| Error::Timeout("creating page target".into()))??;
        let decoded: crate::cdp::target::commands::CreateTargetResponse =
            crate::error::decode_ref("Target.createTarget", &response)?;
        let id = decoded.target_id;
        if id.0.is_empty() {
            // No usable identity: do not attach, record, or close an empty ID.
            // The caller below applies context disposal or default uncertainty.
            return Err(Error::UnexpectedResponse {
                method: "Target.createTarget".into(),
                payload: response,
            });
        }
        target = Some(id.clone());
        pages
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unclaimed
            .push(id.clone());
        let (session, setup_deadline) = if context.is_some() {
            // Preserve the context path's single create/attach/domain budget.
            let attached = tokio::time::timeout_at(
                deadline,
                commands.execute(
                    &crate::cdp::target::commands::AttachToTarget::new(id.clone())
                        .with_flatten(true),
                ),
            )
            .await
            .map_err(|_| Error::Timeout("attaching context page".into()))??;
            (commands.session(attached.session_id, id)?, deadline)
        } else {
            // Default tabs survive their owner. Use an owned attachment so the
            // final session owner detaches without closing the delivered tab.
            // Await this helper's own bounded recovery; never cancel its task
            // with a surrounding creation timeout and race target cleanup.
            let session = commands.attach(id).await?;
            (
                session,
                tokio::time::Instant::now() + Duration::from_secs(30),
            )
        };
        tokio::time::timeout_at(
            setup_deadline,
            Page::from_session_for_created_target(session, context.clone()),
        )
        .await
        .map_err(|_| Error::Timeout("setting up created page".into()))?
    }
    .await;
    let mut page = match result {
        Ok(page) => page.with_command_timeout(connection.command_timeout()),
        Err(error) => {
            let recovery = match target {
                Some(id) => cleanup_target(&pages, cleanup.as_deref(), &id).await,
                None if matches!(&error, Error::Protocol(_)) => Ok(()),
                None => match (&cleanup, &context) {
                    (Some(cleanup), Some(context)) => {
                        dispose_after_failure(connection, cleanup, context).await
                    }
                    // Unknown default target identity cannot be recovered safely.
                    // Leave a healthy connection usable; never guess another tab.
                    _ => Ok(()),
                },
            };
            return with_cleanup(Err(error), recovery);
        }
    };
    // Auth owns bounded setup/recovery separately from creation and domain setup.
    if let Some(proxy) = proxy {
        match super::proxy::start_auth(&page, proxy).await {
            Ok(auth) => page.auth = auth,
            Err(error) => {
                let recovery = cleanup_target(&pages, cleanup.as_deref(), page.id()).await;
                return with_cleanup(Err(error), recovery);
            }
        }
    }
    pages.register(&page, false);
    // No gate in the returned result: shutdown never depends on caller polling.
    Ok(PendingPage {
        pages: pages.clone(),
        cleanup,
        page: Some(page),
    })
}

// All helpers below run under the acquisition/disposal gate; none reacquires it.
async fn cleanup_target(
    pages: &ManagedPages,
    cleanup: Option<&Cleanup>,
    id: &TargetId,
) -> Result<()> {
    let connection = &pages.connection;
    let page = pages
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .iter()
        .find(|entry| entry.page.id() == id)
        .map(|entry| entry.page.clone());
    let auth_result = match &page {
        Some(page) => page.stop_proxy_auth().await,
        None => Ok(()),
    };
    let commands = connection.clone().with_command_timeout(None);
    let closed = tokio::time::timeout(Duration::from_secs(30), Page::close_target(&commands, id))
        .await
        .unwrap_or_else(|_| Err(Error::Timeout(format!("closing unclaimed page {id}"))));
    let result = if closed.is_ok() {
        let mut state = pages.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|entry| entry.page.id() != id);
        state.unclaimed.retain(|pending| pending != id);
        closed
    } else if let (Some(cleanup), Scope::Context(context)) = (cleanup, &pages.scope) {
        with_cleanup(
            closed,
            dispose_after_failure(connection, cleanup, context).await,
        )
    } else {
        // Keep the known ID for reconciliation during browser-owned shutdown.
        // Default cleanup failure never authorizes disposing another context.
        closed
    };
    with_cleanup(auth_result, result)
}

async fn dispose_after_failure(
    connection: &Connection,
    cleanup: &Cleanup,
    id: &BrowserContextId,
) -> Result<()> {
    let result = dispose_context(connection, cleanup, id).await;
    if !cleanup.closed.load(Ordering::Acquire) {
        connection.close().await;
    }
    result
}

async fn dispose_context(
    connection: &Connection,
    cleanup: &Cleanup,
    id: &BrowserContextId,
) -> Result<()> {
    let (pages, auth) = {
        let mut state = cleanup
            .pages
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        let pages: Vec<_> = state
            .entries
            .iter()
            .map(|entry| entry.page.clone())
            .collect();
        let auth = cleanup
            .pending_auth
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        (pages, auth)
    };
    let mut auth_result = Ok(());
    for page in &pages {
        auth_result = with_cleanup(auth_result, page.stop_proxy_auth().await);
    }
    if let Some(auth) = auth {
        auth_result = with_cleanup(auth_result, auth.stop().await);
    }
    let result = if cleanup.closed.load(Ordering::Acquire) {
        Ok(())
    } else {
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            connection.execute_raw(
                "Target.disposeBrowserContext",
                json!({"browserContextId":id}),
            ),
        )
        .await
        .unwrap_or_else(|_| Err(Error::Timeout("Target.disposeBrowserContext".into())));
        if matches!(&result, Err(Error::Timeout(_))) && !connection.is_closed() {
            connection.close().await;
        }
        if result.is_ok() {
            cleanup.closed.store(true, Ordering::Release);
            let mut state = cleanup
                .pages
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            state.entries.clear();
            state.unclaimed.clear();
            state.proxy = None;
        }
        result.map(|_| ())
    };
    // Keep the cloned Pages (and sessions/workers) alive through disposal.
    drop(pages);
    with_cleanup(auth_result, result)
}

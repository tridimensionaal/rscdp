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
            return Err(Error::InvalidArgument("browser context is closing".into()));
        }
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| entry.page.id() == id)
            .ok_or_else(|| Error::InvalidArgument("pending page is no longer managed".into()))?;
        entry.delivered = true;
        Ok(entry.page.clone())
    }

    fn is_pending(&self, id: &TargetId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .any(|entry| entry.page.id() == id && !entry.delivered)
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
        let connection = self.cleanup.pages.connection.clone();
        let cleanup = self.cleanup.clone();
        let id = self.id.clone();
        let task = connection
            .runtime()
            .spawn(async move { acquire_page(connection, cleanup, id).await });
        let pending = if let Some(timeout) = self.cleanup.pages.connection.command_timeout() {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("creating context page".into()))??
        } else {
            task.await?
        }?;
        pending.publish()
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
    cleanup: Arc<Cleanup>,
    context: BrowserContextId,
    page: Option<Page>,
}

impl PendingPage {
    fn publish(mut self) -> Result<Page> {
        let page = self.cleanup.pages.publish(
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
        let connection = self.cleanup.pages.connection.clone();
        let cleanup = self.cleanup.clone();
        let context = self.context.clone();
        self.cleanup.pages.connection.runtime().spawn(async move {
            let _gate = cleanup.pages.gate.lock().await;
            if !cleanup.closed.load(Ordering::Acquire) && cleanup.pages.is_pending(page.id()) {
                let _ = cleanup_target(&connection, &cleanup, &context, page.id()).await;
            }
            drop(page);
        });
    }
}

async fn acquire_page(
    connection: Connection,
    cleanup: Arc<Cleanup>,
    context: BrowserContextId,
) -> Result<PendingPage> {
    let _gate = tokio::time::timeout(Duration::from_secs(30), cleanup.pages.gate.lock())
        .await
        .map_err(|_| Error::Timeout("waiting to create context page".into()))?;
    let proxy = {
        let state = cleanup
            .pages
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("browser context is closing".into()));
        }
        state.proxy.clone()
    };
    let commands = connection.clone().with_command_timeout(None);
    let mut target = None;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let id = commands
            .execute(
                &crate::cdp::target::commands::CreateTarget::new("about:blank")
                    .with_browser_context_id(context.clone()),
            )
            .await?
            .target_id;
        target = Some(id.clone());
        let attached = commands
            .execute(
                &crate::cdp::target::commands::AttachToTarget::new(id.clone()).with_flatten(true),
            )
            .await?;
        let session = commands.session(attached.session_id, id)?;
        Page::from_session_in_context(session, context.clone()).await
    })
    .await
    .unwrap_or_else(|_| Err(Error::Timeout("creating context page".into())));
    let mut page = match result {
        Ok(page) => page.with_command_timeout(connection.command_timeout()),
        Err(error) => {
            let recovery = match target {
                Some(id) => cleanup_target(&connection, &cleanup, &context, &id).await,
                None if matches!(&error, Error::Protocol(_)) => Ok(()),
                None => dispose_after_failure(&connection, &cleanup, &context).await,
            };
            return with_cleanup(Err(error), recovery);
        }
    };
    // Auth start owns bounded setup/recovery. Never cancel it with the page's
    // creation timeout, or disposal could race its recovery commands.
    if let Some(proxy) = proxy {
        match super::proxy::start_auth(&page, proxy).await {
            Ok(auth) => page.auth = auth,
            Err(error) => {
                let recovery = cleanup_target(&connection, &cleanup, &context, page.id()).await;
                return with_cleanup(Err(error), recovery);
            }
        }
    }
    cleanup.pages.register(&page, false);
    // This result owns no gate. Context close can run even if nobody ever polls
    // the completed acquisition handle to retrieve or drop its result.
    Ok(PendingPage {
        cleanup: cleanup.clone(),
        context,
        page: Some(page),
    })
}

// All helpers below run under the acquisition/disposal gate; none reacquires it.
async fn cleanup_target(
    connection: &Connection,
    cleanup: &Cleanup,
    context: &BrowserContextId,
    id: &TargetId,
) -> Result<()> {
    let page = cleanup
        .pages
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
        cleanup
            .pages
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .retain(|entry| entry.page.id() != id);
        closed
    } else {
        with_cleanup(
            closed,
            dispose_after_failure(connection, cleanup, context).await,
        )
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
            state.proxy = None;
        }
        result.map(|_| ())
    };
    // Keep the cloned Pages (and sessions/workers) alive through disposal.
    drop(pages);
    with_cleanup(auth_result, result)
}

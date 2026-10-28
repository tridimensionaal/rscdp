use crate::{
    Connection, Error, Page, Result,
    cdp::{browser::BrowserContextId, target::TargetId},
    error::with_cleanup,
    proxy::{Proxy, ProxyAuth},
};
use serde_json::json;
use std::{
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle};

struct Entry {
    page: Page,
    delivered: bool,
    closed: Option<tokio::task::JoinHandle<()>>,
}

impl Entry {
    fn new(page: Page, delivered: bool) -> Self {
        let mut entry = Self {
            page,
            delivered: false,
            closed: None,
        };
        if delivered {
            entry.deliver();
        }
        entry
    }

    fn deliver(&mut self) {
        self.delivered = true;
        if self.closed.is_some() {
            return;
        }
        // No Page or context lease in this observer, and no strong scope until
        // the session closes. Evicting the entry cancels its observer as well.
        let owner = self
            .page
            .registration
            .owner
            .get()
            .expect("registered page")
            .clone();
        let session = self.page.session().clone();
        let auth = self.page.auth.clone();
        self.closed = Some(session.connection().runtime().spawn(async move {
            session.closed().await;
            let Some(owner) = owner.upgrade() else { return };
            let _gate = owner.gate.lock().await;
            if let Some(auth) = auth {
                let _ = auth.stop().await;
            }
            // Session detachment does not prove target closure: preserve any
            // undelivered target obligation and never issue a close command.
            let removed = {
                let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .entries
                    .iter()
                    .position(|entry| entry.delivered && entry.page.session().id() == session.id())
                    .map(|index| state.entries.swap_remove(index))
            };
            drop(removed);
        }));
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        if let Some(task) = &self.closed {
            task.abort();
        }
    }
}

/// Shared by Page clones, without keeping the managed scope alive.
#[derive(Default)]
pub(crate) struct PageRegistration {
    owner: OnceLock<Weak<ManagedPages>>,
}

impl PageRegistration {
    fn bind(&self, owner: &Arc<ManagedPages>) {
        let weak = Arc::downgrade(owner);
        let registered = self.owner.get_or_init(|| weak.clone());
        assert!(
            registered.ptr_eq(&weak),
            "page registered in different scopes"
        );
    }

    pub(crate) async fn close(&self, connection: &Connection, id: &TargetId) -> Result<()> {
        let Some(owner) = self.owner.get().and_then(Weak::upgrade) else {
            return Page::close_target(connection, id).await;
        };
        // Serialize confirmation and eviction against replacement attachment.
        // The caller owns and bounds this whole operation, including admission.
        let _gate = owner.gate.lock().await;
        Page::close_target(connection, id).await?;
        let removed = {
            let mut state = owner.state.lock().unwrap_or_else(|e| e.into_inner());
            state.unclaimed.retain(|pending| pending != id);
            state
                .entries
                .iter()
                .position(|entry| entry.page.id() == id)
                .map(|index| state.entries.swap_remove(index))
        };
        drop(removed);
        Ok(())
    }
}

#[derive(Default)]
struct State {
    closing: bool,
    entries: Vec<Entry>,
    unclaimed: Vec<TargetId>,
    proxy: Option<Proxy>,
}

impl State {
    fn mark_delivered(&mut self, id: &TargetId) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.page.id() == id) {
            entry.deliver();
        }
        self.unclaimed.retain(|pending| pending != id);
    }
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

    async fn pages(self: &Arc<Self>) -> Result<Vec<Page>> {
        self.ensure_open()?;
        let pages = self.clone();
        let task = self
            .connection
            .runtime()
            .spawn(async move { pages.list_owned().await });
        let result = if let Some(timeout) = self.connection.command_timeout() {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("listing context pages".into()))??
        } else {
            task.await?
        }?;
        // A completed task can sit unpolled while the context closes. Never
        // deliver that result after observing scope closure; no await follows.
        self.ensure_open()?;
        Ok(result)
    }

    fn ensure_open(&self) -> Result<()> {
        if self.state.lock().unwrap_or_else(|e| e.into_inner()).closing {
            return Err(Error::InvalidArgument("page scope is closing".into()));
        }
        Ok(())
    }

    async fn target_snapshot(&self) -> Result<Vec<crate::cdp::target::TargetInfo>> {
        let commands = self
            .connection
            .clone()
            .with_command_timeout(Some(Duration::from_secs(30)));
        Ok(commands
            .execute(
                &crate::cdp::target::commands::GetTargets::new().with_filter(
                    crate::cdp::target::TargetFilter(vec![
                        crate::cdp::target::FilterEntry::default(),
                    ]),
                ),
            )
            .await?
            .target_infos)
    }

    async fn list_owned(self: &Arc<Self>) -> Result<Vec<Page>> {
        let _gate = tokio::time::timeout(Duration::from_secs(30), self.gate.lock())
            .await
            .map_err(|_| Error::Timeout("waiting to list pages".into()))?;
        self.ensure_open()?;
        let targets = self.target_snapshot().await?;
        // Chrome may report a default-profile ID which Storage must not receive.
        // This branch supports the same helper's eventual default-scope caller;
        // Context.pages always filters by the exact explicit context ID.
        let isolated = if self.scope == Scope::Default {
            self.connection
                .clone()
                .with_command_timeout(Some(Duration::from_secs(30)))
                .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                .await?
                .browser_context_ids
        } else {
            Vec::new()
        };
        let mut targets: Vec<_> = targets
            .into_iter()
            .filter(|target| {
                target.type_ == "page"
                    && match &self.scope {
                        Scope::Context(id) => target.browser_context_id.as_ref() == Some(id),
                        Scope::Default => !target
                            .browser_context_id
                            .as_ref()
                            .is_some_and(|id| isolated.contains(id)),
                    }
            })
            .collect();
        targets.sort_by(|a, b| a.target_id.0.cmp(&b.target_id.0));
        let mut pages = Vec::with_capacity(targets.len());
        for target in targets {
            self.ensure_open()?;
            if let Some(page) = self.adopt_owned(&target.target_id).await? {
                pages.push(page);
            }
        }
        // Atomically publish the complete successful selection while holding the
        // gate. In particular, a PendingPage guard cannot later delete a created
        // tab now adopted by this enumeration. Failed lists publish no pending IDs.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("page scope is closing".into()));
        }
        for page in &pages {
            state.mark_delivered(page.id());
        }
        Ok(pages)
    }

    // Caller owns the scope gate. Reuse this for later browser lookup as well as
    // enumeration: this operation never owns permission to close an existing tab.
    async fn adopt_owned(self: &Arc<Self>, id: &TargetId) -> Result<Option<Page>> {
        let (cached, proxy) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state
                    .entries
                    .iter()
                    .find(|entry| entry.page.id() == id)
                    .map(|entry| entry.page.clone()),
                state.proxy.clone(),
            )
        };
        if let Some(page) = cached {
            if !page.session().is_closed() {
                return Ok(Some(page));
            }
            // A manual detach can leave the target alive. Await the old worker
            // before creating a replacement session rather than duplicate auth.
            page.stop_proxy_auth().await?;
        }
        let commands = self.connection.clone().with_command_timeout(None);
        let session = match commands.attach(id.clone()).await {
            Ok(session) => session,
            Err(error) => {
                // Only confirmed target disappearance is a skippable race.
                return match self.target_snapshot().await {
                    Ok(targets) if !targets.iter().any(|target| &target.target_id == id) => {
                        Ok(None)
                    }
                    Ok(_) => Err(error),
                    Err(reconcile) => with_cleanup(Err(error), Err(reconcile)),
                };
            }
        };
        let context = match &self.scope {
            Scope::Default => None,
            Scope::Context(id) => Some(id.clone()),
        };
        // Do not wrap these owned helpers in a cancellation-prone timeout. They
        // each bound their transitions and finish recovery before returning.
        let setup =
            Page::from_session(session.clone(), context, crate::page::TargetKind::Tab).await;
        let mut page = match setup {
            Ok(page) => page.with_command_timeout(self.connection.command_timeout()),
            Err(error) => return with_cleanup(Err(error), detach_adopted_session(&session).await),
        };
        if let Some(proxy) = proxy {
            match super::proxy::start_auth(&page, proxy).await {
                Ok(auth) => page.auth = auth,
                // Existing ProxyAuth fatal recovery can close its target or
                // connection. Await it, then detach; do not add acquisition close.
                Err(error) => {
                    return with_cleanup(Err(error), detach_adopted_session(&session).await);
                }
            }
        }
        // Preserve a created target's pending obligation until the whole list
        // succeeds, including when replacing a manually detached cached session.
        // Synchronous PendingPage publication does not acquire the gate, so read
        // the current obligation under the same lock that replaces the entry.
        let mut cached = page.clone();
        cached.context_lease = None;
        page.registration.bind(self);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let delivered = !state.unclaimed.contains(id);
        state.entries.retain(|entry| entry.page.id() != id);
        state.entries.push(Entry::new(cached, delivered));
        Ok(Some(page))
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

    fn register(self: &Arc<Self>, page: &Page, delivered: bool) {
        // Never introduce a cache -> Page -> ContextLease -> cache cycle.
        let mut page = page.clone();
        page.context_lease = None;
        page.registration.bind(self);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|entry| entry.page.id() != page.id());
        state.entries.push(Entry::new(page, delivered));
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
        let page = entry.page.clone();
        state.mark_delivered(id);
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
    disposed: Arc<AtomicBool>,
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

impl BrowserState {
    fn prune_disposed(&mut self) {
        self.known
            .retain(|entry| !entry.disposed.load(Ordering::Acquire));
    }
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
                disposed: lease.cleanup.closed.clone(),
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

    pub(super) async fn lookup(self: &Arc<Self>, id: Option<TargetId>) -> Result<Vec<Page>> {
        self.ensure_open()?;
        let browser = self.clone();
        let task = self
            .default
            .connection
            .runtime()
            .spawn(async move { browser.lookup_owned(id).await });
        let selected = if let Some(timeout) = self.default.connection.command_timeout() {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("looking up browser pages".into()))??
        } else {
            task.await?
        }?;
        // No await between checking closure and delivering the completed result.
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("browser is closing".into()));
        }
        for (scope, _) in &selected {
            scope.ensure_open()?;
        }
        Ok(selected.into_iter().map(|(_, page)| page).collect())
    }

    async fn lookup_owned(&self, id: Option<TargetId>) -> Result<Vec<(Arc<ManagedPages>, Page)>> {
        // Serialize browser lookup and context registration. No returned task
        // result owns this gate or any scope gate.
        let _acquisition =
            tokio::time::timeout(Duration::from_secs(30), self.acquisition_gate.lock())
                .await
                .map_err(|_| Error::Timeout("waiting to look up browser pages".into()))?;
        self.ensure_open()?;
        // The acquisition gate excludes discovery while confirmed tombstones
        // are removed. The following snapshot cannot predate their disposal.
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prune_disposed();
        let commands = self
            .default
            .connection
            .clone()
            .with_command_timeout(Some(Duration::from_secs(30)));
        // Bracket target discovery: a changing isolated-context set must not
        // accidentally route a foreign target through default proxy credentials.
        // Only discovery is retried; no attachment/setup has started here.
        let (mut targets, isolated) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut before = commands
                    .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                    .await?
                    .browser_context_ids;
                let targets = self.default.target_snapshot().await?;
                let mut after = commands
                    .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                    .await?
                    .browser_context_ids;
                before.sort_by(|a, b| a.0.cmp(&b.0));
                after.sort_by(|a, b| a.0.cmp(&b.0));
                if before == after {
                    break Ok::<_, Error>((targets, after));
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout("resolving browser page scopes".into()))??;
        // Reconcile foreign scopes using the complete snapshot, before applying
        // the caller's page filter. An empty context is still a live scope.
        let retired = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut retired = Vec::new();
            state.owned.retain(|scope| {
                let absent = match &scope.scope {
                    Scope::Default => false,
                    Scope::Context(id) => {
                        !isolated.contains(id)
                            && !targets
                                .iter()
                                .any(|target| target.browser_context_id.as_ref() == Some(id))
                    }
                };
                if absent {
                    scope.mark_closing();
                    retired.push(scope.clone());
                }
                !absent
            });
            retired
        };
        for scope in retired {
            let stopped = scope.stop_auth().await;
            scope.clear();
            stopped?;
        }
        targets.retain(|target| {
            target.type_ == "page" && id.as_ref().is_none_or(|id| id == &target.target_id)
        });
        targets.sort_by(|a, b| a.target_id.0.cmp(&b.target_id.0));

        // Each scope appears once so acquiring all selected gates cannot lock a
        // mutex twice. Context operations acquire only their own scope gate.
        let mut scopes: Vec<(Arc<ManagedPages>, Option<Arc<ContextLease>>)> = Vec::new();
        let mut routed = Vec::with_capacity(targets.len());
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closing {
                return Err(Error::InvalidArgument("browser is closing".into()));
            }
            for target in targets {
                let (scope, lease) = if let Some(context) = &target.browser_context_id {
                    if let Some(known) = state.known.iter().find(|known| &known.id == context) {
                        // Expired known scopes remain tombstones: never adopt a
                        // context still being disposed as credentialless foreign.
                        let scope = known.pages.upgrade().ok_or_else(|| {
                            Error::InvalidArgument("browser context is closing".into())
                        })?;
                        let lease = known
                            .fallback
                            .as_ref()
                            .map(|weak| {
                                weak.upgrade().ok_or_else(|| {
                                    Error::InvalidArgument("fallback context is closing".into())
                                })
                            })
                            .transpose()?;
                        (scope, lease)
                    } else if let Some(scope) = state
                        .owned
                        .iter()
                        .find(|scope| scope.scope == Scope::Context(context.clone()))
                    {
                        (scope.clone(), None)
                    } else if isolated.contains(context) {
                        let scope = Arc::new(ManagedPages::new(
                            self.default.connection.clone(),
                            Scope::Context(context.clone()),
                        ));
                        state.owned.push(scope.clone());
                        (scope, None)
                    } else {
                        (self.default.clone(), None)
                    }
                } else {
                    (self.default.clone(), None)
                };
                scope.ensure_open()?;
                let index = if let Some(index) = scopes
                    .iter()
                    .position(|(item, _)| Arc::ptr_eq(item, &scope))
                {
                    index
                } else {
                    scopes.push((scope, lease));
                    scopes.len() - 1
                };
                routed.push((index, target.target_id));
            }
        }
        let mut gates = Vec::with_capacity(scopes.len());
        for (scope, _) in &scopes {
            gates.push(
                tokio::time::timeout(Duration::from_secs(30), scope.gate.lock())
                    .await
                    .map_err(|_| Error::Timeout("waiting to adopt browser pages".into()))?,
            );
            scope.ensure_open()?;
        }
        let mut selected = Vec::with_capacity(routed.len());
        for (index, id) in routed {
            self.ensure_open()?;
            let (scope, fallback) = &scopes[index];
            scope.ensure_open()?;
            if let Some(mut page) = scope.adopt_owned(&id).await? {
                page.context_lease = fallback.clone();
                selected.push((scope.clone(), page));
            }
        }
        // Browser -> all selected scope states is the shutdown lock order.
        // Check every scope before publishing any pending-created target.
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("browser is closing".into()));
        }
        let mut states: Vec<_> = scopes
            .iter()
            .map(|(scope, _)| scope.state.lock().unwrap_or_else(|e| e.into_inner()))
            .collect();
        if states.iter().any(|state| state.closing) {
            return Err(Error::InvalidArgument("page scope is closing".into()));
        }
        for (scope, page) in &selected {
            let index = scopes
                .iter()
                .position(|(item, _)| Arc::ptr_eq(item, scope))
                .unwrap();
            states[index].mark_delivered(page.id());
        }
        Ok(selected)
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
        // Registration also holds acquisition_gate, so subsequent discovery
        // starts after every confirmed disposal reclaimed here.
        state.prune_disposed();
        state.known.retain(|entry| entry.id != lease.id);
        state.known.push(KnownContext {
            id: lease.id.clone(),
            pages: Arc::downgrade(&lease.cleanup.pages),
            disposed: lease.cleanup.closed.clone(),
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
    closed: Arc<AtomicBool>,
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
                closed: Arc::new(AtomicBool::new(false)),
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

    pub(super) async fn pages(&self) -> Result<Vec<Page>> {
        self.cleanup.pages.pages().await
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

async fn detach_adopted_session(session: &crate::Session) -> Result<()> {
    if session.is_closed() {
        Ok(())
    } else {
        let cleanup = session.detach().await;
        if cleanup.is_err() && !session.is_closed() {
            session.connection().close().await;
        }
        cleanup
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_churn_reclaims_confirmed_disposals_but_keeps_uncertain_tombstones() {
        let uncertain = Arc::new(AtomicBool::new(false));
        let mut state = BrowserState::default();
        state.known.push(KnownContext {
            id: BrowserContextId("uncertain".into()),
            pages: Weak::new(),
            disposed: uncertain.clone(),
            fallback: None,
        });
        for index in 0..1000 {
            let disposed = Arc::new(AtomicBool::new(false));
            state.known.push(KnownContext {
                id: BrowserContextId(index.to_string()),
                pages: Weak::new(),
                disposed: disposed.clone(),
                fallback: None,
            });
            state.prune_disposed();
            assert_eq!(
                state.known.len(),
                2,
                "expired scopes alone are not disposable"
            );
            disposed.store(true, Ordering::Release);
            state.prune_disposed();
            assert_eq!(state.known.len(), 1);
            assert_eq!(state.known[0].id.0, "uncertain");
        }
        uncertain.store(true, Ordering::Release);
        state.prune_disposed();
        assert!(state.known.is_empty());
    }

    #[tokio::test]
    #[ignore = "requires Chrome; verifies live context registry reclamation"]
    async fn context_registry_reclaims_closed_scopes_without_retiring_live_foreign_contexts() {
        use crate::{
            Browser,
            cdp::target::commands::{CreateBrowserContext, CreateTarget, DisposeBrowserContext},
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            let browser = Browser::builder()
                .headless(true)
                .arg("--no-sandbox")
                .launch()
                .await
                .unwrap();
            for _ in 0..8 {
                let mut context = browser.new_context().await.unwrap();
                let (closed, discovered) = tokio::join!(context.close(), browser.pages());
                closed.unwrap();
                // Discovery overlapping disposal may conservatively reject closing scopes.
                if let Err(error) = discovered {
                    assert!(matches!(error, Error::InvalidArgument(_)));
                }
                browser.pages().await.unwrap();
                assert!(browser.pages.state.lock().unwrap().known.is_empty());
            }
            let foreign = browser
                .connection()
                .execute(&CreateBrowserContext::new())
                .await
                .unwrap()
                .browser_context_id;
            let target = browser
                .connection()
                .execute(&CreateTarget::new("about:blank").with_browser_context_id(foreign.clone()))
                .await
                .unwrap()
                .target_id;
            let page = browser.page_by_id(&target).await.unwrap().unwrap();
            assert_eq!(browser.pages.state.lock().unwrap().owned.len(), 2);
            // A filtered lookup must not mistake unselected foreign targets for absence.
            browser
                .page_by_id(browser.page().id())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(browser.pages.state.lock().unwrap().owned.len(), 2);
            page.close().await.unwrap();
            browser.pages().await.unwrap();
            assert_eq!(
                browser.pages.state.lock().unwrap().owned.len(),
                2,
                "empty live context retained"
            );
            browser
                .connection()
                .execute(&DisposeBrowserContext::new(foreign))
                .await
                .unwrap();
            browser.pages().await.unwrap();
            assert_eq!(
                browser.pages.state.lock().unwrap().owned.len(),
                1,
                "disposed foreign scope reclaimed"
            );
        })
        .await
        .expect("context churn must complete within one minute");
    }
}

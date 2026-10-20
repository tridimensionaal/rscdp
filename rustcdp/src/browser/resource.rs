use crate::{
    Connection, Error, Page, Result,
    cdp::{browser::BrowserContextId, target::TargetId},
    error::with_cleanup,
    proxy::{Proxy, ProxyAuth},
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle};

#[derive(Default)]
struct State {
    closing: bool,
    pages: Vec<Page>,
    proxy: Option<Proxy>,
    pending_auth: Option<Arc<ProxyAuth>>,
}

#[derive(Default)]
struct Cleanup {
    gate: Mutex<()>,
    closed: AtomicBool,
    state: std::sync::Mutex<State>,
}

pub(crate) struct ContextLease {
    connection: Connection,
    id: BrowserContextId,
    cleanup: Arc<Cleanup>,
}

impl ContextLease {
    pub(super) fn new(connection: Connection, id: BrowserContextId) -> Self {
        Self {
            connection,
            id,
            cleanup: Arc::default(),
        }
    }

    pub(super) fn id(&self) -> Option<&BrowserContextId> {
        (!self.cleanup.closed.load(Ordering::Acquire)).then_some(&self.id)
    }

    pub(super) fn register_page(&self, page: &Page, proxy: Option<Proxy>) {
        // Cached pages must never own this lease, including the fallback page.
        let mut page = page.clone();
        page.context_lease = None;
        let mut state = self.cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pages.retain(|existing| existing.id() != page.id());
        state.pages.push(page);
        state.proxy = proxy;
    }

    pub(super) async fn new_page(&self) -> Result<Page> {
        let connection = self.connection.clone();
        let cleanup = self.cleanup.clone();
        let id = self.id.clone();
        let task = connection
            .runtime()
            .spawn(async move { acquire_page(connection, cleanup, id).await });
        let pending = if let Some(timeout) = self.connection.command_timeout() {
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
            let mut state = self.cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closing = true;
            if auth.is_some() {
                state.pending_auth = auth;
            }
        }
        let connection = self.connection.clone();
        let cleanup = self.cleanup.clone();
        let id = self.id.clone();
        self.connection.runtime().spawn(async move {
            let _gate = cleanup.gate.lock().await;
            dispose_context(&connection, &cleanup, &id).await
        })
    }

    pub(super) fn register_auth(&self, auth: Option<Arc<ProxyAuth>>) {
        if let Some(auth) = auth {
            self.cleanup
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending_auth = Some(auth);
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
    connection: Connection,
    cleanup: Arc<Cleanup>,
    context: BrowserContextId,
    page: Option<Page>,
}

impl PendingPage {
    fn publish(mut self) -> Result<Page> {
        let state = self.cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closing {
            return Err(Error::InvalidArgument("browser context is closing".into()));
        }
        // No await between the closing check and transferring ownership.
        Ok(self
            .page
            .take()
            .expect("pending page has not been delivered"))
    }
}

impl Drop for PendingPage {
    fn drop(&mut self) {
        let Some(page) = self.page.take() else {
            return;
        };
        let connection = self.connection.clone();
        let cleanup = self.cleanup.clone();
        let context = self.context.clone();
        self.connection.runtime().spawn(async move {
            let _gate = cleanup.gate.lock().await;
            if !cleanup.closed.load(Ordering::Acquire) {
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
    let _gate = tokio::time::timeout(Duration::from_secs(30), cleanup.gate.lock())
        .await
        .map_err(|_| Error::Timeout("waiting to create context page".into()))?;
    let proxy = {
        let state = cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
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
    cleanup
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pages
        .push(page.clone());
    // This result owns no gate. Context close can run even if nobody ever polls
    // the completed acquisition handle to retrieve or drop its result.
    Ok(PendingPage {
        connection,
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
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pages
        .iter()
        .find(|page| page.id() == id)
        .cloned();
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
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pages
            .retain(|page| page.id() != id);
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
        let mut state = cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closing = true;
        (state.pages.clone(), state.pending_auth.take())
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
            let mut state = cleanup.state.lock().unwrap_or_else(|e| e.into_inner());
            state.pages.clear();
            state.proxy = None;
        }
        result.map(|_| ())
    };
    // Keep the cloned Pages (and sessions/workers) alive through disposal.
    drop(pages);
    with_cleanup(auth_result, result)
}

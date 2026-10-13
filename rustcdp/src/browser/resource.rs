use crate::{
    Connection, Error, Result, cdp::browser::BrowserContextId, error::with_cleanup,
    proxy_auth::ProxyAuth,
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
struct Cleanup {
    gate: Mutex<()>,
    closed: AtomicBool,
    pending_auth: std::sync::Mutex<Option<ProxyAuth>>,
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

    pub(super) async fn close(&self) -> Result<()> {
        self.close_with_auth(None).await?
    }

    pub(super) fn close_with_auth(&self, auth: Option<ProxyAuth>) -> JoinHandle<Result<()>> {
        self.register_auth(auth);
        self.connection.runtime().spawn(dispose_context(
            self.connection.clone(),
            self.cleanup.clone(),
            self.id.clone(),
        ))
    }

    pub(super) fn register_auth(&self, auth: Option<ProxyAuth>) {
        // Transfer the context's auth owner before any cleanup task can race
        // for the gate. Retries with no auth must leave this pending owner intact.
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

async fn dispose_context(
    connection: Connection,
    cleanup: Arc<Cleanup>,
    id: BrowserContextId,
) -> Result<()> {
    let _gate = cleanup.gate.lock().await;
    let mut auth = cleanup
        .pending_auth
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let auth_result = if let Some(auth) = &mut auth {
        auth.stop().await
    } else {
        Ok(())
    };
    let result = async {
        if cleanup.closed.load(Ordering::Acquire) {
            return Ok(());
        }
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
        }
        result.map(|_| ())
    }
    .await;
    with_cleanup(auth_result, result)
}

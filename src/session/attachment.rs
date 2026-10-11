use crate::{Connection, Error, Result};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, watch};

const DETACH_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Attachment {
    connection: Connection,
    runtime: tokio::runtime::Handle,
    id: String,
    parent: Option<String>,
    closed: watch::Receiver<bool>,
    gate: Arc<Mutex<()>>,
    owned: Arc<AtomicBool>,
}

impl Attachment {
    pub(crate) fn new(
        connection: Connection,
        id: String,
        parent: Option<String>,
        closed: watch::Receiver<bool>,
        gate: Arc<Mutex<()>>,
        owned: Arc<AtomicBool>,
    ) -> Self {
        Self {
            runtime: connection.runtime(),
            connection: connection.with_command_timeout(Some(DETACH_TIMEOUT)),
            id,
            parent,
            closed,
            gate,
            owned,
        }
    }

    pub(super) fn arm(&self) {
        self.owned.store(true, Ordering::Release);
    }

    pub(super) async fn detach(&self, connection: Connection) -> Result<()> {
        detach(
            connection,
            self.id.clone(),
            self.parent.clone(),
            self.closed.clone(),
            self.gate.clone(),
        )
        .await
    }
}

async fn detach(
    connection: Connection,
    id: String,
    parent: Option<String>,
    closed: watch::Receiver<bool>,
    gate: Arc<Mutex<()>>,
) -> Result<()> {
    let _gate = gate.lock().await;
    if *closed.borrow() || connection.is_closed() {
        return Ok(());
    }
    let result = tokio::time::timeout(
        DETACH_TIMEOUT,
        connection.execute_session("Target.detachFromTarget", json!({"sessionId":id}), parent),
    )
    .await
    .unwrap_or_else(|_| Err(Error::Timeout("Target.detachFromTarget".into())));
    if matches!(&result, Err(Error::Timeout(_))) {
        if *closed.borrow() || connection.is_closed() {
            return Ok(());
        }
        connection.close().await;
    }
    result.map(|_| ())
}

impl Drop for Attachment {
    fn drop(&mut self) {
        if self.owned.load(Ordering::Acquire)
            && !*self.closed.borrow()
            && !self.connection.is_closed()
        {
            self.runtime.spawn(detach(
                self.connection.clone(),
                self.id.clone(),
                self.parent.clone(),
                self.closed.clone(),
                self.gate.clone(),
            ));
        }
    }
}

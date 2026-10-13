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
    state: Option<DetachState>,
    parent_attachment: Option<Arc<Attachment>>,
}

struct DetachState {
    connection: Connection,
    runtime: tokio::runtime::Handle,
    id: String,
    closed: watch::Receiver<bool>,
    gate: Arc<Mutex<()>>,
    owned: Arc<AtomicBool>,
}

impl Attachment {
    pub(crate) fn new(
        connection: Connection,
        id: String,
        closed: watch::Receiver<bool>,
        gate: Arc<Mutex<()>>,
        owned: Arc<AtomicBool>,
        parent_attachment: Option<Arc<Attachment>>,
    ) -> Self {
        Self {
            state: Some(DetachState {
                runtime: connection.runtime(),
                connection: connection.with_command_timeout(Some(DETACH_TIMEOUT)),
                id,
                closed,
                gate,
                owned,
            }),
            parent_attachment,
        }
    }

    pub(super) fn arm(&self) {
        self.state
            .as_ref()
            .expect("live attachment")
            .owned
            .store(true, Ordering::Release);
    }

    pub(super) async fn detach(&self, connection: Connection) -> Result<()> {
        let state = self.state.as_ref().expect("live attachment");
        detach(
            connection,
            state.id.clone(),
            parent_id(&self.parent_attachment),
            state.closed.clone(),
            state.gate.clone(),
        )
        .await
    }

    fn release(&mut self) -> Option<Arc<Attachment>> {
        // Clear both fields before any cleanup or automatic Drop can run again.
        let state = self.state.take();
        let parent = self.parent_attachment.take();
        let Some(state) = state else {
            return parent;
        };
        if !state.owned.load(Ordering::Acquire)
            || *state.closed.borrow()
            || state.connection.is_closed()
        {
            return parent;
        }
        let route = parent_id(&parent);
        // Construct before spawning: a stopped runtime may drop an unpolled
        // future synchronously, including cleanup for a root with no parent.
        let parent = ParentChain(Some((state.connection.clone(), parent)));
        state.runtime.spawn(async move {
            let _ = detach(state.connection, state.id, route, state.closed, state.gate).await;
            parent.complete();
        });
        None
    }
}

fn parent_id(parent: &Option<Arc<Attachment>>) -> Option<String> {
    parent.as_ref().map(|parent| {
        parent
            .state
            .as_ref()
            .expect("live parent attachment")
            .id
            .clone()
    })
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
        release_chain(self.release());
    }
}

struct ParentChain(Option<(Connection, Option<Arc<Attachment>>)>);

impl ParentChain {
    fn complete(mut self) {
        if let Some((_connection, parent)) = self.0.take() {
            release_chain(parent);
        }
    }
}

impl Drop for ParentChain {
    fn drop(&mut self) {
        if let Some((connection, parent)) = self.0.take() {
            // Prevent ancestor release from spawning more work onto a runtime
            // that rejected or abandoned this task.
            connection.signal_shutdown();
            release_chain(parent);
        }
    }
}

fn release_chain(mut parent: Option<Arc<Attachment>>) {
    while let Some(attachment) = parent {
        let Some(mut attachment) = Arc::into_inner(attachment) else {
            break;
        };
        parent = attachment.release();
    }
}

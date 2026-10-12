use super::{CloseState, Connection, EventKey, RawEvent, close_error, wait_closed_state};
use crate::{Error, Result};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};

const CAPACITY: usize = 4096;
const TEXT_BUDGET: usize = 4 * 1024 * 1024;
const OVERFLOW: &str = "authentication event queue exceeded its capacity";

type Failure = watch::Receiver<Option<&'static str>>;
pub(super) type AuthSubscriptions = Arc<Mutex<HashMap<EventKey, Vec<Weak<Sender>>>>>;

pub(crate) struct AuthEvent {
    pub(crate) id: String,
    pub(crate) kind: AuthEventKind,
}

pub(crate) enum AuthEventKind {
    Request,
    Response {
        rejected: bool,
        failed: bool,
    },
    Challenge {
        is_proxy: bool,
        origin: Option<String>,
    },
}

struct QueuedEvent {
    event: AuthEvent,
    _permit: OwnedSemaphorePermit,
}

pub(super) struct Sender {
    sender: mpsc::Sender<QueuedEvent>,
    budget: Arc<Semaphore>,
    failure: watch::Sender<Option<&'static str>>,
}

impl Sender {
    fn send(&self, event: &RawEvent) {
        if self.failure.borrow().is_some() {
            return;
        }
        let Some(id) = event.params["requestId"].as_str() else {
            self.failure
                .send_replace(Some("authentication event has no request ID"));
            return;
        };
        let challenge = event.method == "Fetch.authRequired";
        let origin = challenge
            .then(|| event.params["authChallenge"]["origin"].as_str())
            .flatten();
        let bytes = id.len().saturating_add(origin.map_or(0, str::len));
        if bytes > TEXT_BUDGET {
            self.failure.send_replace(Some(OVERFLOW));
            return;
        }
        let Ok(permit) = self.budget.clone().try_acquire_many_owned(bytes as u32) else {
            self.failure.send_replace(Some(OVERFLOW));
            return;
        };
        // Keep only fields needed for authentication; headers and bodies can be large.
        let kind = if challenge {
            AuthEventKind::Challenge {
                is_proxy: event.params["authChallenge"]["source"] == "Proxy",
                origin: origin.map(str::to_owned),
            }
        } else if event.params.get("responseStatusCode").is_some()
            || event.params.get("responseErrorReason").is_some()
        {
            AuthEventKind::Response {
                rejected: event.params["responseStatusCode"] == 407,
                failed: event.params.get("responseErrorReason").is_some(),
            }
        } else {
            AuthEventKind::Request
        };
        let queued = QueuedEvent {
            event: AuthEvent {
                id: id.into(),
                kind,
            },
            _permit: permit,
        };
        if self.sender.try_send(queued).is_err() {
            // Never block the socket reader: it must keep receiving command replies.
            self.failure.send_replace(Some(OVERFLOW));
        }
    }
}

pub(super) fn dispatch(subscriptions: &AuthSubscriptions, event: &RawEvent) {
    let key = (event.session_id.clone(), event.method.clone());
    if let Some(senders) = subscriptions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        for sender in senders.iter().filter_map(Weak::upgrade) {
            sender.send(event);
        }
    }
}

impl Connection {
    pub(crate) fn subscribe_auth(&self, session: &str) -> AuthEvents {
        let (sender, receiver) = mpsc::channel(CAPACITY);
        let (failed, failure) = watch::channel(None);
        let sender = Arc::new(Sender {
            sender,
            budget: Arc::new(Semaphore::new(TEXT_BUDGET)),
            failure: failed,
        });
        let mut subscriptions = self
            .0
            .auth_subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        subscriptions.retain(|_, senders| {
            senders.retain(|sender| sender.strong_count() > 0);
            !senders.is_empty()
        });
        for method in ["Fetch.authRequired", "Fetch.requestPaused"] {
            subscriptions
                .entry((Some(session.into()), method.into()))
                .or_default()
                .push(Arc::downgrade(&sender));
        }
        AuthEvents {
            receiver,
            _sender: sender,
            failure,
            closed: self.0.closed.clone(),
        }
    }
}

// An overflow is fatal: dropping paused requests silently would strand them.
pub(crate) struct AuthEvents {
    receiver: mpsc::Receiver<QueuedEvent>,
    _sender: Arc<Sender>,
    failure: Failure,
    closed: watch::Receiver<CloseState>,
}

impl AuthEvents {
    pub(crate) fn failure(&self) -> Failure {
        self.failure.clone()
    }

    pub(crate) async fn recv(&mut self) -> Result<AuthEvent> {
        loop {
            if let Some(reason) = *self.failure.borrow() {
                return Err(Error::ProxyAuthentication(reason.into()));
            }
            let terminal = self.closed.borrow().clone();
            match self.receiver.try_recv() {
                Ok(queued) => return Ok(queued.event),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(close_error(&self.closed.borrow()));
                }
            }
            if terminal.is_some() {
                return Err(close_error(&terminal));
            }
            tokio::select! {
                _ = self.failure.changed() => {}
                error = wait_closed_state(&mut self.closed) => {
                    // Drain a final queued event before reporting terminal state.
                    if let Some(reason) = *self.failure.borrow() {
                        return Err(Error::ProxyAuthentication(reason.into()));
                    }
                    return match self.receiver.try_recv() {
                        Ok(queued) => Ok(queued.event),
                        Err(_) => Err(error),
                    };
                }
                event = self.receiver.recv() => return event.map(|queued| queued.event).ok_or_else(|| close_error(&self.closed.borrow())),
            }
        }
    }
}

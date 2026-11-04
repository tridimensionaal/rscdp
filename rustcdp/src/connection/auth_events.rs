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
    pub(crate) network_id: Option<String>,
    pub(crate) kind: AuthEventKind,
}

pub(crate) enum AuthEventKind {
    Request,
    NetworkFailure,
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
        let Some(id) = event.params["requestId"]
            .as_str()
            .filter(|id| !id.is_empty())
        else {
            self.failure
                .send_replace(Some("authentication event has no request ID"));
            return;
        };
        let network_id = if event.method == "Fetch.requestPaused" {
            match event.params.get("networkId") {
                None => None,
                Some(value) => match value.as_str().filter(|id| !id.is_empty()) {
                    Some(id) => Some(id),
                    None => {
                        self.failure
                            .send_replace(Some("authentication event has invalid Network ID"));
                        return;
                    }
                },
            }
        } else {
            None
        };
        let challenge = event.method == "Fetch.authRequired";
        let origin = challenge
            .then(|| event.params["authChallenge"]["origin"].as_str())
            .flatten();
        let bytes = id
            .len()
            .saturating_add(origin.map_or(0, str::len))
            .saturating_add(network_id.map_or(0, str::len));
        if bytes > TEXT_BUDGET {
            self.failure.send_replace(Some(OVERFLOW));
            return;
        }
        let Ok(permit) = self.budget.clone().try_acquire_many_owned(bytes as u32) else {
            self.failure.send_replace(Some(OVERFLOW));
            return;
        };
        // Keep only fields needed for authentication; headers and bodies can be large.
        let kind = if event.method == "Network.loadingFailed" {
            AuthEventKind::NetworkFailure
        } else if challenge {
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
                network_id: network_id.map(str::to_owned),
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
        for method in [
            "Fetch.authRequired",
            "Fetch.requestPaused",
            "Network.loadingFailed",
        ] {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sender() -> (Arc<Sender>, mpsc::Receiver<QueuedEvent>) {
        let (sender, receiver) = mpsc::channel(CAPACITY);
        let (failure, _) = watch::channel(None);
        (
            Arc::new(Sender {
                sender,
                budget: Arc::new(Semaphore::new(TEXT_BUDGET)),
                failure,
            }),
            receiver,
        )
    }

    #[test]
    fn network_failure_is_not_a_fetch_request() {
        let (sender, mut events) = sender();
        sender.send(
            &serde_json::from_value(json!({
                "method":"Network.loadingFailed", "sessionId":"session",
                "params":{"requestId":"network-id", "errorText":"net::ERR_ABORTED"}
            }))
            .unwrap(),
        );
        let event = events.try_recv().unwrap().event;
        assert_eq!(event.id, "network-id");
        assert!(!matches!(event.kind, AuthEventKind::Request));
    }
    #[test]
    fn fetch_and_network_ids_are_distinct_and_budgeted() {
        let (sender, mut events) = sender();
        sender.send(&serde_json::from_value(json!({
            "method":"Fetch.requestPaused", "params":{"requestId":"fetch-id","networkId":"network-id"}
        })).unwrap());
        let queued = events.try_recv().unwrap();
        assert_eq!(queued.event.id, "fetch-id");
        assert_eq!(queued.event.network_id.as_deref(), Some("network-id"));
        assert!(matches!(queued.event.kind, AuthEventKind::Request));
        assert_eq!(sender.budget.available_permits(), TEXT_BUDGET - 18);
        drop(queued);
        assert_eq!(sender.budget.available_permits(), TEXT_BUDGET);
    }

    #[test]
    fn malformed_and_oversized_events_fail_explicitly() {
        for params in [
            json!({}),
            json!({"requestId":""}),
            json!({"requestId":42}),
            json!({"requestId":"fetch", "networkId":42}),
            json!({"requestId":"fetch", "networkId":""}),
            json!({"requestId":"fetch", "networkId":"x".repeat(TEXT_BUDGET)}),
        ] {
            let (sender, mut events) = sender();
            sender.send(
                &serde_json::from_value(json!({"method":"Fetch.requestPaused","params":params}))
                    .unwrap(),
            );
            assert!(sender.failure.borrow().is_some());
            assert!(events.try_recv().is_err());
        }
    }

    #[test]
    fn other_sessions_do_not_reach_authentication_and_overflow_is_explicit() {
        let (sender, mut events) = sender();
        let subscriptions = Arc::new(Mutex::new(HashMap::from([(
            (Some("selected".into()), "Network.loadingFailed".into()),
            vec![Arc::downgrade(&sender)],
        )])));
        let mut event:RawEvent=serde_json::from_value(json!({"sessionId":"other","method":"Network.loadingFailed","params":{"requestId":"network"}})).unwrap();
        dispatch(&subscriptions, &event);
        assert!(events.try_recv().is_err());
        event.session_id = Some("selected".into());
        for _ in 0..CAPACITY {
            dispatch(&subscriptions, &event);
        }
        assert!(sender.failure.borrow().is_none());
        dispatch(&subscriptions, &event);
        assert!(sender.failure.borrow().is_some());
        assert!(matches!(
            events.try_recv().unwrap().event.kind,
            AuthEventKind::NetworkFailure
        ));
    }
}

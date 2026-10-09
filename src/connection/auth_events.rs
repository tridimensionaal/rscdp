use super::{Connection, EventKey, RawEvent};
use crate::{Error, Result};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{mpsc, watch};

type Sender = mpsc::UnboundedSender<RawEvent>;
pub(super) type AuthSubscriptions = Arc<Mutex<HashMap<EventKey, Vec<Weak<Sender>>>>>;

pub(super) fn dispatch(subscriptions: &AuthSubscriptions, event: &RawEvent) {
    let key = (event.session_id.clone(), event.method.clone());
    if let Some(senders) = subscriptions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        for sender in senders.iter().filter_map(Weak::upgrade) {
            let _ = sender.send(event.clone());
        }
    }
}

impl Connection {
    pub(crate) fn subscribe_auth(&self, session: &str) -> AuthEvents {
        let (sender, receiver) = mpsc::unbounded_channel();
        let sender = Arc::new(sender);
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
            closed: self.0.closed.clone(),
        }
    }
}

// Fetch pauses browser requests until answered: unlike observational events,
// these must never be dropped. The handler separately bounds held requests.
pub(crate) struct AuthEvents {
    receiver: mpsc::UnboundedReceiver<RawEvent>,
    _sender: Arc<Sender>,
    closed: watch::Receiver<Option<String>>,
}

impl AuthEvents {
    pub(crate) async fn recv(&mut self) -> Result<RawEvent> {
        loop {
            let reason = self.closed.borrow().clone();
            match self.receiver.try_recv() {
                Ok(event) => return Ok(event),
                Err(mpsc::error::TryRecvError::Empty) => {}
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(Error::ConnectionClosed("event sender stopped".into()));
                }
            }
            if let Some(reason) = reason {
                return Err(Error::ConnectionClosed(reason));
            }
            tokio::select! {
                result = self.closed.changed() => {
                    if result.is_err() { return Err(Error::ConnectionClosed("reader stopped".into())); }
                }
                event = self.receiver.recv() => return event.ok_or_else(|| Error::ConnectionClosed("event sender stopped".into())),
            }
        }
    }
}

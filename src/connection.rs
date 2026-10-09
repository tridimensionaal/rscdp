use crate::{Error, Result, error::ProtocolError, protocol::Command};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const CAPACITY: usize = 256;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEvent {
    pub method: String,
    #[serde(default)]
    pub params: Value,
    pub session_id: Option<String>,
}

struct Request {
    method: String,
    params: Value,
    session_id: Option<String>,
    reply: oneshot::Sender<Result<Value>>,
}

struct Inner {
    requests: mpsc::Sender<Request>,
    events: broadcast::Sender<RawEvent>,
    shutdown: watch::Sender<bool>,
    closed: watch::Receiver<Option<String>>,
    permits: Semaphore,
    sessions: SessionRegistry,
    subscriptions: Subscriptions,
}

struct SessionEntry {
    parent: Option<String>,
    closed: watch::Sender<bool>,
}

type EventKey = (Option<String>, String);
type Subscriptions = Arc<Mutex<HashMap<EventKey, Weak<broadcast::Sender<RawEvent>>>>>;

type SessionRegistry = Arc<Mutex<HashMap<String, SessionEntry>>>;

fn close_sessions(registry: &SessionRegistry, id: &str) -> Vec<String> {
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    let mut removed = vec![id.to_owned()];
    let mut index = 0;
    while index < removed.len() {
        let id = removed[index].clone();
        removed.extend(
            registry
                .iter()
                .filter(|(_, entry)| entry.parent.as_deref() == Some(&id))
                .map(|(id, _)| id.clone()),
        );
        if let Some(entry) = registry.remove(&id) {
            entry.closed.send_replace(true);
        }
        index += 1;
    }
    removed
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

/// A cloneable handle to one CDP socket. The reader task starts automatically.
#[derive(Clone)]
pub struct Connection(Arc<Inner>);

impl Connection {
    /// Connect to a ws/wss endpoint or discover it via HTTP(S) `/json/version`.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        tokio::time::timeout(TIMEOUT, Self::connect_inner(endpoint))
            .await
            .map_err(|_| Error::Timeout("connecting to CDP".into()))?
    }

    async fn connect_inner(endpoint: &str) -> Result<Self> {
        let url = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            #[derive(Deserialize)]
            struct Version {
                #[serde(rename = "webSocketDebuggerUrl")]
                websocket_url: String,
            }
            reqwest::Client::new()
                .get(format!("{}/json/version", endpoint.trim_end_matches('/')))
                .send()
                .await?
                .error_for_status()?
                .json::<Version>()
                .await?
                .websocket_url
        } else if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
            endpoint.into()
        } else {
            return Err(Error::InvalidArgument(
                "expected http(s) or ws(s) debugging URL".into(),
            ));
        };
        let (socket, _) = connect_async(url).await?;
        let (requests, mut incoming) = mpsc::channel::<Request>(CAPACITY);
        let (events, _) = broadcast::channel(CAPACITY);
        let event_sender = events.clone();
        let (shutdown, mut stop) = watch::channel(false);
        let (closed_sender, closed) = watch::channel(None);
        let sessions = SessionRegistry::default();
        let active_sessions = sessions.clone();
        let subscriptions = Subscriptions::default();
        let active_subscriptions = subscriptions.clone();
        tokio::spawn(async move {
            let (mut sink, mut stream) = socket.split();
            let mut pending = HashMap::<u64, Request>::new();
            let mut id = 0u64;
            let mut cleanup = tokio::time::interval(Duration::from_secs(1));
            let outcome: Result<()> = async {
                loop {
                    tokio::select! {
                        _ = stop.changed() => break,
                        message = stream.next() => {
                            let Some(message) = message else { break; };
                            match message? {
                                Message::Text(text) => {
                                    let value: Value = serde_json::from_str(&text)?;
                                    if let Some(id) = value["id"].as_u64() {
                                        if let Some(request) = pending.remove(&id) {
                                            let result = if value.get("error").is_some() {
                                                Err(serde_json::from_value::<ProtocolError>(value["error"].clone())?.into())
                                            } else {
                                                value.get("result").cloned().ok_or_else(|| Error::ConnectionClosed("response missing result".into()))
                                            };
                                            if matches!(request.method.as_str(), "Target.attachToTarget" | "Target.attachToBrowserTarget")
                                                && let Ok(result) = &result
                                                && let Some(id) = result["sessionId"].as_str()
                                            {
                                                active_sessions.lock().unwrap_or_else(|e| e.into_inner())
                                                    .entry(id.into()).or_insert_with(|| SessionEntry { parent: request.session_id.clone(), closed: watch::channel(false).0 });
                                            }
                                            let _ = request.reply.send(result);
                                        }
                                    } else {
                                        let event: RawEvent = serde_json::from_value(value)?;
                                        if let Some(id) = event.params["sessionId"].as_str() {
                                            if event.method == "Target.attachedToTarget" {
                                                active_sessions.lock().unwrap_or_else(|e| e.into_inner())
                                                    .entry(id.into()).or_insert_with(|| SessionEntry { parent: event.session_id.clone(), closed: watch::channel(false).0 });
                                            } else if event.method == "Target.detachedFromTarget" {
                                                let removed = close_sessions(&active_sessions, id);
                                                let cancelled: Vec<_> = pending.keys().copied().filter(|key| pending[key].session_id.as_ref().is_some_and(|id| removed.contains(id))).collect();
                                                for key in cancelled {
                                                    if let Some(request) = pending.remove(&key) {
                                                        let _ = request.reply.send(Err(Error::SessionClosed(request.session_id.unwrap_or_default())));
                                                    }
                                                }
                                            }
                                        }
                                        // No subscribers is normal; subscribed readers detect lag explicitly.
                                        let key = (event.session_id.clone(), event.method.clone());
                                        if let Some(sender) = active_subscriptions.lock().unwrap_or_else(|e| e.into_inner()).get(&key).and_then(Weak::upgrade) {
                                            let _ = sender.send(event.clone());
                                        }
                                        let _ = event_sender.send(event);
                                    }
                                }
                                Message::Close(_) => break,
                                Message::Ping(_) => {
                                    tokio::select! {
                                        _ = stop.changed() => break,
                                        result = tokio::time::timeout(TIMEOUT, sink.flush()) => {
                                            result.map_err(|_| Error::Timeout("writing WebSocket pong".into()))??;
                                        }
                                    }
                                }
                                Message::Pong(_) => {}
                                _ => return Err(Error::ConnectionClosed("non-text CDP frame".into())),
                            }
                        }
                        request = incoming.recv() => {
                            let Some(request) = request else { break; };
                            pending.retain(|_, p| !p.reply.is_closed());
                            if request.reply.is_closed() { continue; }
                            if let Some(id) = &request.session_id
                                && !active_sessions.lock().unwrap_or_else(|e| e.into_inner()).contains_key(id)
                            {
                                let _ = request.reply.send(Err(Error::SessionClosed(id.clone())));
                                continue;
                            }
                            id = id.checked_add(1).ok_or_else(|| Error::ConnectionClosed("command ID space exhausted".into()))?;
                            let mut message = json!({"id":id,"method":request.method,"params":request.params});
                            if let Some(session_id) = &request.session_id { message["sessionId"] = session_id.clone().into(); }
                            pending.insert(id, request);
                            let send = sink.send(Message::Text(serde_json::to_string(&message)?.into()));
                            tokio::select! {
                                _ = stop.changed() => break,
                                result = tokio::time::timeout(TIMEOUT, send) => {
                                    result.map_err(|_| Error::Timeout("writing CDP command".into()))??;
                                }
                            }
                        }
                        _ = cleanup.tick() => pending.retain(|_, p| !p.reply.is_closed()),
                    }
                }
                Ok(())
            }.await;
            let reason = outcome
                .err()
                .map_or_else(|| "socket closed".into(), |e| e.to_string());
            closed_sender.send_replace(Some(reason.clone()));
            for (_, state) in active_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
            {
                state.closed.send_replace(true);
            }
            incoming.close();
            for (_, request) in pending {
                let _ = request
                    .reply
                    .send(Err(Error::ConnectionClosed(reason.clone())));
            }
            // Never let a peer that ignores close hold up shutdown.
            let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
        });
        Ok(Self(Arc::new(Inner {
            requests,
            events,
            shutdown,
            closed,
            permits: Semaphore::new(CAPACITY),
            sessions,
            subscriptions,
        })))
    }

    pub fn is_closed(&self) -> bool {
        *self.0.shutdown.borrow() || self.0.closed.borrow().is_some()
    }

    pub async fn execute<C: Command>(&self, command: &C) -> Result<C::Response> {
        serde_json::from_value(
            self.execute_raw(C::METHOD, serde_json::to_value(command)?)
                .await?,
        )
        .map_err(Into::into)
    }

    pub async fn execute_raw(&self, method: &str, params: Value) -> Result<Value> {
        self.execute_session(method, params, None).await
    }

    pub(crate) fn session_state(
        &self,
        id: &str,
    ) -> Result<(watch::Receiver<bool>, Option<String>)> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|entry| (entry.closed.subscribe(), entry.parent.clone()))
            .ok_or_else(|| Error::SessionClosed(id.into()))
    }

    pub(crate) async fn execute_session(
        &self,
        method: &str,
        params: Value,
        session_id: Option<String>,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(Error::ConnectionClosed("connection is closed".into()));
        }
        if !params.is_object() {
            return Err(Error::InvalidArgument(
                "CDP params must be an object".into(),
            ));
        }
        let mut session_closed = session_id
            .as_deref()
            .map(|id| self.session_state(id).map(|state| state.0))
            .transpose()?;
        let mut connection_closed = self.0.closed.clone();
        tokio::time::timeout(TIMEOUT, async {
            let admission = async {
                let permit = self.0.permits.acquire().await.map_err(|_| Error::ConnectionClosed("connection is closed".into()))?;
                let slot = self.0.requests.reserve().await.map_err(|_| Error::ConnectionClosed("reader stopped".into()))?;
                Ok::<_, Error>((permit, slot))
            };
            let (_permit, slot) = tokio::select! {
                result = admission => result?,
                _ = connection_closed.changed() => return Err(Error::ConnectionClosed("reader stopped".into())),
                _ = async {
                    if let Some(state) = &mut session_closed {
                        if !*state.borrow() { let _ = state.changed().await; }
                    } else { std::future::pending::<()>().await; }
                } => return Err(Error::SessionClosed(session_id.clone().unwrap_or_default())),
            };
            let (reply, response) = oneshot::channel();
            slot.send(Request { method: method.into(), params, session_id, reply });
            response
                .await
                .map_err(|_| Error::ConnectionClosed("reader stopped".into()))?
        })
        .await
        .map_err(|_| Error::Timeout(method.into()))?
    }

    /// Register synchronously so a subsequent command cannot race subscription.
    pub fn subscribe(&self) -> EventStream {
        EventStream {
            _sender: None,
            receiver: self.0.events.subscribe(),
            closed: self.0.closed.clone(),
        }
    }

    pub(crate) fn subscribe_typed(&self, method: &str, session: Option<String>) -> EventStream {
        let mut subscriptions = self
            .0
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        subscriptions.retain(|_, sender| sender.strong_count() > 0);
        let key = (session, method.to_owned());
        let sender = subscriptions
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| {
                let sender = Arc::new(broadcast::channel(CAPACITY).0);
                subscriptions.insert(key, Arc::downgrade(&sender));
                sender
            });
        EventStream {
            receiver: sender.subscribe(),
            closed: self.0.closed.clone(),
            _sender: Some(sender),
        }
    }

    pub async fn close(&self) {
        self.0.shutdown.send_replace(true);
        let mut closed = self.0.closed.clone();
        while closed.borrow().is_none() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Bounded stream of raw events. Lag is reported, never silently ignored.
pub struct EventStream {
    _sender: Option<Arc<broadcast::Sender<RawEvent>>>,
    receiver: broadcast::Receiver<RawEvent>,
    closed: watch::Receiver<Option<String>>,
}

impl EventStream {
    pub(crate) fn try_recv(&mut self) -> Result<Option<RawEvent>> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(broadcast::error::TryRecvError::Empty) => Ok(None),
            Err(broadcast::error::TryRecvError::Lagged(count)) => Err(Error::Lagged(count)),
            Err(broadcast::error::TryRecvError::Closed) => {
                Err(Error::ConnectionClosed("reader stopped".into()))
            }
        }
    }
    pub async fn recv(&mut self) -> Result<RawEvent> {
        loop {
            // Observe closure before draining, so no final event can race this check.
            let reason = self.closed.borrow().clone();
            match self.receiver.try_recv() {
                Ok(event) => return Ok(event),
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    return Err(Error::Lagged(count));
                }
                Err(broadcast::error::TryRecvError::Closed) => {
                    return Err(Error::ConnectionClosed("reader stopped".into()));
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
            }
            if let Some(reason) = reason {
                return Err(Error::ConnectionClosed(reason));
            }
            tokio::select! {
                result = self.closed.changed() => {
                    if result.is_err() { return Err(Error::ConnectionClosed("reader stopped".into())); }
                },
                event = self.receiver.recv() => return event.map_err(|error| match error {
                    broadcast::error::RecvError::Closed => Error::ConnectionClosed("reader stopped".into()),
                    broadcast::error::RecvError::Lagged(count) => Error::Lagged(count),
                }),
            }
        }
    }
}

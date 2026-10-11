use crate::{Error, Result, error::ProtocolError, protocol::Command};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::poll_fn,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

mod auth_events;
pub(crate) use auth_events::{AuthEvent, AuthEventKind, AuthEvents};

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
    runtime: tokio::runtime::Handle,
    requests: mpsc::Sender<Request>,
    events: broadcast::Sender<RawEvent>,
    shutdown: watch::Sender<bool>,
    closed: watch::Receiver<Option<String>>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    permits: Semaphore,
    sessions: SessionRegistry,
    subscriptions: Subscriptions,
    auth_subscriptions: auth_events::AuthSubscriptions,
}

struct SessionEntry {
    parent: Option<String>,
    target_id: Option<String>,
    closed: watch::Sender<bool>,
    auth_owner: Arc<std::sync::atomic::AtomicBool>,
    domains: Arc<[tokio::sync::Mutex<usize>; 2]>,
    detach: Arc<tokio::sync::Mutex<()>>,
}

type EventKey = (Option<String>, String);
type Subscriptions = Arc<Mutex<HashMap<EventKey, Vec<Weak<broadcast::Sender<RawEvent>>>>>>;

pub(crate) struct SessionState {
    pub(crate) closed: watch::Receiver<bool>,
    pub(crate) parent: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) auth_owner: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) domains: Arc<[tokio::sync::Mutex<usize>; 2]>,
    pub(crate) detach: Arc<tokio::sync::Mutex<()>>,
}

type SessionRegistry = Arc<Mutex<HashMap<String, SessionEntry>>>;

fn register_session(
    registry: &SessionRegistry,
    id: &str,
    parent: Option<String>,
    target_id: Option<&str>,
) {
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    let entry = registry.entry(id.into()).or_insert_with(|| SessionEntry {
        parent,
        target_id: None,
        closed: watch::channel(false).0,
        auth_owner: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        domains: Arc::default(),
        detach: Arc::default(),
    });
    // The first known identity is authoritative, even if a later report conflicts.
    if entry.target_id.is_none() {
        entry.target_id = target_id.map(str::to_owned);
    }
}

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

fn close_session_requests(
    registry: &SessionRegistry,
    pending: &mut HashMap<u64, Request>,
    id: &str,
) {
    let removed = close_sessions(registry, id);
    let cancelled: Vec<_> = pending
        .iter()
        .filter(|(_, request)| {
            request
                .session_id
                .as_ref()
                .is_some_and(|id| removed.contains(id))
        })
        .map(|(&id, _)| id)
        .collect();
    for id in cancelled {
        if let Some(request) = pending.remove(&id) {
            let _ = request.reply.send(Err(Error::SessionClosed(
                request.session_id.unwrap_or_default(),
            )));
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

/// A cloneable handle to one CDP socket. The reader task starts automatically.
#[derive(Clone)]
pub struct Connection(Arc<Inner>, Option<Duration>);

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
        let config = WebSocketConfig::default()
            .max_message_size(Some(100 * 1024 * 1024))
            .max_frame_size(Some(100 * 1024 * 1024));
        let (socket, _) = connect_async_with_config(url, Some(config), false).await?;
        let (requests, mut incoming) = mpsc::channel::<Request>(CAPACITY);
        let (events, _) = broadcast::channel(CAPACITY);
        let event_sender = events.clone();
        let (shutdown, mut stop) = watch::channel(false);
        let (closed_sender, closed) = watch::channel(None);
        let sessions = SessionRegistry::default();
        let active_sessions = sessions.clone();
        let subscriptions = Subscriptions::default();
        let active_subscriptions = subscriptions.clone();
        let auth_subscriptions = auth_events::AuthSubscriptions::default();
        let active_auth_subscriptions = auth_subscriptions.clone();
        let task = tokio::spawn(async move {
            let (mut sink, mut stream) = socket.split();
            let mut pending = HashMap::<u64, Request>::new();
            let mut id = 0u64;
            let mut outbound = None;
            let mut write_deadline = None;
            let mut cleanup = tokio::time::interval(Duration::from_secs(1));
            let outcome: Result<()> = async {
                loop {
                    tokio::select! {
                        _ = stop.changed() => break,
                        result = poll_fn(|cx| {
                            // All write progress lives in the sink and outbound slot,
                            // so selecting a read does not restart a partial send.
                            if outbound.is_some()
                                && !pending.get(&id).is_some_and(|request| !request.reply.is_closed())
                            {
                                outbound = None;
                            }
                            if outbound.is_some() {
                                std::task::ready!(sink.poll_ready_unpin(cx))?;
                                sink.start_send_unpin(outbound.take().expect("outbound message"))?;
                            }
                            sink.poll_flush_unpin(cx)
                        }), if write_deadline.is_some() => {
                            result?;
                            write_deadline = None;
                        }
                        _ = tokio::time::sleep_until(write_deadline.unwrap_or_else(tokio::time::Instant::now)),
                            if write_deadline.is_some() => {
                            return Err(Error::Timeout("writing to CDP socket".into()));
                        }
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
                                                let target_id = if request.method == "Target.attachToTarget" {
                                                    request.params["targetId"].as_str()
                                                } else {
                                                    None
                                                };
                                                register_session(&active_sessions, id, request.session_id.clone(), target_id);
                                            }
                                            if request.method == "Target.detachFromTarget"
                                                && result.is_ok()
                                                && let Some(id) = request.params["sessionId"].as_str()
                                            {
                                                close_session_requests(&active_sessions, &mut pending, id);
                                            }
                                            let _ = request.reply.send(result);
                                        }
                                    } else {
                                        let event: RawEvent = serde_json::from_value(value)?;
                                        if let Some(id) = event.params["sessionId"].as_str() {
                                            if event.method == "Target.attachedToTarget" {
                                                register_session(&active_sessions, id, event.session_id.clone(), event.params["targetInfo"]["targetId"].as_str());
                                            } else if event.method == "Target.detachedFromTarget" {
                                                close_session_requests(&active_sessions, &mut pending, id);
                                            }
                                        }
                                        // No subscribers is normal; subscribed readers detect lag explicitly.
                                        let key = (event.session_id.clone(), event.method.clone());
                                        if let Some(senders) = active_subscriptions.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                                            for sender in senders.iter().filter_map(Weak::upgrade) { let _ = sender.send(event.clone()); }
                                        }
                                        auth_events::dispatch(&active_auth_subscriptions, &event);
                                        let _ = event_sender.send(event);
                                    }
                                }
                                Message::Close(_) => break,
                                Message::Ping(_) => {
                                    // Tungstenite queues the pong while reading. Flush it
                                    // without suspending reads or extending an active write.
                                    write_deadline.get_or_insert_with(|| tokio::time::Instant::now() + TIMEOUT);
                                }
                                Message::Pong(_) => {}
                                _ => return Err(Error::ConnectionClosed("non-text CDP frame".into())),
                            }
                        }
                        request = incoming.recv(), if write_deadline.is_none() => {
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
                            outbound = Some(Message::Text(serde_json::to_string(&message)?.into()));
                            write_deadline = Some(tokio::time::Instant::now() + TIMEOUT);
                        }
                        _ = cleanup.tick() => pending.retain(|_, p| !p.reply.is_closed()),
                    }
                }
                Ok(())
            }.await;
            #[cfg(feature = "tracing")]
            tracing::debug!(target: "rustcdp", failed = outcome.is_err(), pending = pending.len(), "connection stopped");
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
        #[cfg(feature = "tracing")]
        tracing::debug!(target: "rustcdp", "connection established");
        Ok(Self(
            Arc::new(Inner {
                runtime: tokio::runtime::Handle::current(),
                requests,
                events,
                shutdown,
                closed,
                task: tokio::sync::Mutex::new(Some(task)),
                permits: Semaphore::new(CAPACITY),
                sessions,
                subscriptions,
                auth_subscriptions,
            }),
            Some(TIMEOUT),
        ))
    }

    /// Set this handle's command deadline, including queue admission. None waits
    /// until completion, session closure, or caller cancellation. Clones inherit it.
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.1 = timeout;
        self
    }

    pub fn is_closed(&self) -> bool {
        self.0.requests.is_closed() || *self.0.shutdown.borrow() || self.0.closed.borrow().is_some()
    }

    pub(crate) fn runtime(&self) -> tokio::runtime::Handle {
        self.0.runtime.clone()
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

    pub(crate) fn session_state(&self, id: &str) -> Result<SessionState> {
        self.0
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|entry| SessionState {
                closed: entry.closed.subscribe(),
                parent: entry.parent.clone(),
                target_id: entry.target_id.clone(),
                auth_owner: entry.auth_owner.clone(),
                domains: entry.domains.clone(),
                detach: entry.detach.clone(),
            })
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
            .map(|id| self.session_state(id).map(|state| state.closed))
            .transpose()?;
        #[cfg(feature = "tracing")]
        let mut diagnostic = crate::diagnostics::Command::new(method);
        let mut connection_closed = self.0.closed.clone();
        let execute = async {
            let admission = async {
                let permit = self
                    .0
                    .permits
                    .acquire()
                    .await
                    .map_err(|_| Error::ConnectionClosed("connection is closed".into()))?;
                let slot = self
                    .0
                    .requests
                    .reserve()
                    .await
                    .map_err(|_| Error::ConnectionClosed("reader stopped".into()))?;
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
            slot.send(Request {
                method: method.into(),
                params,
                session_id,
                reply,
            });
            response
                .await
                .map_err(|_| Error::ConnectionClosed("reader stopped".into()))?
        };
        let result = if let Some(timeout) = self.1 {
            tokio::time::timeout(timeout, execute)
                .await
                .unwrap_or_else(|_| Err(Error::Timeout(method.into())))
        } else {
            execute.await
        };
        #[cfg(feature = "tracing")]
        diagnostic.finish(&result);
        result
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
        self.subscribe_filtered(&[method], session, CAPACITY)
    }

    pub(crate) fn subscribe_filtered(
        &self,
        methods: &[&str],
        session: Option<String>,
        capacity: usize,
    ) -> EventStream {
        let mut subscriptions = self
            .0
            .subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        subscriptions.retain(|_, senders| {
            senders.retain(|sender| sender.strong_count() > 0);
            !senders.is_empty()
        });
        let sender = Arc::new(broadcast::channel(capacity).0);
        for method in methods {
            subscriptions
                .entry((session.clone(), (*method).to_owned()))
                .or_default()
                .push(Arc::downgrade(&sender));
        }
        EventStream {
            receiver: sender.subscribe(),
            closed: self.0.closed.clone(),
            _sender: Some(sender),
        }
    }

    /// Disconnect and await transport teardown. A canceled wait can be resumed.
    pub async fn close(&self) {
        self.0.shutdown.send_replace(true);
        let mut task = self.0.task.lock().await;
        if let Some(pending) = &mut *task {
            let _ = pending.await;
            *task = None;
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

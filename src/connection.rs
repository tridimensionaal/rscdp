use crate::{Error, Result, error::ProtocolError, protocol::Command};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
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
                                            let _ = request.reply.send(result);
                                        }
                                    } else {
                                        let event = serde_json::from_value(value)?;
                                        // No subscribers is normal; subscribed readers detect lag explicitly.
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
        tokio::time::timeout(TIMEOUT, async {
            let _permit = self
                .0
                .permits
                .acquire()
                .await
                .map_err(|_| Error::ConnectionClosed("connection is closed".into()))?;
            let (reply, response) = oneshot::channel();
            self.0
                .requests
                .send(Request {
                    method: method.into(),
                    params,
                    session_id,
                    reply,
                })
                .await
                .map_err(|_| Error::ConnectionClosed("reader stopped".into()))?;
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
            receiver: self.0.events.subscribe(),
            closed: self.0.closed.clone(),
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
    receiver: broadcast::Receiver<RawEvent>,
    closed: watch::Receiver<Option<String>>,
}

impl EventStream {
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

use super::Page;
use crate::{
    Error, Result,
    cdp::network::{
        self, RequestId,
        commands::GetResponseBody,
        events::{LoadingFailed, LoadingFinished, RequestWillBeSent, ResponseReceived},
    },
    connection::{EventOptions, EventStream, RawEvent},
    error::decode_ref,
    session::DomainGuard,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::time::Instant;

/// Configure an exact-URL response observation. Call `prepare().await` before
/// the action that may produce the response; constructing a builder does no I/O.
#[must_use = "call prepare().await before triggering the request"]
pub struct ResponseObserverBuilder<'a> {
    page: &'a Page,
    url: String,
    timeout: Duration,
}

/// An armed, exact-session observer. The first matching response wins, including
/// redirect headers. This does not prove which action caused the response.
#[must_use = "perform the action, then call wait().await"]
pub struct PreparedResponse {
    observation: Observation,
    url: String,
}

struct Observation {
    page: Page,
    events: EventStream,
    _network: DomainGuard,
    deadline: Instant,
}

/// A selected response and its still-active completion observation.
///
/// The response retains its Page, exact-session event stream and Network guard.
/// Dropping it releases only its share of managed Network reporting. Direct
/// Network enable/disable commands bypass that ownership and must not be mixed
/// with these observers. Metadata contains Chrome's `responseReceived` or
/// `redirectResponse` headers; separately reported extra-info headers are not merged.
#[must_use = "retain the response while inspecting its metadata"]
pub struct Response {
    observation: Observation,
    request_id: RequestId,
    metadata: network::Response,
    redirect: bool,
}

impl Page {
    /// Observe the first response whose reported URL exactly matches `url`.
    /// No network-idle or application-readiness guarantee is implied.
    pub fn expect_response(&self, url: impl Into<String>) -> ResponseObserverBuilder<'_> {
        ResponseObserverBuilder {
            page: self,
            url: url.into(),
            timeout: Duration::from_secs(30),
        }
    }
}

impl ResponseObserverBuilder<'_> {
    /// Set one budget for preparation, intervening action time, observation and
    /// body retrieval. Defaults to 30 seconds. Zero expires before setup.
    /// This observer cannot preempt an action awaited independently of it.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Subscribe before enabling Network. Caller cancellation releases an
    /// unclaimed domain guard through the usual bounded acquisition/cleanup.
    /// Domain transitions retain their independent cleanup/recovery bounds.
    pub async fn prepare(self) -> Result<PreparedResponse> {
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            Error::InvalidArgument("response timeout exceeds monotonic deadline range".into())
        })?;
        check_deadline(deadline)?;
        tokio::time::timeout_at(deadline, async {
            let events = self
                .page
                .session()
                .subscribe_with(EventOptions::default().methods([
                    "Network.requestWillBeSent",
                    "Network.responseReceived",
                    "Network.loadingFinished",
                    "Network.loadingFailed",
                ]))?;
            let network = self.page.session().enable_network().await?;
            check_deadline(deadline)?;
            Ok(PreparedResponse {
                observation: Observation {
                    page: self.page.clone(),
                    events,
                    _network: network,
                    deadline,
                },
                url: self.url,
            })
        })
        .await
        .map_err(|_| observation_timeout())?
    }
}

impl PreparedResponse {
    /// Wait for matching headers, retaining completion events for the selected
    /// request. Same-URL requests cannot replace an already selected response.
    /// Failures before matching headers do not identify a qualifying response;
    /// the observer can expire if no matching headers ever arrive.
    pub async fn wait(mut self) -> Result<Response> {
        loop {
            let event = self.observation.recv().await?;
            let selected = match event {
                NetworkEvent::Response(event) if event.response.url == self.url => {
                    Some((event.request_id, event.response, false))
                }
                NetworkEvent::Request(event) => {
                    let RequestWillBeSent {
                        request_id,
                        redirect_response,
                        ..
                    } = *event;
                    redirect_response
                        .filter(|response| response.url == self.url)
                        .map(|response| (request_id, response, true))
                }
                NetworkEvent::Finished(event) => {
                    let _ = event.request_id;
                    None
                }
                NetworkEvent::Failed(event) => {
                    let _ = event.request_id;
                    None
                }
                _ => None,
            };
            if let Some((request_id, metadata, redirect)) = selected {
                check_deadline(self.observation.deadline)?;
                return Ok(Response {
                    observation: self.observation,
                    request_id,
                    metadata,
                    redirect,
                });
            }
        }
    }
}

impl Response {
    /// Read the selected response after its load completes, using the remaining
    /// observation budget. This consumes the response; retain the returned bytes
    /// if they are needed more than once. HTTP error statuses can have valid bodies.
    ///
    /// Redirect bodies are unavailable. Chrome may also evict bodies or discard
    /// them on navigation; those protocol errors propagate without refetching.
    /// The event queue, Chrome's retention buffers and transport message limits
    /// impose different bounds. This method does not impose an allocation cap.
    /// Cancellation does not cancel the browser's network request.
    pub async fn body(mut self) -> Result<Vec<u8>> {
        check_deadline(self.observation.deadline)?;
        if self.redirect {
            return Err(Error::ResponseBody("redirect response".into()));
        }
        loop {
            match self.observation.recv().await? {
                NetworkEvent::Finished(event) if event.request_id == self.request_id => break,
                NetworkEvent::Failed(event) if event.request_id == self.request_id => {
                    return Err(Error::ResponseBody(event.error_text));
                }
                NetworkEvent::Request(event) if event.request_id == self.request_id => {
                    return Err(Error::ResponseBody(
                        "request ID was reused for another hop".into(),
                    ));
                }
                _ => {}
            }
        }
        check_deadline(self.observation.deadline)?;
        let response = tokio::time::timeout_at(
            self.observation.deadline,
            self.observation
                .page
                .session()
                .execute(&GetResponseBody::new(self.request_id)),
        )
        .await
        .map_err(|_| observation_timeout())??;
        let bytes = if response.base64_encoded {
            STANDARD.decode(response.body)?
        } else {
            response.body.into_bytes()
        };
        check_deadline(self.observation.deadline)?;
        Ok(bytes)
    }

    /// Read the body as strict UTF-8. Invalid bytes produce [`Error::Utf8`].
    pub async fn text(self) -> Result<String> {
        let deadline = self.observation.deadline;
        let text = String::from_utf8(self.body().await?)?;
        check_deadline(deadline)?;
        Ok(text)
    }

    /// Read and deserialize the completed JSON body, without refetching it.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        let deadline = self.observation.deadline;
        let value = serde_json::from_slice(&self.body().await?)?;
        check_deadline(deadline)?;
        Ok(value)
    }

    pub fn metadata(&self) -> &network::Response {
        &self.metadata
    }

    /// ID scoped to this response's session. Redirect hops reuse this ID; it
    /// alone is not a unique response identifier and does not guarantee a body.
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// Whether these headers describe a redirect hop, whose body is unavailable.
    pub fn is_redirect(&self) -> bool {
        self.redirect
    }

    /// The originating session, also retained by this response's observation.
    pub fn session(&self) -> &crate::Session {
        self.observation.page.session()
    }
}

impl Observation {
    async fn recv(&mut self) -> Result<NetworkEvent> {
        check_deadline(self.deadline)?;
        let raw = tokio::time::timeout_at(self.deadline, self.events.recv())
            .await
            .map_err(|_| observation_timeout())??;
        check_deadline(self.deadline)?;
        NetworkEvent::decode(raw)
    }
}

enum NetworkEvent {
    Request(Box<RequestWillBeSent>),
    Response(Box<ResponseReceived>),
    Finished(LoadingFinished),
    Failed(LoadingFailed),
}

impl NetworkEvent {
    fn decode(event: RawEvent) -> Result<Self> {
        match event.method.as_str() {
            "Network.requestWillBeSent" => Ok(Self::Request(Box::new(decode_ref(
                &event.method,
                &event.params,
            )?))),
            "Network.responseReceived" => Ok(Self::Response(Box::new(decode_ref(
                &event.method,
                &event.params,
            )?))),
            "Network.loadingFinished" => {
                Ok(Self::Finished(decode_ref(&event.method, &event.params)?))
            }
            "Network.loadingFailed" => Ok(Self::Failed(decode_ref(&event.method, &event.params)?)),
            _ => Err(Error::UnexpectedResponse {
                method: event.method,
                payload: event.params,
            }),
        }
    }
}

fn observation_timeout() -> Error {
    Error::Timeout("observing network response".into())
}
fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(observation_timeout())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cdp::target::TargetId, connection::ConnectionOptions, page::TargetKind};
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::Message;

    const URL: &str = "https://response.invalid/data";

    // A narrow protocol peer lets the observer see malformed events and a full
    // queue without depending on Chrome timing or corrupting a browser session.
    async fn peer_page(capacity: usize) -> (Page, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                if message.is_close() {
                    break;
                }
                let request: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.attachToTarget" => json!({"sessionId": "observer"}),
                    "Page.enable"
                    | "Runtime.enable"
                    | "Network.enable"
                    | "Page.disable"
                    | "Runtime.disable"
                    | "Network.disable"
                    | "Target.detachFromTarget" => json!({}),
                    "Test.emit" => {
                        for event in request["params"]["events"].as_array().unwrap() {
                            let mut event = event.clone();
                            event["sessionId"] = json!("observer");
                            socket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        json!({})
                    }
                    method => panic!("unexpected command after observation failure: {method}"),
                };
                // Receipt of this reply proves earlier events reached the actor;
                // no sleeps or timing assumptions are needed to overflow a queue.
                socket
                    .send(Message::Text(
                        json!({"id": request["id"], "result": result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let connection = ConnectionOptions::default()
            .event_capacity(capacity)
            .connect(&endpoint)
            .await
            .unwrap();
        let session = connection.attach(TargetId("tab".into())).await.unwrap();
        let page = Page::from_session(session, None, TargetKind::Tab)
            .await
            .unwrap();
        (page, peer)
    }

    async fn emit(page: &Page, events: Value) {
        page.session()
            .connection()
            .execute_raw("Test.emit", json!({"events": events}))
            .await
            .unwrap();
    }

    async fn select_headers(page: &Page, prepared: PreparedResponse) -> Response {
        emit(
            page,
            json!([{
                "method": "Network.responseReceived",
                "params": {
                    "requestId": "selected", "loaderId": "loader", "timestamp": 1,
                    "type": "Fetch", "hasExtraInfo": false,
                    "response": {
                        "url": URL, "status": 200, "statusText": "OK", "headers": {},
                        "mimeType": "text/plain", "charset": "utf-8",
                        "connectionReused": false, "connectionId": 1,
                        "encodedDataLength": 0, "securityState": "secure"
                    }
                }
            }]),
        )
        .await;
        prepared.wait().await.unwrap()
    }

    #[tokio::test]
    async fn malformed_events_fail_before_and_after_response_selection() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for after_headers in [false, true] {
                for method in ["Network.responseReceived", "Network.requestWillBeSent",
                    "Network.loadingFinished", "Network.loadingFailed"] {
                    let (page, peer) = peer_page(8).await;
                    let prepared = page.expect_response(URL).prepare().await.unwrap();
                    let events = json!([{"method": method, "params": {}}]);
                    let outcome = if after_headers {
                        let response = select_headers(&page, prepared).await;
                        emit(&page, events).await;
                        response.body().await.map(|_| ())
                    } else {
                        emit(&page, events).await;
                        prepared.wait().await.map(|_| ())
                    };
                    assert!(matches!(outcome, Err(Error::Decode { method: actual, .. }) if actual == method));
                    page.session().connection().close().await;
                    peer.await.unwrap();
                }
            }
        }).await.expect("malformed events must fail promptly");
    }

    #[tokio::test]
    async fn queue_overflow_fails_before_and_after_response_selection() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for after_headers in [false, true] {
                let (page, peer) = peer_page(1).await;
                let prepared = page.expect_response(URL).prepare().await.unwrap();
                let events = json!([
                    {"method": "Network.loadingFinished", "params": {
                        "requestId": "unrelated", "timestamp": 2, "encodedDataLength": 0}},
                    {"method": "Network.loadingFinished", "params": {
                        "requestId": "selected", "timestamp": 3, "encodedDataLength": 0}}
                ]);
                let outcome = if after_headers {
                    let response = select_headers(&page, prepared).await;
                    emit(&page, events).await;
                    response.body().await.map(|_| ())
                } else {
                    emit(&page, events).await;
                    prepared.wait().await.map(|_| ())
                };
                assert!(matches!(outcome, Err(Error::Lagged(1))));
                page.session().connection().close().await;
                peer.await.unwrap();
            }
        })
        .await
        .expect("queue overflow must fail promptly");
    }
}

use super::Page;
use crate::{
    Error, Result,
    cdp::network::{
        self, RequestId,
        events::{LoadingFailed, LoadingFinished, RequestWillBeSent, ResponseReceived},
    },
    connection::{EventOptions, EventStream, RawEvent},
    error::decode_ref,
    session::DomainGuard,
};
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
/// with these observers. Metadata describes Chrome's responseReceived headers;
/// it does not merge the separately reported extra-info headers.
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

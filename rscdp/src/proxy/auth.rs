//! Single-flight authentication for a session already configured to use a proxy.
//!
//! Only challenges from the configured proxy origin receive credentials.
//! Rejection stays latched until teardown; Fetch stays enabled for later challenges.
//! Authentication buffering and held requests/challenges are bounded. Overflow
//! fails authentication explicitly. Interception failures trigger a target-close
//! attempt, then Fetch disable if needed. If both fail while the session remains
//! open, the connection is closed.
use crate::{
    Error, Result, Session,
    connection::{AuthEvent, AuthEventKind, AuthEvents},
    error::with_cleanup,
    proxy::Proxy,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::{
    sync::{Mutex, oneshot, watch},
    task::JoinHandle,
};

const MAX_PENDING: usize = 256;
const MAX_TRACKED: usize = 4096;
const TRACKED_TEXT_BUDGET: usize = 4 * 1024 * 1024;
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAuthStatus {
    Pending,
    /// A response was observed without verifying the supplied credentials.
    Unchallenged,
    /// A credential-bearing probe received a response without proxy rejection or
    /// a reported network error; later challenges start a new probe.
    Authenticated,
    Rejected,
    Stopped,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Running,
    Shutdown,
}

/// An owned authentication worker for one session.
///
/// Credentials are sent only for challenges matching the configured proxy's
/// scheme, host, and port; mismatches are canceled. The cold request burst is
/// serialized and credential rejection stays latched. Fetch remains enabled after
/// verification to handle later challenges, including expired cached credentials.
/// Each new credential-bearing probe resets readiness to Pending. Network failures
/// do not establish credential rejection; they leave that probe unverified.
///
/// The event queue holds at most 4,096 compact events and 4 MiB of request-ID/origin
/// text, without headers or bodies. Held requests and challenges are each capped
/// at 256; excess work is aborted or canceled. Queue overflow fails authentication
/// and triggers recovery rather than silently losing paused requests. Outstanding
/// Fetch/Network correlations have a separate 4,096-entry, 4 MiB ID-text limit,
/// including IDs retained by the worker's active and queued requests.
/// See the module documentation for recovery behavior.
#[must_use = "retain this worker while authentication is needed; await stop() to finish cleanup"]
pub struct ProxyAuth {
    stop: watch::Sender<Stop>,
    task: Mutex<Option<JoinHandle<Result<()>>>>,
    status: watch::Receiver<ProxyAuthStatus>,
}

impl ProxyAuth {
    /// Arm before navigation. Only one worker may own a session at a time.
    /// First acquires shared Network reporting under [`crate::session::DomainGuard`]'s
    /// independent lock and transition bounds, then enables Fetch within 30 seconds.
    /// Recovery can extend setup beyond these bounds, even after caller cancellation.
    /// Interception covers only this session, not other tabs or separate child
    /// sessions. This does not configure proxy routing or navigate the page.
    /// Retain the returned worker; dropping it requests shutdown.
    pub async fn start(session: Session, proxy: Proxy) -> Result<Self> {
        if !proxy.requires_authentication() {
            return Err(Error::InvalidArgument("proxy has no credentials".into()));
        }
        let ownership = session.claim_proxy_auth()?;
        let mut events = session.connection().subscribe_auth(&session.id().0);
        let mut failure = events.failure();
        let (stop, mut stopped) = watch::channel(Stop::Running);
        let (status_sender, status) = watch::channel(ProxyAuthStatus::Pending);
        let (ready, started) = oneshot::channel();
        let task = session.connection().runtime().spawn(async move {
            let _ownership = ownership;
            // Do not cancel this nested owner: await its bounded acquisition and
            // recovery so setup/stop cannot leave an unclaimed domain transition.
            let network = match session.enable_network().await {
                Ok(network) => network,
                Err(error) => {
                    status_sender.send_replace(ProxyAuthStatus::Failed(error.to_string()));
                    let _ = ready.send(Err(error));
                    return Ok(());
                }
            };
            let outcome = async {
                if *stopped.borrow() != Stop::Running {
                    let _ = ready.send(Err(Error::ProxyAuthentication(
                        "authentication stopped during setup".into(),
                    )));
                    status_sender.send_replace(ProxyAuthStatus::Stopped);
                    return Ok(());
                }
                let enable = session.execute_raw(
                    "Fetch.enable",
                    json!({
                        "patterns": [
                            {"urlPattern": "*", "requestStage": "Request"},
                            {"urlPattern": "*", "requestStage": "Response"}
                        ],
                        "handleAuthRequests": true
                    }),
                );
                let enabled = tokio::select! {
                    result = tokio::time::timeout(SETUP_TIMEOUT, enable) => result
                        .map_err(|_| Error::Timeout("enabling proxy interception".into()))
                        .and_then(|result| result),
                    _ = failure.changed() => Err(event_failure(&failure)),
                    _ = stopped.changed() => Err(Error::ProxyAuthentication(
                        "authentication stopped during setup".into(),
                    )),
                };
                if let Err(error) = enabled {
                    status_sender.send_replace(ProxyAuthStatus::Failed(error.to_string()));
                    let _ = ready.send(Err(error));
                    recover_interception(&session).await;
                    return Ok(());
                }
                let _ = ready.send(Ok(()));
                let mut handler = Handler {
                    session: session.clone(),
                    proxy,
                    status: status_sender.clone(),
                    attempted: HashSet::new(),
                    network_requests: HashMap::new(),
                    challenges: VecDeque::new(),
                    requests: VecDeque::new(),
                    cold: None,
                    probe: None,
                    gate_open: false,
                    verified: false,
                    rejected: false,
                };
                let outcome = if *stopped.borrow() != Stop::Running {
                    Ok(())
                } else {
                    tokio::select! {
                        result = handler.run(&mut events) => result,
                        _ = failure.changed() => Err(event_failure(&failure)),
                        _ = stopped.changed() => Ok(()),
                        _ = session.closed() => Ok(()),
                    }
                };
                if session.is_closed() {
                    status_sender.send_replace(ProxyAuthStatus::Stopped);
                    return Ok(());
                }
                if let Err(error) = outcome {
                    status_sender.send_replace(ProxyAuthStatus::Failed(error.to_string()));
                    recover_interception(&session).await;
                    return Err(error);
                }
                let _ = tokio::time::timeout(DRAIN_TIMEOUT, handler.drain()).await;
                let disabled = tokio::time::timeout(
                    CLEANUP_TIMEOUT,
                    session.execute_raw("Fetch.disable", json!({})),
                )
                .await
                .map_err(|_| Error::Timeout("disabling proxy interception".into()))
                .and_then(|result| result)
                .map(|_| ());
                let outcome = if failure.borrow().is_some() {
                    Err(event_failure(&failure))
                } else {
                    disabled
                };
                match outcome {
                    Ok(()) => {
                        status_sender.send_replace(ProxyAuthStatus::Stopped);
                        Ok(())
                    }
                    Err(error) => {
                        status_sender.send_replace(ProxyAuthStatus::Failed(error.to_string()));
                        // A protocol failure must not leave requests stranded.
                        recover_interception(&session).await;
                        Err(error)
                    }
                }
            }
            .await;
            // Fetch recovery must finish before releasing Network reporting.
            let outcome = with_cleanup(outcome, network.close().await);
            if let Err(error) = &outcome {
                status_sender.send_replace(ProxyAuthStatus::Failed(error.to_string()));
            }
            outcome
        });
        let auth = Self {
            stop,
            task: Mutex::new(Some(task)),
            status,
        };
        let started = started
            .await
            .map_err(|_| {
                Error::ConnectionClosed("authentication worker stopped during setup".into())
            })
            .and_then(|result| result);
        match started {
            Ok(()) => Ok(auth),
            // Finish worker recovery before the owning browser/context can
            // dispose resources after a setup failure.
            Err(error) => crate::error::with_cleanup(Err(error), auth.stop().await),
        }
    }

    pub fn status(&self) -> ProxyAuthStatus {
        self.status.borrow().clone()
    }

    /// Wait for a response without observed proxy rejection or network error.
    /// Returns the observed `Authenticated` or `Unchallenged` status.
    /// Cached credentials, bypasses, or a proxy without authentication can yield
    /// an unchallenged response; readiness proves neither
    /// proxy traversal nor use of the supplied credentials.
    /// Readiness does not imply HTTP success or body completion, or guarantee
    /// future requests. A later credential-bearing probe resets the status to Pending.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<ProxyAuthStatus> {
        let mut status = self.status.clone();
        tokio::time::timeout(timeout, async {
            loop {
                match status.borrow().clone() {
                    ready @ (ProxyAuthStatus::Unchallenged | ProxyAuthStatus::Authenticated) => {
                        return Ok(ready);
                    }
                    ProxyAuthStatus::Rejected => {
                        return Err(Error::ProxyAuthentication("credentials rejected".into()));
                    }
                    ProxyAuthStatus::Failed(error) => {
                        return Err(Error::ProxyAuthentication(error));
                    }
                    ProxyAuthStatus::Stopped => {
                        return Err(Error::ProxyAuthentication("authentication stopped".into()));
                    }
                    ProxyAuthStatus::Pending => {}
                }
                status.changed().await.map_err(|_| {
                    Error::ProxyAuthentication("authentication worker stopped".into())
                })?;
            }
        })
        .await
        .map_err(|_| Error::Timeout("proxy readiness".into()))?
    }

    /// Stop the worker and await cleanup. Fetch disable has a one-second limit
    /// before recovery, even when ordinary command timeouts are disabled. Shared
    /// Network reporting is released afterward under [`crate::session::DomainGuard`]'s
    /// independent cleanup bounds.
    /// Canceling this wait leaves cleanup running; another call can await it.
    /// Concurrent calls serialize; only the call that joins reports a worker error.
    pub async fn stop(&self) -> Result<()> {
        self.stop.send_replace(Stop::Shutdown);
        let mut pending = self.task.lock().await;
        if let Some(task) = &mut *pending {
            let result = task.await;
            *pending = None;
            result??;
        }
        Ok(())
    }
}

impl Drop for ProxyAuth {
    fn drop(&mut self) {
        self.stop.send_replace(Stop::Shutdown);
        // Dropping the handle detaches the owned worker so recovery can finish.
        drop(self.task.get_mut().take());
    }
}

struct Handler {
    session: Session,
    proxy: Proxy,
    status: watch::Sender<ProxyAuthStatus>,
    attempted: HashSet<String>,
    network_requests: HashMap<String, String>,
    challenges: VecDeque<String>,
    requests: VecDeque<String>,
    cold: Option<String>,
    probe: Option<String>,
    gate_open: bool,
    verified: bool,
    rejected: bool,
}

impl Handler {
    async fn run(&mut self, events: &mut AuthEvents) -> Result<()> {
        loop {
            let event = events.recv().await?;
            self.event(event).await?;
        }
    }

    async fn event(&mut self, event: AuthEvent) -> Result<()> {
        let AuthEvent {
            id,
            network_id,
            kind,
        } = event;
        let (rejected, failed) = match kind {
            AuthEventKind::NetworkFailure => {
                if let Some(fetch_id) = self.network_requests.remove(&id) {
                    self.retire(&fetch_id);
                    self.advance().await?;
                }
                return Ok(());
            }
            AuthEventKind::Challenge { is_proxy, origin } => {
                self.challenge(id, is_proxy, origin.as_deref()).await?;
                return Ok(());
            }
            AuthEventKind::Request => {
                if let Some(network_id) = network_id {
                    self.track(&id, network_id)?;
                }
                if !self.gate_open && !self.rejected {
                    self.require_correlation(&id)?;
                }
                self.request(id).await?;
                return Ok(());
            }
            AuthEventKind::Response { rejected, failed } => (rejected, failed),
        };
        self.continue_request(&id).await?;
        let attempted = self.probe.as_ref() == Some(&id);
        let cold = self.cold.as_ref() == Some(&id);
        self.retire(&id);
        if !attempted && !cold {
            if self.gate_open
                && self.probe.is_none()
                && !self.rejected
                && !self.verified
                && !rejected
                && !failed
            {
                self.status.send_replace(ProxyAuthStatus::Unchallenged);
            }
            return Ok(());
        }
        if rejected {
            self.reject().await?;
            return Ok(());
        }
        if !failed {
            self.gate_open = true;
            if attempted {
                self.verified = true;
                self.status.send_replace(ProxyAuthStatus::Authenticated);
            } else if self.probe.is_none() && !self.verified {
                // A bypassed request or cached credentials do not verify this proxy.
                self.status.send_replace(ProxyAuthStatus::Unchallenged);
            }
        }
        // Failure is inconclusive; retire the slot without rejecting credentials.
        self.advance().await
    }

    fn retire(&mut self, id: &str) {
        self.network_requests.retain(|_, fetch_id| fetch_id != id);
        self.attempted.remove(id);
        self.requests.retain(|pending| pending != id);
        self.challenges.retain(|pending| pending != id);
        if self.cold.as_deref() == Some(id) {
            self.cold = None;
        }
        if self.probe.as_deref() == Some(id) {
            self.probe = None;
        }
    }

    fn require_correlation(&self, id: &str) -> Result<()> {
        if self
            .network_requests
            .values()
            .any(|fetch_id| fetch_id == id)
        {
            Ok(())
        } else {
            Err(Error::ProxyAuthentication(
                "active authentication request has no Network identity".into(),
            ))
        }
    }

    fn check_tracking_space(&self, additional: usize) -> Result<()> {
        // Include every retained string copy, not just map keys and values.
        let text = self
            .network_requests
            .iter()
            .map(|(network, fetch)| network.len() + fetch.len())
            .chain(self.attempted.iter().map(String::len))
            .chain(self.requests.iter().map(String::len))
            .chain(self.challenges.iter().map(String::len))
            .chain(self.cold.iter().map(String::len))
            .chain(self.probe.iter().map(String::len))
            .fold(additional, usize::saturating_add);
        if text > TRACKED_TEXT_BUDGET {
            return Err(Error::ProxyAuthentication(
                "authentication request tracking exceeded its text capacity".into(),
            ));
        }
        Ok(())
    }

    fn track(&mut self, id: &str, network: String) -> Result<()> {
        // A redirect may reuse a Network ID. Later predecessor retirement only
        // removes mappings whose Fetch identity still matches that predecessor.
        self.network_requests
            .retain(|key, fetch| key != &network && fetch != id);
        if self.network_requests.len() >= MAX_TRACKED {
            return Err(Error::ProxyAuthentication(
                "authentication request tracking exceeded its capacity".into(),
            ));
        }
        self.check_tracking_space(network.len().saturating_add(id.len()))?;
        self.network_requests.insert(network, id.into());
        Ok(())
    }

    async fn advance(&mut self) -> Result<()> {
        if self.rejected {
            return Ok(());
        }
        if self.probe.is_none()
            && let Some(id) = self.challenges.pop_front()
        {
            self.provide(id).await?;
        }
        if self.probe.is_none() {
            if self.gate_open {
                while let Some(id) = self.requests.pop_front() {
                    self.continue_request(&id).await?;
                }
            } else if self.cold.is_none() {
                self.cold = self.requests.pop_front();
                if let Some(id) = &self.cold {
                    self.continue_request(id).await?;
                }
            }
        }
        Ok(())
    }

    async fn request(&mut self, id: String) -> Result<()> {
        if self.rejected {
            self.fail(&id).await?;
        } else if self.gate_open || self.cold.as_ref() == Some(&id) {
            self.continue_request(&id).await?;
        } else if self.cold.is_none() {
            self.check_tracking_space(id.len())?;
            self.cold = Some(id.clone());
            self.continue_request(&id).await?;
        } else if self.requests.len() >= MAX_PENDING {
            self.fail(&id).await?;
        } else if !self.requests.contains(&id) {
            self.check_tracking_space(id.len())?;
            self.requests.push_back(id);
        }
        Ok(())
    }

    async fn challenge(&mut self, id: String, is_proxy: bool, origin: Option<&str>) -> Result<()> {
        if !is_proxy {
            return self.respond(&id, "Default").await;
        }
        if self.rejected {
            return self.respond(&id, "CancelAuth").await;
        }
        if !origin.is_some_and(|origin| self.proxy.matches_origin(origin)) {
            self.respond(&id, "CancelAuth").await?;
            return Err(Error::ProxyAuthentication(
                "challenge origin does not match the configured proxy".into(),
            ));
        }
        self.require_correlation(&id)?;
        if self.attempted.contains(&id) {
            self.respond(&id, "CancelAuth").await?;
            return self.reject().await;
        }
        if self.probe.is_some() {
            if self.challenges.len() >= MAX_PENDING {
                self.respond(&id, "CancelAuth").await?;
            } else if !self.challenges.contains(&id) {
                self.check_tracking_space(id.len())?;
                self.challenges.push_back(id);
            }
            return Ok(());
        }
        self.provide(id).await
    }

    async fn provide(&mut self, id: String) -> Result<()> {
        let (username, password) = self
            .proxy
            .credentials()
            .ok_or_else(|| Error::ProxyAuthentication("missing credentials".into()))?;
        self.check_tracking_space(id.len().saturating_mul(2))?;
        self.verified = false;
        self.status.send_replace(ProxyAuthStatus::Pending);
        self.probe = Some(id.clone());
        self.attempted.insert(id.clone());
        self.session.execute_raw("Fetch.continueWithAuth", json!({"requestId":id,"authChallengeResponse":{"response":"ProvideCredentials","username":username,"password":password}})).await?;
        Ok(())
    }

    async fn respond(&self, id: &str, response: &str) -> Result<()> {
        self.session
            .execute_raw(
                "Fetch.continueWithAuth",
                json!({"requestId":id,"authChallengeResponse":{"response":response}}),
            )
            .await?;
        Ok(())
    }

    async fn continue_request(&self, id: &str) -> Result<()> {
        self.session
            .execute_raw("Fetch.continueRequest", json!({"requestId":id}))
            .await?;
        Ok(())
    }

    async fn fail(&self, id: &str) -> Result<()> {
        self.session
            .execute_raw(
                "Fetch.failRequest",
                json!({"requestId":id,"errorReason":"Aborted"}),
            )
            .await?;
        Ok(())
    }

    async fn reject(&mut self) -> Result<()> {
        self.rejected = true;
        self.verified = false;
        self.probe = None;
        self.cold = None;
        self.status.send_replace(ProxyAuthStatus::Rejected);
        self.drain().await
    }

    async fn drain(&mut self) -> Result<()> {
        while let Some(id) = self.challenges.pop_front() {
            self.respond(&id, "CancelAuth").await?;
        }
        while let Some(id) = self.requests.pop_front() {
            self.fail(&id).await?;
        }
        Ok(())
    }
}

fn event_failure(failure: &watch::Receiver<Option<&'static str>>) -> Error {
    Error::ProxyAuthentication(
        failure
            .borrow()
            .unwrap_or("authentication event channel stopped")
            .into(),
    )
}

async fn recover_interception(session: &Session) {
    if session.is_closed() {
        return;
    }
    let closed = tokio::time::timeout(
        Duration::from_secs(1),
        session.connection().execute_raw(
            "Target.closeTarget",
            json!({"targetId":session.target_id()}),
        ),
    )
    .await;
    if matches!(closed, Ok(Ok(ref value)) if value["success"] == true) || session.is_closed() {
        return;
    }
    if matches!(
        tokio::time::timeout(
            DRAIN_TIMEOUT,
            session.execute_raw("Fetch.disable", json!({}))
        )
        .await,
        Ok(Ok(_))
    ) || session.is_closed()
    {
        return;
    }
    // Last resort: detach the failed session and dispose disposeOnDetach contexts.
    session.connection().close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canceled_stop_preserves_worker_and_shared_stops_report_error_once() {
        let (stop, stopped) = watch::channel(Stop::Running);
        let (_status, status) = watch::channel(ProxyAuthStatus::Pending);
        let (finish, cleanup) = oneshot::channel();
        let auth = ProxyAuth {
            stop,
            task: Mutex::new(Some(tokio::spawn(async move {
                cleanup.await.unwrap();
                Err(Error::ProxyAuthentication("worker failed".into()))
            }))),
            status,
        };
        tokio::select! {
            biased;
            _ = auth.stop() => panic!("cleanup has not finished"),
            _ = std::future::ready(()) => {},
        }
        assert!(*stopped.borrow() == Stop::Shutdown);
        assert!(auth.task.lock().await.is_some());
        finish.send(()).unwrap();
        let (first, second) = tokio::join!(auth.stop(), auth.stop());
        assert_eq!(
            usize::from(first.is_err()) + usize::from(second.is_err()),
            1
        );
        let error = first.err().or_else(|| second.err()).unwrap();
        assert!(matches!(error, Error::ProxyAuthentication(message) if message == "worker failed"));
        auth.stop().await.unwrap();
    }

    #[tokio::test]
    async fn dropping_auth_signals_shutdown_and_allows_worker_cleanup() {
        let (stop, mut stopped) = watch::channel(Stop::Running);
        let (_status, status) = watch::channel(ProxyAuthStatus::Pending);
        let (finished, cleanup) = oneshot::channel();
        let auth = ProxyAuth {
            stop,
            task: Mutex::new(Some(tokio::spawn(async move {
                while *stopped.borrow_and_update() == Stop::Running {
                    stopped.changed().await.unwrap();
                }
                finished.send(()).unwrap();
                Ok(())
            }))),
            status,
        };
        drop(auth);
        tokio::time::timeout(Duration::from_secs(1), cleanup)
            .await
            .expect("detached worker must finish cleanup")
            .unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod state_tests;

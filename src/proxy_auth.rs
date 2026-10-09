//! Single-flight authentication for a session already configured to use a proxy.
//!
//! Only challenges from the configured proxy origin receive credentials.
//! Rejection stays latched until teardown; successful verification disables Fetch. Authentication events are
//! lossless; held requests/challenges are bounded. Fatal worker failures close the target;
//! if closing it and disabling Fetch both fail, the connection is closed.
use crate::{
    Error, Result, Session,
    connection::{AuthEvents, RawEvent},
    proxy::Proxy,
};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

const MAX_PENDING: usize = 256;
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAuthStatus {
    Pending,
    /// A request completed without a proxy challenge; credentials remain unverified.
    Unchallenged,
    Authenticated,
    Rejected,
    Stopped,
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Running,
    Shutdown,
    Detach,
}

pub struct ProxyAuth {
    stop: watch::Sender<Stop>,
    task: Option<JoinHandle<Result<()>>>,
    status: watch::Receiver<ProxyAuthStatus>,
}

impl ProxyAuth {
    /// Arm before navigation. Only one worker may own a session at a time.
    pub async fn start(session: Session, proxy: Proxy) -> Result<Self> {
        if !proxy.requires_authentication() {
            return Err(Error::InvalidArgument("proxy has no credentials".into()));
        }
        let ownership = session.claim_proxy_auth()?;
        let events = session.connection().subscribe_auth(&session.id().0);
        let (stop, mut stopped) = watch::channel(Stop::Running);
        let (status_sender, status) = watch::channel(ProxyAuthStatus::Pending);
        let (ready, started) = oneshot::channel();
        let task = session.connection().runtime().spawn(async move {
            let _ownership = ownership;
            let enabled = session.execute_raw("Fetch.enable", json!({"patterns":[{"urlPattern":"*","requestStage":"Request"},{"urlPattern":"*","requestStage":"Response"}],"handleAuthRequests":true})).await;
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
                challenges: VecDeque::new(),
                requests: VecDeque::new(),
                cold: None,
                probe: None,
                gate_open: false,
                verified: false,
                rejected: false,
            };
            let outcome = if *stopped.borrow() != Stop::Running {
                Ok(false)
            } else {
                tokio::select! {
                    result = handler.run(events) => result,
                    _ = stopped.changed() => Ok(false),
                    _ = session.closed() => Ok(false),
                }
            };
            if *stopped.borrow() == Stop::Detach {
                return Ok(());
            }
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
            let disabled = session
                .execute_raw("Fetch.disable", json!({}))
                .await
                .map(|_| ());
            let outcome = outcome.and_then(|authenticated| disabled.map(|_| authenticated));
            match outcome {
                Ok(true) => {
                    status_sender.send_replace(ProxyAuthStatus::Authenticated);
                    Ok(())
                }
                Ok(false) => {
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
        });
        let auth = Self {
            stop,
            task: Some(task),
            status,
        };
        started.await.map_err(|_| {
            Error::ConnectionClosed("authentication worker stopped during setup".into())
        })??;
        Ok(auth)
    }

    pub fn status(&self) -> ProxyAuthStatus {
        self.status.borrow().clone()
    }

    /// Wait for a successful request, with or without an observed proxy challenge.
    /// Inspect `status()` to distinguish verified authentication from unchallenged traffic.
    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let mut status = self.status.clone();
        tokio::time::timeout(timeout, async {
            loop {
                match status.borrow().clone() {
                    ProxyAuthStatus::Unchallenged | ProxyAuthStatus::Authenticated => return Ok(()),
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

    pub async fn stop(&mut self) -> Result<()> {
        self.stop.send_replace(Stop::Shutdown);
        if let Some(task) = &mut self.task {
            let result = task.await;
            self.task = None;
            result??;
        }
        Ok(())
    }

    /// Skip Fetch cleanup. Use only immediately before disposing the target/context.
    pub fn detach(&mut self) {
        self.stop.send_replace(Stop::Detach);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for ProxyAuth {
    fn drop(&mut self) {
        self.stop.send_replace(Stop::Shutdown);
    }
}

struct Handler {
    session: Session,
    proxy: Proxy,
    status: watch::Sender<ProxyAuthStatus>,
    attempted: HashSet<String>,
    challenges: VecDeque<String>,
    requests: VecDeque<String>,
    cold: Option<String>,
    probe: Option<String>,
    gate_open: bool,
    verified: bool,
    rejected: bool,
}

impl Handler {
    async fn run(&mut self, mut events: AuthEvents) -> Result<bool> {
        loop {
            let event = events.recv().await?;
            if self.event(event).await? {
                return Ok(true);
            }
        }
    }

    async fn event(&mut self, event: RawEvent) -> Result<bool> {
        let value = event.params;
        let id: String = serde_json::from_value(value["requestId"].clone())?;
        if event.method == "Fetch.authRequired" {
            self.challenge(id, value).await?;
            return Ok(false);
        }
        let response =
            value.get("responseStatusCode").is_some() || value.get("responseErrorReason").is_some();
        if !response {
            self.request(id).await?;
            return Ok(false);
        }
        self.continue_request(&id).await?;
        let attempted = self.probe.as_ref() == Some(&id);
        let cold = self.cold.as_ref() == Some(&id);
        if attempted {
            self.probe = None;
            self.attempted.remove(&id);
        }
        if cold {
            self.cold = None;
        }
        if !attempted && !cold {
            if self.gate_open
                && self.probe.is_none()
                && !self.rejected
                && !self.verified
                && value["responseStatusCode"] != 407
                && value.get("responseErrorReason").is_none()
            {
                self.status.send_replace(ProxyAuthStatus::Unchallenged);
            }
            return Ok(false);
        }
        if value["responseStatusCode"] == 407 {
            self.reject().await?;
            return Ok(false);
        }
        if value.get("responseErrorReason").is_none() {
            self.gate_open = true;
            if attempted {
                self.verified = true;
            } else if self.probe.is_none() && !self.verified {
                // A bypassed request or cached credentials do not verify this proxy.
                self.status.send_replace(ProxyAuthStatus::Unchallenged);
            }
        }
        // A network failure is inconclusive: retire this probe and let another
        // request try, instead of latching an incorrect credential rejection.
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
            return Ok(self.verified);
        }
        Ok(false)
    }

    async fn request(&mut self, id: String) -> Result<()> {
        if self.rejected {
            self.fail(&id).await?;
        } else if self.gate_open || self.cold.as_ref() == Some(&id) {
            self.continue_request(&id).await?;
        } else if self.cold.is_none() {
            self.cold = Some(id.clone());
            self.continue_request(&id).await?;
        } else if self.requests.len() >= MAX_PENDING {
            self.fail(&id).await?;
        } else if !self.requests.contains(&id) {
            self.requests.push_back(id);
        }
        Ok(())
    }

    async fn challenge(&mut self, id: String, value: Value) -> Result<()> {
        if value["authChallenge"]["source"] != "Proxy" {
            return self.respond(&id, "Default").await;
        }
        if self.rejected {
            return self.respond(&id, "CancelAuth").await;
        }
        if !value["authChallenge"]["origin"]
            .as_str()
            .is_some_and(|origin| self.proxy.matches_origin(origin))
        {
            self.respond(&id, "CancelAuth").await?;
            return Err(Error::ProxyAuthentication(
                "challenge origin does not match the configured proxy".into(),
            ));
        }
        if self.attempted.contains(&id) {
            self.respond(&id, "CancelAuth").await?;
            return self.reject().await;
        }
        if self.probe.is_some() {
            if self.challenges.len() >= MAX_PENDING {
                self.respond(&id, "CancelAuth").await?;
            } else if !self.challenges.contains(&id) {
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

use crate::{
    Connection, Error, Result,
    cdp::target::{SessionId, TargetId},
    connection::EventStream,
    protocol::{Command, Event},
};
use serde_json::{Value, json};
use std::marker::PhantomData;
use tokio::sync::watch;

mod domain;
pub use domain::DomainGuard;

/// A flattened CDP session multiplexed over its parent connection.
#[derive(Clone)]
pub struct Session {
    connection: Connection,
    id: SessionId,
    target: TargetId,
    closed: watch::Receiver<bool>,
    parent: Option<String>,
    auth_owner: std::sync::Arc<std::sync::atomic::AtomicBool>,
    domains: std::sync::Arc<[tokio::sync::Mutex<usize>; 2]>,
    detach: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Connection {
    pub async fn attach(&self, target: TargetId) -> Result<Session> {
        let response = self
            .execute(&crate::cdp::target::commands::AttachToTarget {
                target_id: target.clone(),
                flatten: Some(true),
            })
            .await?;
        self.session(response.session_id, target)
    }

    /// Adopt a session reported by `Target.attachedToTarget` (auto-attach).
    ///
    /// The supplied target must match the session's known target identity.
    /// If the identity is unknown (for example, after `Target.attachToBrowserTarget`),
    /// the caller is responsible for supplying the correct target. The supplied
    /// identity is retained on this handle but does not establish the registry's
    /// identity; a later attach event can establish it for future adoptions.
    pub fn session(&self, id: SessionId, target: TargetId) -> Result<Session> {
        let state = self.session_state(&id.0)?;
        if let Some(known_target) = &state.target_id
            && known_target != &target.0
        {
            return Err(Error::InvalidArgument(format!(
                "session {} belongs to target {}, not {}",
                id.0, known_target, target.0
            )));
        }
        Ok(Session {
            connection: self.clone(),
            id,
            target,
            closed: state.closed,
            parent: state.parent,
            auth_owner: state.auth_owner,
            domains: state.domains,
            detach: state.detach,
        })
    }

    /// Listen only to events on the root session.
    pub fn listen<E: Event>(&self) -> TypedEvents<E> {
        TypedEvents {
            stream: self.subscribe_typed(E::METHOD, None),
            session_id: None,
            closed: None,
            marker: PhantomData,
        }
    }
}

impl Session {
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Option<std::time::Duration>) -> Self {
        self.connection = self.connection.with_command_timeout(timeout);
        self
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }
    pub fn target_id(&self) -> &TargetId {
        &self.target
    }
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow() || self.connection.is_closed()
    }

    pub(crate) fn claim_proxy_auth(&self) -> Result<ProxyAuthLease> {
        self.auth_owner
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| {
                Error::ProxyAuthentication(
                    "authentication is already armed for this session".into(),
                )
            })?;
        Ok(ProxyAuthLease(self.auth_owner.clone()))
    }

    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        while !*closed.borrow() && !self.connection.is_closed() {
            if closed.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn execute<C: Command>(&self, command: &C) -> Result<C::Response> {
        serde_json::from_value(
            self.execute_raw(C::METHOD, serde_json::to_value(command)?)
                .await?,
        )
        .map_err(Into::into)
    }

    pub async fn execute_raw(&self, method: &str, params: Value) -> Result<Value> {
        if self.is_closed() {
            return Err(Error::SessionClosed(self.id.0.clone()));
        }
        self.connection
            .execute_session(method, params, Some(self.id.0.clone()))
            .await
    }

    pub fn listen<E: Event>(&self) -> TypedEvents<E> {
        TypedEvents {
            stream: self
                .connection
                .subscribe_typed(E::METHOD, Some(self.id.0.clone())),
            session_id: Some(self.id.0.clone()),
            closed: Some(self.closed.clone()),
            marker: PhantomData,
        }
    }

    /// Detach this session and its descendants.
    ///
    /// Once polled, detachment continues in an owned task if the caller cancels.
    /// Calls for the same session are serialized and recheck closure after waiting.
    /// The command uses this handle's timeout, capped at 30 seconds after acquiring
    /// the detach gate (including when command timeouts are disabled).
    /// On either timeout, an already closed session counts as success. Otherwise,
    /// the connection is closed to resolve uncertain detachment before returning
    /// `Error::Timeout`; connection teardown may take additional time.
    /// Protocol errors leave the session retryable unless it detached asynchronously.
    pub async fn detach(&self) -> Result<()> {
        let session = self.clone();
        self.connection
            .runtime()
            .spawn(async move {
                let _gate = session.detach.lock().await;
                if session.is_closed() {
                    return Ok(());
                }
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    session.connection.execute_session(
                        "Target.detachFromTarget",
                        json!({"sessionId":session.id}),
                        session.parent.clone(),
                    ),
                )
                .await
                .unwrap_or_else(|_| Err(Error::Timeout("Target.detachFromTarget".into())));
                if matches!(&result, Err(Error::Timeout(_))) {
                    if session.is_closed() {
                        return Ok(());
                    }
                    session.connection.close().await;
                }
                result.map(|_| ())
            })
            .await?
    }
}

pub struct TypedEvents<E> {
    stream: EventStream,
    session_id: Option<String>,
    closed: Option<watch::Receiver<bool>>,
    marker: PhantomData<E>,
}

impl<E: Event> TypedEvents<E> {
    pub async fn recv(&mut self) -> Result<E> {
        loop {
            let event = if let Some(closed) = &mut self.closed {
                if *closed.borrow() {
                    match self.stream.try_recv()? {
                        Some(event) => {
                            if event.session_id == self.session_id && event.method == E::METHOD {
                                return serde_json::from_value(event.params).map_err(Into::into);
                            }
                            continue;
                        }
                        None => {
                            return Err(Error::SessionClosed(
                                self.session_id.clone().unwrap_or_default(),
                            ));
                        }
                    }
                }
                tokio::select! {
                    result = self.stream.recv() => result?,
                    _ = closed.changed() => continue,
                }
            } else {
                self.stream.recv().await?
            };
            if event.session_id == self.session_id && event.method == E::METHOD {
                return serde_json::from_value(event.params).map_err(Into::into);
            }
        }
    }
}

pub(crate) struct ProxyAuthLease(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for ProxyAuthLease {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

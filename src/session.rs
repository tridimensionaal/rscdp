use crate::{
    Connection, Error, Result,
    cdp::target::{SessionId, TargetId},
    connection::EventStream,
    protocol::{Command, Event},
};
use serde_json::{Value, json};
use std::marker::PhantomData;
use tokio::sync::watch;

/// A flattened CDP session multiplexed over its parent connection.
#[derive(Clone)]
pub struct Session {
    connection: Connection,
    id: SessionId,
    target: TargetId,
    closed: watch::Receiver<bool>,
    parent: Option<String>,
    auth_owner: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    pub fn session(&self, id: SessionId, target: TargetId) -> Result<Session> {
        let (closed, parent, auth_owner) = self.session_state(&id.0)?;
        Ok(Session {
            connection: self.clone(),
            id,
            target,
            closed,
            parent,
            auth_owner,
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

    pub async fn detach(&self) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        self.connection
            .execute_session(
                "Target.detachFromTarget",
                json!({"sessionId":self.id}),
                self.parent.clone(),
            )
            .await?;
        Ok(())
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

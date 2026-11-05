use crate::{
    Connection, Error, Result,
    cdp::target::{SessionId, TargetId},
    connection::{EventOptions, EventStream},
    protocol::{Command, Event},
};
use serde_json::Value;
use std::marker::PhantomData;
use tokio::sync::watch;

mod domain;
pub use domain::DomainGuard;
pub(crate) use domain::Domains;
mod attachment;
pub(crate) use attachment::Attachment;

/// A flattened CDP session multiplexed over its parent connection.
///
/// Sessions returned by [`Connection::attach`] share attachment ownership with
/// their clones, wrappers for the same ID, and session-scoped event listeners.
/// Descendant sessions and their listeners retain ancestor attachments, but an
/// explicit ancestor detach still closes those descendants.
/// Dropping the last owner schedules best-effort detachment on the connection's
/// runtime. Protocol errors during Drop cleanup are ignored; an uncertain timeout
/// closes the connection. Use [`Session::detach`] to observe the cleanup result.
#[derive(Clone)]
pub struct Session {
    connection: Connection,
    id: SessionId,
    target: TargetId,
    closed: watch::Receiver<bool>,
    auth_owner: std::sync::Arc<std::sync::atomic::AtomicBool>,
    domains: std::sync::Arc<Domains>,
    attachment: std::sync::Arc<Attachment>,
    dom_transaction: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("target_id", &self.target)
            .field("session_watch_closed", &self.closed.has_changed().is_err())
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Attach with shared ownership: the last session handle or session listener
    /// schedules detachment.
    ///
    /// The handle's command timeout limits the caller's wait. Acquisition continues
    /// on the connection runtime after cancellation or timeout; an unclaimed session
    /// is dropped and detached. Acquisition has its own 30-second limit, including
    /// when command timeouts are disabled. If that limit expires, or a reply cannot
    /// be decoded, the connection is closed to resolve the unknown attachment state.
    pub async fn attach(&self, target: TargetId) -> Result<Session> {
        let connection = self.clone();
        let task = self.runtime().spawn(async move {
            let command_connection = connection.clone().with_command_timeout(None);
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                command_connection.execute(&crate::cdp::target::commands::AttachToTarget {
                    target_id: target.clone(),
                    flatten: Some(true),
                }),
            )
            .await;
            let response = match response {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    if matches!(&error, Error::Json(_) | Error::Decode { .. }) {
                        connection.close().await;
                    }
                    return Err(error);
                }
                Err(_) => {
                    connection.close().await;
                    return Err(Error::Timeout("Target.attachToTarget".into()));
                }
            };
            let session = connection.session(response.session_id, target)?;
            session.attachment.arm();
            Ok(session)
        });
        let joined = if let Some(timeout) = self.command_timeout() {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("Target.attachToTarget".into()))?
        } else {
            task.await
        };
        joined?
    }

    /// Wrap a registered session, including one reported by auto-attach.
    ///
    /// Wrapping a raw-command or auto-attached session does not acquire cleanup
    /// ownership: the caller must explicitly detach it. Wrappers of a session
    /// owned by [`Connection::attach`] share that ownership, including wrappers
    /// created from an event before the high-level attach completes.
    /// The wrapper retains its protocol ancestor attachments; explicitly detaching
    /// an ancestor still closes the wrapper and other descendants.
    /// Closed sessions and expired owned attachments cannot be wrapped again.
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
            auth_owner: state.auth_owner,
            domains: state.domains,
            attachment: state.attachment,
            dom_transaction: state.dom_transaction,
        })
    }

    /// Listen only to this event method on the root session, not attached sessions.
    /// Registers synchronously; subscribe before the action that emits events.
    /// This does not enable any protocol domain. Use [`Session::listen`] for an
    /// attached session or [`Connection::subscribe`] for raw events across sessions.
    pub fn listen<E: Event>(&self) -> TypedEvents<E> {
        TypedEvents {
            stream: self.subscribe_typed(E::METHOD, None),
            marker: PhantomData,
        }
    }

    /// Listen to one root-session event with an explicit buffer capacity.
    /// Capacity must be in 1..=usize::MAX / 2 and rounds up to a power of two.
    /// Registers synchronously without enabling a domain or retaining attachments.
    pub fn listen_with_capacity<E: Event>(&self, capacity: usize) -> Result<TypedEvents<E>> {
        Ok(TypedEvents {
            stream: self.subscribe_typed_with_capacity(E::METHOD, None, capacity)?,
            marker: PhantomData,
        })
    }
}

impl Session {
    /// Set this handle's command deadline, including queue admission.
    /// Subsequent clones inherit it; existing handles for the same session are
    /// unchanged. `None` removes the caller deadline, not socket-write deadlines
    /// or independent acquisition, domain-transition, and cleanup bounds.
    /// See [`Connection::with_command_timeout`] and [`Self::detach`].
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

    pub(crate) async fn lock_dom_transaction(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        let gate = tokio::select! {
            result = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.dom_transaction.clone().lock_owned(),
            ) => result.map_err(|_| Error::Timeout("waiting for DOM transaction".into()))?,
            () = self.closed() => return Err(Error::SessionClosed(self.id.0.clone())),
        };
        if self.is_closed() {
            return Err(Error::SessionClosed(self.id.0.clone()));
        }
        Ok(gate)
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

    /// Wait for this session or its connection to close, including when the
    /// connection's runtime shuts down.
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        while !*closed.borrow() && !self.connection.is_closed() {
            tokio::select! {
                result = closed.changed() => {
                    if result.is_err() {
                        break;
                    }
                }
                () = self.connection.receiver_closed() => break,
            }
        }
    }

    /// Send a typed command on this session and decode its response.
    /// Cancellation or timeout stops waiting but cannot undo a command already
    /// received by Chrome. Retrying can repeat side effects; retry only when the
    /// operation is safe to repeat. A decoding error can follow remote success.
    /// [`Error::Decode`] retains the method, original result payload, and cause.
    /// Typed commands do not acquire cleanup ownership of returned resources;
    /// prefer owned helpers such as [`Connection::attach`] where available.
    pub async fn execute<C: Command>(&self, command: &C) -> Result<C::Response> {
        self.execute_before(command, None).await
    }

    pub(crate) async fn execute_before<C: Command>(
        &self,
        command: &C,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<C::Response> {
        crate::error::decode(
            C::METHOD,
            self.execute_raw_before(C::METHOD, serde_json::to_value(command)?, deadline)
                .await?,
        )
    }

    /// Send a command on this session without typed response decoding.
    /// `params` must be a JSON object; use `serde_json::json!({})` for no parameters.
    /// Cancellation or timeout stops waiting, not remote execution; retries can
    /// repeat side effects. The ownership caveats of [`Self::execute`] also apply.
    pub async fn execute_raw(&self, method: &str, params: Value) -> Result<Value> {
        self.execute_raw_before(method, params, None).await
    }

    pub(crate) async fn execute_raw_before(
        &self,
        method: &str,
        params: Value,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Value> {
        let mut admitted = false;
        self.execute_raw_tracked(method, params, deadline, &mut admitted)
            .await
    }

    pub(crate) async fn execute_raw_tracked(
        &self,
        method: &str,
        params: Value,
        deadline: Option<tokio::time::Instant>,
        admitted: &mut bool,
    ) -> Result<Value> {
        *admitted = false;
        if self.is_closed() {
            return Err(self
                .connection
                .closed_error()
                .unwrap_or_else(|| Error::SessionClosed(self.id.0.clone())));
        }
        self.connection
            .execute_session_tracked(method, params, Some(self.id.0.clone()), deadline, admitted)
            .await
    }

    /// Listen only to this event method on this session, not descendant sessions.
    /// Registers synchronously and retains attachment ownership. This does not
    /// enable the domain: subscribe before enabling it or triggering the event,
    /// and retain the appropriate [`DomainGuard`] while receiving domain events.
    pub fn listen<E: Event>(&self) -> TypedEvents<E> {
        TypedEvents {
            stream: self
                .connection
                .subscribe_typed(E::METHOD, Some(self.id.0.clone()))
                .with_session(
                    self.id.0.clone(),
                    self.closed.clone(),
                    self.attachment.clone(),
                ),
            marker: PhantomData,
        }
    }

    /// Subscribe to all raw event methods on this exact session, not descendants.
    /// Registers synchronously and retains this attachment and its ancestors.
    /// No domain is enabled automatically. Buffered events precede detachment
    /// errors; overflow reports [`Error::Lagged`]. The connection's event capacity
    /// applies (256 by default, rounded up to a power of two).
    pub fn subscribe(&self) -> EventStream {
        self.connection.subscribe_session(&self.id.0).with_session(
            self.id.0.clone(),
            self.closed.clone(),
            self.attachment.clone(),
        )
    }

    /// Subscribe to selected raw methods on this exact session, not descendants.
    /// Filtering occurs before buffering, with matching methods in receive order.
    /// Empty methods select all. Retains attachment ownership and wakes on detach,
    /// like [`Self::subscribe`]. Invalid options fail before registration.
    pub fn subscribe_with(&self, options: EventOptions) -> Result<EventStream> {
        Ok(self
            .connection
            .subscribe_session_with(&self.id.0, options)?
            .with_session(
                self.id.0.clone(),
                self.closed.clone(),
                self.attachment.clone(),
            ))
    }

    /// Listen to one exact-session event with an explicit buffer capacity.
    /// Capacity must be in 1..=usize::MAX / 2 and rounds up to a power of two.
    /// Retains attachment ownership and detach behavior, like [`Self::listen`].
    pub fn listen_with_capacity<E: Event>(&self, capacity: usize) -> Result<TypedEvents<E>> {
        Ok(TypedEvents {
            stream: self
                .connection
                .subscribe_typed_with_capacity(E::METHOD, Some(self.id.0.clone()), capacity)?
                .with_session(
                    self.id.0.clone(),
                    self.closed.clone(),
                    self.attachment.clone(),
                ),
            marker: PhantomData,
        })
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
            .spawn(async move { session.attachment.detach(session.connection.clone()).await })
            .await?
    }
}

/// Events filtered by session and method before buffering. Capacity defaults to
/// the connection's setting (256 by default); `listen_with_capacity` overrides it.
/// Capacities round up to a power of two.
/// Overflow reports [`Error::Lagged`]. Detachment wakes session listeners,
/// including descendants; buffered events are delivered before closure.
/// Register with [`Session::listen`] before the action that can emit the event.
/// A fragment-only navigation may not emit a load event; use
/// [`crate::Page::navigate`] to await a specific navigation.
pub struct TypedEvents<E> {
    stream: EventStream,
    marker: PhantomData<E>,
}

impl<E: Event> std::fmt::Debug for TypedEvents<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedEvents")
            .field("method", &E::METHOD)
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

impl<E: Event> TypedEvents<E> {
    /// Receive the next matching event. Canceling a pending wait does not consume
    /// an event, so this can be used in `tokio::select!`.
    /// [`Error::Lagged`] reports lost events; call again to receive retained events.
    /// A typed decoding error consumes that event; later events remain receivable.
    /// [`Error::Decode`] retains its method, original payload, and cause.
    pub async fn recv(&mut self) -> Result<E> {
        let event = self.stream.recv().await?;
        crate::error::decode(E::METHOD, event.params)
    }
}

pub(crate) struct ProxyAuthLease(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for ProxyAuthLease {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

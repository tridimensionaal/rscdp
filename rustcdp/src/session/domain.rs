use super::Session;
use crate::{Error, Result, error::with_cleanup};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

type Counts = Arc<[Mutex<usize>; 3]>;
const METHODS: [(&str, &str); 3] = [
    ("DOM.enable", "DOM.disable"),
    ("Page.enable", "Page.disable"),
    ("Runtime.enable", "Runtime.disable"),
];
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(30);
const DETACH_TIMEOUT: Duration = Duration::from_secs(1);

/// Shared ownership of an enabled DOM, Page, or Runtime domain.
/// Await `close` to confirm the final disable; Drop schedules the same cleanup.
/// A failed final disable or a transition that stalls for 30 seconds triggers
/// session detachment. If the session remains open, recovery closes the connection.
/// The transition error is preserved; a failed detach is combined with it in
/// [`Error::Cleanup`]. Neither enable nor disable is retried.
/// Counts are shared across handles of the same session. Direct enable/disable
/// commands through either `execute` or `execute_raw` bypass these counts; do not
/// mix them with guards for the same domain. The first guard enables and the last
/// disables.
/// Transitions run on the connection runtime independently of caller cancellation
/// and command timeouts, with a one-second detach attempt during recovery.
#[must_use = "retain this guard while using the enabled domain; await close() to observe cleanup errors"]
pub struct DomainGuard {
    session: Session,
    counts: Counts,
    index: usize,
    active: bool,
    runtime: tokio::runtime::Handle,
}

impl Session {
    /// Enable DOM until the last shared guard is released.
    /// Retain the guard while using the domain; subscribe before enabling events.
    /// Direct typed or raw enable/disable commands bypass guard accounting;
    /// do not mix them with guards for this domain. See [`DomainGuard`].
    pub async fn enable_dom(&self) -> Result<DomainGuard> {
        self.enable_domain(0).await
    }
    /// Enable Page until the last shared guard is released.
    /// Retain the guard while using the domain; subscribe before enabling events.
    /// Direct typed or raw enable/disable commands bypass guard accounting;
    /// do not mix them with guards for this domain. See [`DomainGuard`].
    pub async fn enable_page(&self) -> Result<DomainGuard> {
        self.enable_domain(1).await
    }
    /// Enable Runtime events until the last shared guard is released.
    ///
    /// Subscribe before acquiring the guard to receive existing-context events
    /// emitted during enablement. Keep the guard while using those subscriptions.
    /// Direct typed or raw enable/disable commands bypass guard accounting;
    /// do not mix them with guards for this domain. See [`DomainGuard`].
    pub async fn enable_runtime(&self) -> Result<DomainGuard> {
        self.enable_domain(2).await
    }

    /// Only for setup whose enclosing ContextLease owns all failure recovery.
    /// The caller must disable command timeouts, bound setup, and dispose the
    /// context on error or cancellation, including partially completed enables.
    /// Subscribe before calling. No independent task, guard, or recovery is
    /// created until both enables succeed. Lock order is always Page then Runtime.
    pub(crate) async fn enable_page_runtime_in_context(&self) -> Result<[DomainGuard; 2]> {
        let counts = self.domains.clone();
        let mut page_users = counts[1].lock().await;
        let mut runtime_users = counts[2].lock().await;
        if self.is_closed() {
            return Err(Error::SessionClosed(self.id.0.clone()));
        }
        if *page_users == 0 {
            self.execute_raw("Page.enable", json!({})).await?;
        }
        if *runtime_users == 0 {
            self.execute_raw("Runtime.enable", json!({})).await?;
        }
        *page_users += 1;
        *runtime_users += 1;
        drop(runtime_users);
        drop(page_users);
        Ok([1, 2].map(|index| DomainGuard {
            session: self.clone().with_command_timeout(None),
            counts: counts.clone(),
            index,
            active: true,
            runtime: self.connection.runtime(),
        }))
    }

    async fn enable_domain(&self, index: usize) -> Result<DomainGuard> {
        if self.is_closed() {
            return Err(Error::SessionClosed(self.id.0.clone()));
        }
        // Keep transitions owned through completion or recovery, independently
        // of the caller's deadline.
        let session = self.clone().with_command_timeout(None);
        let runtime = self.connection.runtime();
        let owner_runtime = runtime.clone();
        let counts = self.domains.clone();
        // Acquisition completes even when its caller is cancelled; an unclaimed
        // guard is dropped and releases its count.
        runtime
            .spawn(async move {
                let mut users = counts[index].lock().await;
                if session.is_closed() {
                    return Err(Error::SessionClosed(session.id.0.clone()));
                }
                // This async mutex deliberately serializes the enable/disable
                // transition with reference-count updates across all session handles.
                if *users == 0 {
                    transition(&session, METHODS[index].0).await?;
                }
                *users += 1;
                drop(users);
                Ok(DomainGuard {
                    session,
                    counts,
                    index,
                    active: true,
                    runtime: owner_runtime,
                })
            })
            .await?
    }
}

impl DomainGuard {
    pub async fn close(mut self) -> Result<()> {
        let session = self.session.clone();
        let counts = self.counts.clone();
        let index = self.index;
        let task = self
            .runtime
            .spawn(async move { release(session, counts, index).await });
        self.active = false;
        task.await?
    }
}

async fn release(session: Session, counts: Counts, index: usize) -> Result<()> {
    let mut users = counts[index].lock().await;
    if *users == 0 {
        return Ok(());
    }
    *users -= 1;
    if *users == 0
        && !session.is_closed()
        && let Err(error) = transition(&session, METHODS[index].1).await
    {
        // There is no guard left to own an enabled domain. Retain the count
        // lock through recovery so another acquisition cannot reuse it.
        // A transition timeout already recovered; the closed check makes
        // that case a no-op rather than a second detach attempt.
        return with_cleanup(Err(error), recover(&session).await);
    }
    Ok(())
}

async fn transition(session: &Session, method: &str) -> Result<()> {
    match tokio::time::timeout(TRANSITION_TIMEOUT, session.execute_raw(method, json!({}))).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            // Timing out cannot establish whether the remote domain changed.
            // Keep ownership until detachment or connection teardown resolves it.
            with_cleanup(Err(Error::Timeout(method.into())), recover(session).await)
        }
    }
}

async fn recover(session: &Session) -> Result<()> {
    if session.is_closed() {
        return Ok(());
    }
    let detached = tokio::time::timeout(DETACH_TIMEOUT, session.detach())
        .await
        .unwrap_or_else(|_| {
            Err(Error::Timeout(
                "detaching session after domain transition failure".into(),
            ))
        });
    if !session.is_closed() {
        session.connection().close().await;
    }
    detached
}

impl Drop for DomainGuard {
    fn drop(&mut self) {
        if self.active {
            let session = self.session.clone();
            let counts = self.counts.clone();
            let index = self.index;
            self.runtime.spawn(async move {
                let _ = release(session, counts, index).await;
            });
        }
    }
}

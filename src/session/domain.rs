use super::Session;
use crate::{Error, Result};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

type Counts = Arc<[Mutex<usize>; 2]>;
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(30);
const DETACH_TIMEOUT: Duration = Duration::from_secs(1);

/// Shared ownership of an enabled DOM or Page domain.
/// Await `close` to confirm the final disable; Drop schedules the same cleanup.
/// A transition that stalls for 30 seconds triggers session detachment; failed
/// detachment closes the connection to resolve the uncertain domain state.
pub struct DomainGuard {
    session: Session,
    counts: Counts,
    index: usize,
    active: bool,
    runtime: tokio::runtime::Handle,
}

impl Session {
    pub async fn enable_dom(&self) -> Result<DomainGuard> {
        self.enable_domain(0).await
    }
    pub async fn enable_page(&self) -> Result<DomainGuard> {
        self.enable_domain(1).await
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
                    let method = if index == 0 {
                        "DOM.enable"
                    } else {
                        "Page.enable"
                    };
                    transition(&session, method).await?;
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
    if *users == 0 && !session.is_closed() {
        let method = if index == 0 {
            "DOM.disable"
        } else {
            "Page.disable"
        };
        transition(&session, method).await?;
    }
    Ok(())
}

async fn transition(session: &Session, method: &str) -> Result<()> {
    match tokio::time::timeout(TRANSITION_TIMEOUT, session.execute_raw(method, json!({}))).await {
        Ok(result) => result.map(|_| ()),
        Err(_) => {
            // Timing out cannot establish whether the remote domain changed.
            // Keep ownership until detachment or connection teardown resolves it.
            let detached = tokio::time::timeout(DETACH_TIMEOUT, session.detach()).await;
            if !matches!(detached, Ok(Ok(()))) && !session.is_closed() {
                session.connection().close().await;
            }
            Err(Error::Timeout(method.into()))
        }
    }
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

use super::Page;
use crate::{
    Connection, Error, Result,
    cdp::target::{
        FilterEntry, TargetFilter, TargetId,
        commands::{CloseTarget, GetTargets},
    },
};
use std::time::Duration;
use tokio::task::JoinHandle;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    Tab,
    Iframe,
}

#[derive(Default)]
pub(super) struct CloseState {
    confirmed_closed: bool,
    task: Option<JoinHandle<Result<()>>>,
}

impl Page {
    /// The target identity, distinct from a session or frame ID and available after close.
    pub fn id(&self) -> &TargetId {
        self.session.target_id()
    }

    /// Close this tab and confirm its absence from the browser's target list.
    /// Does not dispose its context, close sibling tabs, or kill an external browser.
    /// Iframe-target wrappers reject this operation locally. Dropping Page alone
    /// does not close a tab.
    ///
    /// Clones share one in-flight attempt. Once started, it continues after caller
    /// cancellation, retaining its session and fallback context. A later call
    /// awaits that attempt; confirmed success is cached. One caller receives an
    /// attempt's error; a subsequent explicit call reconciles target existence
    /// before retrying. No close command is automatically retried.
    ///
    /// The handle deadline, capped at 30 seconds, bounds waiting including mutex
    /// admission. The initiating handle's same budget independently bounds the
    /// owned attempt. A zero budget starts no attempt. Timing out does not prove
    /// the tab stayed open; losing the connection does not prove it closed.
    /// Session notifications and authentication-worker shutdown may follow target
    /// disappearance asynchronously. Keep the Tokio runtime alive through cleanup.
    pub async fn close(&self) -> Result<()> {
        if self.target_kind != TargetKind::Tab {
            return Err(Error::InvalidArgument(
                "Page::close cannot close an iframe target".into(),
            ));
        }
        let limit = self
            .session
            .connection()
            .command_timeout()
            .unwrap_or(Duration::from_secs(30))
            .min(Duration::from_secs(30));
        let timed_out = || Error::Timeout(format!("closing page {}", self.id()));
        if limit.is_zero() {
            return Err(timed_out());
        }
        tokio::time::timeout(limit, async {
            let mut state = self.close_state.lock().await;
            if state.confirmed_closed {
                return Ok(());
            }
            if state.task.is_none() {
                let session = self.session.clone();
                let lease = self.context_lease.clone();
                let auth = self.auth.clone();
                let connection = session.connection().clone().with_command_timeout(None);
                let id = self.id().clone();
                state.task = Some(connection.runtime().spawn(async move {
                    let result = tokio::time::timeout(limit, Self::close_target(&connection, &id))
                        .await
                        .unwrap_or_else(|_| Err(Error::Timeout(format!("closing page {id}"))));
                    drop(session);
                    drop(auth);
                    drop(lease);
                    result
                }));
            }
            // Await by reference: cancellation leaves the completion handle owned.
            let joined = state.task.as_mut().expect("close attempt started").await;
            state.task = None;
            let result = joined?;
            state.confirmed_closed = result.is_ok();
            result
        })
        .await
        .map_err(|_| timed_out())?
    }
}

async fn target_kind(connection: &Connection, id: &TargetId) -> Result<Option<String>> {
    // An omitted filter can inherit a discovery filter and falsely imply absence.
    let targets = connection
        .execute(&GetTargets::new().with_filter(TargetFilter(vec![FilterEntry::default()])))
        .await?;
    Ok(targets
        .target_infos
        .into_iter()
        .find(|target| target.target_id == *id)
        .map(|target| target.type_))
}

impl Page {
    pub(crate) async fn close_target(connection: &Connection, id: &TargetId) -> Result<()> {
        match target_kind(connection, id).await?.as_deref() {
            None => return Ok(()),
            Some("page") => {}
            Some(_) => {
                return Err(Error::InvalidArgument(
                    "Page::close requires a page target".into(),
                ));
            }
        }
        let response = connection.execute(&CloseTarget::new(id.clone())).await?;
        if !response.success {
            return Err(Error::UnexpectedResponse {
                method: "Target.closeTarget".into(),
                payload: serde_json::to_value(response)?,
            });
        }
        while target_kind(connection, id).await?.is_some() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }
}

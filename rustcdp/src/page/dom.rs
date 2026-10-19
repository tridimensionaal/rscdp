use super::Page;
use crate::{Error, Result, Session, cdp::dom::Node, error::with_cleanup};
use serde_json::json;
use std::{sync::Arc, time::Duration};

/// Options for [`Page::search_dom`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DomSearchOptions {
    /// Include user-agent shadow DOM in search results. Defaults to false.
    /// Document preparation traverses frames and shadow roots independently.
    pub include_user_agent_shadow_dom: bool,
}

impl Page {
    /// Read document HTML in a serialized, owned DOM transaction.
    /// The caller's deadline limits waiting, not ownership. Queueing and transaction
    /// work each have a 30-second limit, excluding domain transitions and recovery.
    /// An uncertain transaction timeout detaches this session (and descendants),
    /// or closes the connection if detachment cannot be confirmed.
    /// This resets frontend node IDs; raw commands and navigation are not serialized.
    pub async fn html(&self) -> Result<String> {
        let timeout = self.session.connection().command_timeout();
        let page = self.clone();
        let task = self.session.connection().runtime().spawn(async move {
            let domain = page.session.enable_dom().await?;
            let result = async {
                let gate = page.session.lock_dom_transaction().await?;
                let commands = page.clone().with_command_timeout(None);
                let result =
                    tokio::time::timeout(Duration::from_secs(30), commands.document_html())
                        .await
                        .unwrap_or_else(|_| Err(Error::Timeout("reading document HTML".into())));
                if result.as_ref().is_err_and(dom_timeout) {
                    page.recover_session().await;
                }
                drop(gate);
                result
            }
            .await;
            let result = with_cleanup(result, domain.close().await);
            drop(page);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for document HTML".into()))??
        } else {
            task.await?
        }
    }

    async fn document_html(&self) -> Result<String> {
        let response = self
            .session
            .execute_raw("DOM.getDocument", json!({}))
            .await?;
        let response = self
            .session
            .execute_raw(
                "DOM.getOuterHTML",
                json!({"nodeId":response["root"]["nodeId"]}),
            )
            .await?;
        serde_json::from_value(response["outerHTML"].clone()).map_err(Into::into)
    }

    /// Search by plain text, CSS selector, or XPath using CDP DOM search.
    /// [`DomSearchOptions`] controls inclusion of user-agent shadow DOM in results.
    /// Preparation always requests the full subtree with frame/shadow-root traversal.
    /// Returned nodes are protocol snapshots, may include non-element nodes, and
    /// do not own their originating target or document.
    /// Acquire an `enable_dom()` guard before searching and retain it while using
    /// the returned frontend node IDs.
    /// `resolve_node()` uses backend IDs and does not require that extra guard,
    /// but still requires the correct originating target and document. A search
    /// result from a descendant frame is not necessarily resolvable in this
    /// Page's root execution context.
    ///
    /// Once a search ID is known, cleanup makes one owned discard attempt capped
    /// at 30 seconds, preserving a shorter command timeout. Normal completion
    /// awaits cleanup and reports failures; Drop schedules the same attempt
    /// best-effort, without retrying a failed explicit cleanup.
    ///
    /// Transactions are serialized with `Page::html` across all handles for this
    /// session. Caller cancellation or timeout leaves the owner running. Queueing
    /// and transaction work each have a 30-second limit, excluding domain
    /// transitions and recovery. Uncertain timeouts or an invalid search identity
    /// detach the session and descendants, falling back to connection closure.
    /// A DOM guard prevents disable, not frontend ID invalidation: another search,
    /// HTML read, raw document reset, or navigation can invalidate returned IDs.
    /// Recovery runs before releasing the transaction gate, with a one-second
    /// detach attempt before connection closure. Other decoding errors still await
    /// discard; [`Error::Cleanup`] preserves both errors if discard also fails.
    pub async fn search_dom(&self, query: &str, options: DomSearchOptions) -> Result<Vec<Node>> {
        let timeout = self.session.connection().command_timeout();
        let page = self.clone();
        let query = query.to_owned();
        let task = self.session.connection().runtime().spawn(async move {
            let domain = page.session.enable_dom().await?;
            let result = async {
                let gate = page.session.lock_dom_transaction().await?;
                let commands = page.clone().with_command_timeout(None);
                let mut recovery_required = false;
                let result = tokio::time::timeout(
                    Duration::from_secs(30),
                    commands.search_dom_owned(
                        &query,
                        options,
                        &page.session,
                        &mut recovery_required,
                    ),
                )
                .await
                .unwrap_or_else(|_| Err(Error::Timeout("searching DOM".into())));
                if recovery_required || result.as_ref().is_err_and(dom_timeout) {
                    page.recover_session().await;
                }
                drop(gate);
                result
            }
            .await;
            let result = with_cleanup(result, domain.close().await);
            drop(page);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for DOM search".into()))??
        } else {
            task.await?
        }
    }

    async fn search_dom_owned(
        &self,
        query: &str,
        options: DomSearchOptions,
        cleanup_session: &Session,
        recovery_required: &mut bool,
    ) -> Result<Vec<Node>> {
        self.session
            .execute_raw("DOM.getDocument", json!({"depth":-1,"pierce":true}))
            .await?;
        let search = self
            .session
            .execute_raw(
                "DOM.performSearch",
                json!({"query":query,"includeUserAgentShadowDOM":options.include_user_agent_shadow_dom}),
            )
            .await?;
        let search_id = match serde_json::from_value::<String>(search["searchId"].clone()) {
            Ok(id) if !id.is_empty() => id,
            invalid => {
                let error = match invalid {
                    Err(error) => Error::Json(error),
                    Ok(_) => Error::InvalidArgument("empty DOM search ID".into()),
                };
                *recovery_required = true;
                return Err(error);
            }
        };
        let guard = Search {
            session: cleanup_session.clone(),
            id: Some(search_id.clone()),
            context_lease: self.context_lease.clone(),
        };
        let result = async {
            let count: i64 = serde_json::from_value(search["resultCount"].clone())?;
            if count < 0 {
                return Err(Error::InvalidArgument(
                    "search result count must be nonnegative".into(),
                ));
            }
            let mut nodes = Vec::new();
            if count > 0 {
                let response = self
                    .session
                    .execute_raw(
                        "DOM.getSearchResults",
                        json!({"searchId":search_id,"fromIndex":0,"toIndex":count}),
                    )
                    .await?;
                let ids: Vec<i64> = serde_json::from_value(response["nodeIds"].clone())?;
                for id in ids {
                    let node = self
                        .session
                        .execute_raw("DOM.describeNode", json!({"nodeId":id}))
                        .await?;
                    nodes.push(serde_json::from_value(node["node"].clone())?);
                }
            }
            Ok(nodes)
        }
        .await;
        with_cleanup(result, guard.close().await)
    }
}

fn dom_timeout(error: &Error) -> bool {
    match error {
        Error::Timeout(_) => true,
        Error::Cleanup { source, cleanup } => dom_timeout(source) || dom_timeout(cleanup),
        _ => false,
    }
}

struct Search {
    session: Session,
    id: Option<String>,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
}
impl Search {
    async fn close(mut self) -> Result<()> {
        if let Some(task) = self.discard() {
            task.await?
        } else {
            Ok(())
        }
    }

    fn discard(&mut self) -> Option<tokio::task::JoinHandle<Result<()>>> {
        let id = self.id.take()?;
        let session = self.session.clone();
        let context_lease = self.context_lease.take();
        Some(self.session.connection().runtime().spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                session.execute_raw("DOM.discardSearchResults", json!({"searchId":id})),
            )
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("DOM.discardSearchResults".into())))
            .map(|_| ());
            drop(context_lease);
            result
        }))
    }
}
impl Drop for Search {
    fn drop(&mut self) {
        drop(self.discard());
    }
}

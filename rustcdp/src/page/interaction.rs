use super::{ClickMethod, Element, Page, dom_timeout, element::node_result};
use crate::{
    Error, Result, Session,
    cdp::{dom::Node, page::FrameId},
    error::with_cleanup,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

/// Options for [`Page::wait_for_selector`] and [`Page::wait_for_xpath`].
///
/// Stability means the sampled number of matching elements stays nonzero and
/// unchanged. It does not track element identity, visibility, or position.
#[derive(Debug, Clone)]
pub struct WaitOptions {
    /// Deadline for observing a stable count. Defaults to 5 seconds.
    ///
    /// This excludes [`Self::delay_after`]. Expiration returns `Ok(false)`.
    pub timeout: Duration,
    /// Delay between completed polls. Defaults to 100 milliseconds.
    ///
    /// A zero interval is rejected with [`Error::InvalidArgument`].
    pub polling_interval: Duration,
    /// How long the sampled count must remain stable. Defaults to 2 seconds.
    ///
    /// A zero count, changed count, or recoverable navigation error resets this
    /// interval. A zero duration accepts the first nonzero count.
    pub stability_time: Duration,
    /// Additional delay after success, outside the timeout. Defaults to 1 second.
    pub delay_after: Duration,
}

impl Default for WaitOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            polling_interval: Duration::from_millis(100),
            stability_time: Duration::from_secs(2),
            delay_after: Duration::from_secs(1),
        }
    }
}

impl Page {
    /// Find the first matching CSS element. Invalid selectors return a JavaScript error.
    /// Acquisition continues after caller cancellation under its own 30-second limit.
    /// Uncertain object acquisition detaches the session or closes the connection.
    /// Recovery attempts detach for one second before closing the connection;
    /// a timeout before the object request does not detach. Known IDs are owned
    /// before decoding and deduplicated; discarded handles schedule bounded
    /// release preserving a shorter original handle timeout.
    /// Only a node or null is accepted; received exception object IDs are released
    /// best-effort and remain diagnostic data, not live handles, in the error.
    pub async fn element_by_selector(&self, selector: &str) -> Result<Option<Element>> {
        let selector = serde_json::to_string(selector)?;
        self.element_for_expression(format!("document.querySelector({selector})"))
            .await
    }

    pub(super) async fn element_for_expression(
        &self,
        expression: String,
    ) -> Result<Option<Element>> {
        self.element_for_expression_in_frame(expression, None).await
    }

    pub(super) async fn element_for_expression_in_frame(
        &self,
        expression: String,
        frame: Option<FrameId>,
    ) -> Result<Option<Element>> {
        let timeout = self.session.connection().command_timeout();
        let page = self.clone();
        let task = self.session.connection().runtime().spawn(async move {
            let commands = page.clone().with_command_timeout(None);
            let mut unknown_resource = false;
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let context = match &frame {
                    Some(frame) => commands.isolated_context_for(frame).await?,
                    None => commands.isolated_context().await?,
                };
                unknown_resource = true;
                let response = match commands
                    .session
                    .execute_raw(
                        "Runtime.evaluate",
                        json!({"expression":expression,"uniqueContextId":context.unique,
                        "silent":true,"returnByValue":false,"awaitPromise":true}),
                    )
                    .await
                {
                    Ok(response) => response,
                    Err(error @ Error::Protocol(_)) => {
                        unknown_resource = false;
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                };
                let mut objects = Vec::new();
                unknown_resource =
                    !page.own_response_objects(&response, "result", false, &mut objects);
                if let Some(error) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                node_result(&response["result"], &objects)
            })
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("acquiring selector element".into())));
            if unknown_resource {
                page.recover_session().await;
            }
            drop(page);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for selector element".into()))??
        } else {
            task.await?
        }
    }

    /// Find the first XPath node, with the same ownership and deadline policy as
    /// [`Self::element_by_selector`].
    pub async fn element_by_xpath(&self, xpath: &str) -> Result<Option<Element>> {
        let xpath = serde_json::to_string(xpath)?;
        self.element_for_expression(format!("document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue")).await
    }

    /// Search by plain text, CSS selector, or XPath using CDP DOM search.
    /// Optionally include user-agent shadow DOM.
    /// Acquire an `enable_dom()` guard before searching and retain it while using
    /// the returned frontend node IDs.
    /// `resolve_node()` uses backend IDs and does not require that extra guard.
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
    pub async fn search_dom(&self, query: &str, include_shadow_dom: bool) -> Result<Vec<Node>> {
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
                        include_shadow_dom,
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
        include_shadow_dom: bool,
        cleanup_session: &Session,
        recovery_required: &mut bool,
    ) -> Result<Vec<Node>> {
        self.session
            .execute_raw(
                "DOM.getDocument",
                json!({"depth":-1,"pierce":include_shadow_dom}),
            )
            .await?;
        let search = self
            .session
            .execute_raw(
                "DOM.performSearch",
                json!({"query":query,"includeUserAgentShadowDOM":include_shadow_dom}),
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

    /// Wait for a stable, nonzero XPath match count, as in [`Self::wait_for_selector`].
    pub async fn wait_for_xpath(&self, xpath: &str, options: WaitOptions) -> Result<bool> {
        let xpath = serde_json::to_string(xpath)?;
        let expression = format!(
            "document.evaluate({xpath}, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength"
        );
        self.wait_for_count(&expression, options).await
    }

    /// Wait for a stable, nonzero number of CSS matches. Returns false on timeout.
    /// Polling retries recognized navigation/context-loss errors and resets the
    /// stability interval; other errors propagate. See [`WaitOptions`] for sampling.
    pub async fn wait_for_selector(&self, selector: &str, options: WaitOptions) -> Result<bool> {
        let selector = serde_json::to_string(selector)?;
        self.wait_for_count(
            &format!("document.querySelectorAll({selector}).length"),
            options,
        )
        .await
    }

    async fn wait_for_count(&self, expression: &str, options: WaitOptions) -> Result<bool> {
        if options.polling_interval.is_zero() {
            return Err(Error::InvalidArgument(
                "polling interval must be nonzero".into(),
            ));
        }
        let result = tokio::time::timeout(options.timeout, async {
            let mut previous = 0;
            let mut stable = Instant::now();
            loop {
                let count = match self.evaluate(expression).await {
                    Ok(value) => value.as_u64().ok_or_else(|| {
                        Error::InvalidArgument("selector count is not an integer".into())
                    })?,
                    Err(error) if is_context_change(&error) => {
                        previous = 0;
                        stable = Instant::now();
                        tokio::time::sleep(options.polling_interval).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if count == 0 || count != previous {
                    stable = Instant::now();
                }
                if count > 0 && stable.elapsed() >= options.stability_time {
                    return Ok::<_, Error>(());
                }
                previous = count;
                tokio::time::sleep(options.polling_interval).await;
            }
        })
        .await;
        match result {
            Ok(result) => {
                result?;
                tokio::time::sleep(options.delay_after).await;
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    pub async fn click_xpath(&self, xpath: &str, method: ClickMethod, scroll: bool) -> Result<()> {
        self.element_by_xpath(xpath)
            .await?
            .ok_or_else(|| Error::InvalidArgument(format!("element not found: {xpath}")))?
            .click(method, scroll)
            .await
    }
}

fn is_context_change(error: &Error) -> bool {
    match error {
        Error::ExecutionContextChanged => true,
        Error::Protocol(error) if error.code == -32602 => {
            error.message == "uniqueContextId not found"
        }
        Error::Protocol(error) if error.code == -32000 => {
            let message = error.message.to_ascii_lowercase();
            [
                "execution context was destroyed",
                "cannot find context",
                "no frame for given id",
                "frame with the given id was not found",
                "inspected target navigated",
            ]
            .iter()
            .any(|known| message.contains(known))
        }
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

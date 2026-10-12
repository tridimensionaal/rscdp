use super::Page;
use crate::{
    Error, Result, Session,
    cdp::{dom::Node, runtime::RemoteObjectId},
    error::with_cleanup,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, Default)]
pub enum ClickMethod {
    #[default]
    Cdp,
    JavaScript,
}

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

#[derive(Debug, Clone, Copy)]
pub struct ElementPosition {
    pub x: f64,
    pub y: f64,
    pub center_x: f64,
    pub center_y: f64,
}

/// An owned remote node reference. Dropping its last clone schedules a best-effort
/// release attempt, capped at 30 seconds while preserving a shorter handle
/// deadline. Drop does not guarantee that the remote release is acknowledged.
#[derive(Clone)]
pub struct Element(pub(super) Arc<RemoteNode>);

pub(super) struct RemoteNode {
    pub(super) session: Session,
    pub(super) id: RemoteObjectId,
    context_lease: Option<Arc<crate::browser::ContextLease>>,
}

impl Drop for RemoteNode {
    fn drop(&mut self) {
        let session = self.session.clone();
        let id = self.id.clone();
        let context_lease = self.context_lease.take();
        self.session.connection().runtime().spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(30),
                session.execute_raw("Runtime.releaseObject", json!({"objectId":id})),
            )
            .await;
            drop(context_lease);
        });
    }
}

impl Element {
    pub(super) fn from_object_id(
        session: Session,
        id: RemoteObjectId,
        context_lease: Option<Arc<crate::browser::ContextLease>>,
    ) -> Self {
        Self(Arc::new(RemoteNode {
            session,
            id,
            context_lease,
        }))
    }

    pub fn object_id(&self) -> &RemoteObjectId {
        &self.0.id
    }

    pub async fn position(&self) -> Result<ElementPosition> {
        let result = self
            .0
            .session
            .execute_raw("DOM.getBoxModel", json!({"objectId":self.0.id}))
            .await?;
        let points: Vec<f64> = serde_json::from_value(result["model"]["border"].clone())?;
        if points.len() != 8 || points.iter().any(|p| !p.is_finite()) {
            return Err(Error::InvalidArgument("invalid element box quad".into()));
        }
        Ok(ElementPosition {
            x: points[0],
            y: points[1],
            center_x: (points[0] + points[2] + points[4] + points[6]) / 4.0,
            center_y: (points[1] + points[3] + points[5] + points[7]) / 4.0,
        })
    }

    pub async fn scroll_into_view(&self) -> Result<()> {
        self.0
            .session
            .execute_raw("DOM.scrollIntoViewIfNeeded", json!({"objectId":self.0.id}))
            .await?;
        Ok(())
    }

    pub async fn click(&self, method: ClickMethod, scroll: bool) -> Result<()> {
        if scroll {
            self.scroll_into_view().await?;
        }
        match method {
            ClickMethod::Cdp => {
                let position = self.position().await?;
                click_cdp(&self.0.session, position.center_x, position.center_y).await
            }
            ClickMethod::JavaScript => {
                let result = self.0.session.execute_raw("Runtime.callFunctionOn", json!({"objectId":self.0.id,"functionDeclaration":"function() { this.click(); }","userGesture":true,"returnByValue":true})).await?;
                if let Some(error) = result.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                Ok(())
            }
        }
    }
}

async fn click_cdp(session: &Session, x: f64, y: f64) -> Result<()> {
    if !x.is_finite() || !y.is_finite() {
        return Err(Error::InvalidArgument(
            "click coordinates must be finite".into(),
        ));
    }
    dispatch_pair(
        session,
        "Input.dispatchMouseEvent",
        json!({"type":"mousePressed","x":x,"y":y,"button":"left","buttons":1,"clickCount":1}),
        json!({"type":"mouseReleased","x":x,"y":y,"button":"left","buttons":0,"clickCount":1}),
    )
    .await
}

pub(super) async fn dispatch_pair(
    session: &Session,
    method: &'static str,
    press: Value,
    release: Value,
) -> Result<()> {
    let session = session.clone();
    session
        .connection()
        .runtime()
        .spawn(async move {
            let pressed = session.execute_raw(method, press).await;
            let released = session.execute_raw(method, release).await;
            with_cleanup(pressed.map(|_| ()), released.map(|_| ()))
        })
        .await?
}

impl Page {
    /// Resolve a CDP DOM node into an owned element for geometry and clicking.
    ///
    /// The node must come from this Page's target, and its originating document
    /// must not have been replaced. Raw CDP nodes carry no session or document
    /// identity, so their origin and freshness are the caller's responsibility.
    /// Before returning, this verifies that the resolved object belongs to the
    /// isolated world's unique execution context.
    /// Validation is capped at 30 seconds, preserving a shorter command timeout.
    pub async fn resolve_node(&self, node: &Node) -> Result<Element> {
        let page = self.clone();
        let backend_id = node.backend_node_id.clone();
        self.session
            .connection()
            .runtime()
            .spawn(async move {
                let context = page.isolated_context().await?;
                let response = page
                    .session
                    .execute_raw(
                        "DOM.resolveNode",
                        json!({"backendNodeId":backend_id,"executionContextId":context.numeric}),
                    )
                    .await?;
                let id: RemoteObjectId =
                    serde_json::from_value(response["object"]["objectId"].clone())?;
                // Own the remote reference before any further await so every
                // failure (including an abandoned owner) schedules its release.
                let element =
                    Element::from_object_id(page.session.clone(), id, page.context_lease.clone());
                let validation = tokio::time::timeout(
                    Duration::from_secs(30),
                    page.session.execute_raw(
                        "Runtime.callFunctionOn",
                        json!({
                            "uniqueContextId": context.unique,
                            "arguments": [{"objectId": element.object_id()}],
                            "functionDeclaration": "function(node) { return true; }",
                            "returnByValue": true,
                            "silent": true,
                        }),
                    ),
                )
                .await
                .map_err(|_| {
                    Error::Timeout("validating resolved node execution context".into())
                })??;
                if let Some(error) = validation.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                if validation["result"]["type"].as_str() != Some("boolean")
                    || validation["result"]["value"].as_bool() != Some(true)
                {
                    return Err(Error::InvalidArgument(
                        "unexpected resolved node context validation result".into(),
                    ));
                }
                Ok(element)
            })
            .await?
    }

    /// Find the first matching CSS element. Invalid selectors return a JavaScript error.
    pub async fn element_by_selector(&self, selector: &str) -> Result<Option<Element>> {
        let selector = serde_json::to_string(selector)?;
        self.element_for_expression(format!("document.querySelector({selector})"))
            .await
    }

    pub(super) async fn element_for_expression(
        &self,
        expression: String,
    ) -> Result<Option<Element>> {
        let page = self.clone();
        self.session
            .connection()
            .runtime()
            .spawn(async move {
                let result = page.evaluate_remote(&expression, false).await?;
                Ok(result.object_id.map(|id| {
                    Element::from_object_id(page.session.clone(), id, page.context_lease.clone())
                }))
            })
            .await?
    }

    pub async fn element_by_xpath(&self, xpath: &str) -> Result<Option<Element>> {
        let xpath = serde_json::to_string(xpath)?;
        self.element_for_expression(format!("document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue")).await
    }

    /// Search by plain text, CSS selector, or XPath using CDP DOM search.
    /// Optionally include user-agent shadow DOM.
    /// Acquire an `enable_dom()` guard before searching and retain it while using
    /// the returned frontend node IDs.
    /// `resolve_node()` uses backend IDs and does not require that extra guard.
    pub async fn search_dom(&self, query: &str, include_shadow_dom: bool) -> Result<Vec<Node>> {
        let page = self.clone();
        let query = query.to_owned();
        self.session
            .connection()
            .runtime()
            .spawn(async move {
                let domain = page.session.enable_dom().await?;
                let result = page.search_dom_owned(&query, include_shadow_dom).await;
                with_cleanup(result, domain.close().await)
            })
            .await?
    }

    async fn search_dom_owned(&self, query: &str, include_shadow_dom: bool) -> Result<Vec<Node>> {
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
        let search_id: String = serde_json::from_value(search["searchId"].clone())?;
        let count: i64 = serde_json::from_value(search["resultCount"].clone())?;
        let guard = Search {
            session: self.session.clone(),
            id: Some(search_id.clone()),
        };
        let result = async {
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

    pub async fn wait_for_xpath(&self, xpath: &str, options: WaitOptions) -> Result<bool> {
        let xpath = serde_json::to_string(xpath)?;
        let expression = format!(
            "document.evaluate({xpath}, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength"
        );
        self.wait_for_count(&expression, options).await
    }

    /// Wait for a stable, nonzero number of CSS matches. Returns false on timeout.
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

    pub async fn click_position(&self, x: f64, y: f64, method: ClickMethod) -> Result<()> {
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidArgument(
                "click coordinates must be finite".into(),
            ));
        }
        match method {
            ClickMethod::Cdp => click_cdp(&self.session, x, y).await,
            ClickMethod::JavaScript => {
                self.evaluate_remote(&format!("(() => {{ const e = document.elementFromPoint({x},{y}); if (!e) throw new Error('No element at point'); e.dispatchEvent(new MouseEvent('click', {{bubbles:true,cancelable:true,clientX:{x},clientY:{y}}})); }})()"),true).await?;
                Ok(())
            }
        }
    }

    pub async fn press_escape(&self) -> Result<()> {
        self.press_key("Escape").await
    }

    pub async fn go_back(&self) -> Result<bool> {
        let history = self
            .session
            .execute(&crate::cdp::page::commands::GetNavigationHistory::default())
            .await?;
        let Some(index) = history
            .current_index
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Ok(false);
        };
        let Some(entry) = history.entries.get(index) else {
            return Ok(false);
        };
        self.session
            .execute_raw("Page.navigateToHistoryEntry", json!({"entryId":entry.id}))
            .await?;
        Ok(true)
    }

    /// Attach to an out-of-process frame target owned by this page.
    pub async fn frame(&self, index: usize) -> Result<Page> {
        let connection = self.session.connection();
        let targets = connection
            .execute(&crate::cdp::target::commands::GetTargets::default())
            .await?;
        let mut matched = 0;
        for target in targets.target_infos.into_iter().filter(|target| {
            target.type_ == "iframe" && target.target_id != *self.session.target_id()
        }) {
            match self
                .session
                .execute_raw("DOM.getFrameOwner", json!({"frameId":target.target_id}))
                .await
            {
                Ok(_) => {
                    if matched == index {
                        let parent = self.clone();
                        let timeout = connection.command_timeout();
                        let task = connection.runtime().spawn(async move {
                            let session = parent
                                .session
                                .connection()
                                .clone()
                                .with_command_timeout(None)
                                .attach(target.target_id)
                                .await?
                                .with_command_timeout(timeout);
                            let mut page =
                                Page::from_session(session, parent.context.clone()).await?;
                            page.context_lease = parent.context_lease.clone();
                            drop(parent);
                            Ok(page)
                        });
                        let joined = if let Some(timeout) = timeout {
                            tokio::time::timeout(timeout, task)
                                .await
                                .map_err(|_| Error::Timeout("attaching frame".into()))?
                        } else {
                            task.await
                        };
                        return joined?;
                    }
                    matched += 1;
                }
                Err(Error::Protocol(error))
                    if error.code == -32000
                        && (error.message == "Frame with the given id was not found."
                            || error.message
                                == "Frame with the given id does not belong to the target.") => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::InvalidArgument(
            "frame target index out of bounds".into(),
        ))
    }

    pub async fn flash_point(&self, x: f64, y: f64, duration: Duration, size: f64) -> Result<()> {
        if !x.is_finite() || !y.is_finite() || !size.is_finite() || size <= 0.0 {
            return Err(Error::InvalidArgument("invalid flash geometry".into()));
        }
        let millis = duration.as_millis();
        self.evaluate_remote(&format!("(() => {{ const d = document.createElement('div'); Object.assign(d.style, {{position:'fixed',pointerEvents:'none',zIndex:2147483647,left:'{}px',top:'{}px',width:'{size}px',height:'{size}px',borderRadius:'50%',background:'red'}}); document.documentElement.appendChild(d); d.animate([{{opacity:1,transform:'scale(1)'}},{{opacity:0,transform:'scale(3)'}}],{{duration:{millis}}}); setTimeout(() => d.remove(), {millis}); }})()",x-size/2.0,y-size/2.0),true).await?;
        Ok(())
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
}
impl Search {
    async fn close(mut self) -> Result<()> {
        if let Some(id) = &self.id {
            self.session
                .execute_raw("DOM.discardSearchResults", json!({"searchId":id}))
                .await?;
        }
        self.id = None;
        Ok(())
    }
}
impl Drop for Search {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let session = self.session.clone();
            self.session.connection().runtime().spawn(async move {
                let _ = session
                    .execute_raw("DOM.discardSearchResults", json!({"searchId":id}))
                    .await;
            });
        }
    }
}

use super::{Page, dom_timeout};
use crate::{
    Error, Result, Session,
    cdp::{
        dom::Node,
        page::FrameId,
        runtime::{RemoteObject, RemoteObjectId},
    },
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
    /// Acquisition, including context setup and validation, has an owned 30-second
    /// limit; the handle deadline separately limits the caller's wait. Validation
    /// and release preserve a shorter handle deadline. Unknown acquisition outcomes
    /// detach the session, falling back to connection closure. Validation timeout
    /// with an owned node only schedules best-effort release: late exception handles
    /// whose IDs were not received cannot be individually released.
    pub async fn resolve_node(&self, node: &Node) -> Result<Element> {
        let timeout = self.session.connection().command_timeout();
        let page = self.clone();
        let backend_id = node.backend_node_id.clone();
        let task = self.session.connection().runtime().spawn(async move {
            let commands = page.clone().with_command_timeout(None);
            let mut unknown_resource = false;
            let result = tokio::time::timeout(Duration::from_secs(30), async {
                let context = commands.isolated_context().await?;
                unknown_resource = true;
                let response = match commands
                    .session
                    .execute_raw(
                        "DOM.resolveNode",
                        json!({
                            "backendNodeId": backend_id,
                            "executionContextId": context.numeric,
                        }),
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
                    !page.own_response_objects(&response, "object", false, &mut objects);
                if let Some(error) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                let element = node_result(&response["object"], &objects)?.ok_or_else(|| {
                    Error::InvalidArgument("DOM.resolveNode returned null".into())
                })?;
                // The node is now owned. Validation uses the original shorter
                // command deadline; owner expiry releases it without detachment.
                let validation = page
                    .session
                    .execute_raw(
                        "Runtime.callFunctionOn",
                        json!({
                            "uniqueContextId": context.unique,
                            "arguments": [{"objectId": element.object_id()}],
                            "functionDeclaration": "function(node) { return true; }",
                            "returnByValue": true,
                            "silent": true,
                        }),
                    )
                    .await?;
                // By-value results need no handle. Still own any IDs actually
                // received, deduplicating against the provisional node as well.
                unknown_resource =
                    !page.own_response_objects(&validation, "result", true, &mut objects);
                if let Some(error) = validation.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                if unknown_resource {
                    return Err(Error::InvalidArgument(
                        "invalid remote object ID in node validation".into(),
                    ));
                }
                if validation["result"]["type"].as_str() != Some("boolean")
                    || validation["result"]["value"].as_bool() != Some(true)
                    || validation["result"].get("objectId").is_some()
                {
                    return Err(Error::InvalidArgument(
                        "unexpected resolved node context validation result".into(),
                    ));
                }
                Ok(element)
            })
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("resolving DOM node".into())));
            if unknown_resource {
                page.recover_session().await;
            }
            drop(page);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for resolved DOM node".into()))??
        } else {
            task.await?
        }
    }

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

    pub(super) fn release_response_objects(&self, response: &Value) {
        let mut objects = Vec::new();
        self.own_response_objects(response, "result", true, &mut objects);
    }

    // Capture every usable ID before decoding other fields. Exception and result
    // may refer to the same object, so share one release owner per distinct ID.
    fn own_response_objects(
        &self,
        response: &Value,
        result_key: &str,
        result_by_value: bool,
        objects: &mut Vec<Element>,
    ) -> bool {
        let result = &response[result_key];
        let mut known = if result_by_value && result.get("objectId").is_none() {
            true
        } else {
            self.own_remote_object(result, objects)
        };
        if let Some(details) = response.get("exceptionDetails") {
            if let Some(exception) = details.get("exception") {
                known &= self.own_remote_object(exception, objects);
            } else if !details.is_object() {
                known = false;
            }
        }
        known
    }

    fn own_remote_object(&self, object: &Value, objects: &mut Vec<Element>) -> bool {
        if let Some(id) = object.get("objectId") {
            let Some(id) = id.as_str().filter(|id| !id.is_empty()) else {
                return false;
            };
            if !objects.iter().any(|owned| owned.object_id().0 == id) {
                objects.push(Element::from_object_id(
                    self.session.clone(),
                    RemoteObjectId(id.to_owned()),
                    self.context_lease.clone(),
                ));
            }
            return true;
        }
        // By-reference objects/functions/symbols require an ID. Only recognized
        // primitives and null conclusively contain no remote reference.
        serde_json::from_value::<RemoteObject>(object.clone()).is_ok()
            && (matches!(
                object["type"].as_str(),
                Some("undefined" | "string" | "number" | "boolean" | "bigint")
            ) || is_null_object(object))
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

    /// Attach to an out-of-process frame target owned by this page, not another tab.
    /// For same-process frames use [`Self::main_frame`] or [`Self::frames`].
    /// After target selection, an owned task retains the parent Page through attach
    /// and domain setup, even if the caller cancels or its handle timeout expires.
    /// The child restores that handle timeout. With `None`, the caller waits for
    /// setup while the independent attachment and domain-transition limits remain.
    /// The returned Page shares any fallback-context ownership with its parent.
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

fn is_null_object(object: &Value) -> bool {
    object["type"] == "object"
        && object["subtype"] == "null"
        && object.get("objectId").is_none()
        && object.get("value").is_none_or(Value::is_null)
}

fn node_result(object: &Value, objects: &[Element]) -> Result<Option<Element>> {
    let _: RemoteObject = serde_json::from_value(object.clone())?;
    if is_null_object(object) {
        return Ok(None);
    }
    if object["type"] == "object"
        && object["subtype"] == "node"
        && let Some(element) = objects
            .iter()
            .find(|owned| object["objectId"].as_str() == Some(owned.object_id().0.as_str()))
    {
        return Ok(Some(element.clone()));
    }
    Err(Error::InvalidArgument(
        "element acquisition did not return a node or null".into(),
    ))
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

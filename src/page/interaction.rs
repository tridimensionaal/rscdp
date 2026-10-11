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

#[derive(Debug, Clone)]
pub struct WaitOptions {
    pub timeout: Duration,
    pub polling_interval: Duration,
    pub stability_time: Duration,
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

/// An owned remote node reference, released when its last clone is dropped.
#[derive(Clone)]
pub struct Element(pub(super) Arc<RemoteNode>);

pub(super) struct RemoteNode {
    pub(super) session: Session,
    pub(super) id: RemoteObjectId,
}

impl Drop for RemoteNode {
    fn drop(&mut self) {
        let session = self.session.clone();
        let id = self.id.clone();
        self.session.connection().runtime().spawn(async move {
            let _ = session
                .execute_raw("Runtime.releaseObject", json!({"objectId":id}))
                .await;
        });
    }
}

impl Element {
    pub(super) fn from_object_id(session: Session, id: RemoteObjectId) -> Self {
        Self(Arc::new(RemoteNode { session, id }))
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
    pub async fn resolve_node(&self, node: &Node) -> Result<Element> {
        let page = self.clone();
        let backend_id = node.backend_node_id.clone();
        self.session
            .connection()
            .runtime()
            .spawn(async move {
                let context = page.create_isolated_world().await?;
                let response = page
                    .session
                    .execute_raw(
                        "DOM.resolveNode",
                        json!({"backendNodeId":backend_id,"executionContextId":context}),
                    )
                    .await?;
                let id: RemoteObjectId =
                    serde_json::from_value(response["object"]["objectId"].clone())?;
                Ok(Element(Arc::new(RemoteNode {
                    session: page.session.clone(),
                    id,
                })))
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
                    Element(Arc::new(RemoteNode {
                        session: page.session.clone(),
                        id,
                    }))
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
                    Err(Error::Protocol(error))
                        if error.code == -32000
                            && [
                                "execution context was destroyed",
                                "cannot find context",
                                "no frame for given id",
                                "frame with the given id was not found",
                                "inspected target navigated",
                            ]
                            .iter()
                            .any(|message| {
                                error.message.to_ascii_lowercase().contains(message)
                            }) =>
                    {
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
                        return Page::from_session(
                            connection.attach(target.target_id).await?,
                            self.context.clone(),
                        )
                        .await;
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

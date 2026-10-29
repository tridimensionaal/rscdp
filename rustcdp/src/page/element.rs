use super::{ElementCallBuilder, Page, input::click_cdp};
use crate::error::decode_ref;
use crate::{
    Error, Result, Session,
    cdp::{
        dom::{
            Node,
            commands::{GetBoxModelResponse, ResolveNodeResponse},
        },
        runtime::{RemoteObject, RemoteObjectId, commands::CallFunctionOnResponse},
    },
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy, Default)]
pub enum ClickMethod {
    #[default]
    Cdp,
    JavaScript,
}

/// Options for clicking an element. Defaults to CDP input with scrolling enabled.
#[derive(Debug, Clone, Copy)]
pub struct ClickOptions {
    /// Dispatch CDP mouse input or invoke the element's JavaScript `click()` method.
    pub method: ClickMethod,
    /// Scroll before clicking, including when using JavaScript.
    pub scroll_into_view: bool,
}

impl Default for ClickOptions {
    fn default() -> Self {
        Self {
            method: ClickMethod::Cdp,
            scroll_into_view: true,
        }
    }
}

/// Border-quad geometry in CSS pixels; this does not establish visibility or clickability.
#[derive(Debug, Clone, Copy)]
pub struct ElementPosition {
    /// X coordinate of the first border-quad vertex, not the minimum bounding-box X.
    pub x: f64,
    /// Y coordinate of the first border-quad vertex, not the minimum bounding-box Y.
    pub y: f64,
    /// Arithmetic mean of the four border-quad vertices' X coordinates.
    pub center_x: f64,
    /// Arithmetic mean of the four border-quad vertices' Y coordinates.
    pub center_y: f64,
}

/// An owned remote node reference. Dropping its last clone schedules a best-effort
/// release attempt, capped at 30 seconds while preserving a shorter handle
/// deadline. Drop does not guarantee that the remote release is acknowledged.
#[derive(Clone)]
pub struct Element(pub(super) Arc<RemoteObjectLease>);

pub(super) struct RemoteObjectLease {
    pub(super) session: Session,
    pub(super) id: RemoteObjectId,
    pub(super) context_lease: Option<Arc<crate::browser::ContextLease>>,
    released: bool,
}

impl RemoteObjectLease {
    fn release_task(&mut self) -> tokio::task::JoinHandle<Result<()>> {
        self.released = true;
        let session = self.session.clone();
        let id = self.id.clone();
        let context_lease = self.context_lease.take();
        self.session.connection().runtime().spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                session.execute_raw("Runtime.releaseObject", json!({"objectId":id})),
            )
            .await
            .map_err(|_| Error::Timeout("releasing remote object".into()))?
            .map(|_| ());
            drop(context_lease);
            result
        })
    }
}

impl Drop for RemoteObjectLease {
    fn drop(&mut self) {
        if !self.released {
            // The task owns the session and fallback context through release.
            drop(self.release_task());
        }
    }
}

impl Element {
    pub(super) fn from_object_id(
        session: Session,
        id: RemoteObjectId,
        context_lease: Option<Arc<crate::browser::ContextLease>>,
    ) -> Self {
        Self(Arc::new(RemoteObjectLease {
            session,
            id,
            context_lease,
            released: false,
        }))
    }

    /// Release this handle. If it is the last clone, await remote-object cleanup.
    /// Other clones remain usable and retain their shared reference. Once final
    /// cleanup starts, it continues if the caller cancels this wait. Release uses
    /// the handle deadline, capped at 30 seconds; failures are returned without
    /// retrying. Navigation or explicit context closure can already invalidate it.
    pub async fn release(self) -> Result<()> {
        match Arc::into_inner(self.0) {
            Some(mut lease) => lease.release_task().await?,
            None => Ok(()),
        }
    }

    /// Configure a function call with `this` bound to this existing element.
    ///
    /// Use a normal JavaScript function to access `this`; arrow functions keep
    /// their lexical `this`. No world selection or fresh element lookup occurs.
    /// See [`ElementCallBuilder`] for attachment and cancellation guarantees.
    ///
    /// ```no_run
    /// # async fn example(element: &rustcdp::page::Element) -> rustcdp::Result<()> {
    /// let value: Option<String> = element
    ///     .call("function(name) { return this.getAttribute(name); }")
    ///     .arg(serde_json::json!("data-value"))
    ///     .json().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn call(&self, function: impl Into<String>) -> ElementCallBuilder<'_> {
        ElementCallBuilder::new(self, function)
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
        let decoded: GetBoxModelResponse = decode_ref("DOM.getBoxModel", &result)?;
        let points = &decoded.model.border.0;
        if points.len() != 8 || points.iter().any(|p| !p.is_finite()) {
            return Err(Error::UnexpectedResponse {
                method: "DOM.getBoxModel".into(),
                payload: result,
            });
        }
        // Divide before summing so finite extreme coordinates do not overflow
        // merely while computing their arithmetic mean.
        let center_x = points.iter().step_by(2).map(|x| x / 4.0).sum::<f64>();
        let center_y = points
            .iter()
            .skip(1)
            .step_by(2)
            .map(|y| y / 4.0)
            .sum::<f64>();
        if !center_x.is_finite() || !center_y.is_finite() {
            return Err(Error::UnexpectedResponse {
                method: "DOM.getBoxModel".into(),
                payload: result,
            });
        }
        Ok(ElementPosition {
            x: points[0],
            y: points[1],
            center_x,
            center_y,
        })
    }

    pub async fn scroll_into_view(&self) -> Result<()> {
        self.0
            .session
            .execute_raw("DOM.scrollIntoViewIfNeeded", json!({"objectId":self.0.id}))
            .await?;
        Ok(())
    }

    /// Scroll into view and click the element's center using CDP mouse input.
    /// If a click should navigate, prepare [`Page::expect_navigation`] before
    /// clicking, then await the prepared observer. Click completion alone does
    /// not mean that navigation or application loading has finished.
    /// See [`Self::click_with_options`] for behavior and limitations.
    pub async fn click(&self) -> Result<()> {
        self.click_with_options(ClickOptions::default()).await
    }

    /// Click this element using the requested method and scrolling behavior.
    ///
    /// Scrolling completes before clicking; a scroll error prevents the click.
    /// CDP input targets the border-quad center; JavaScript invokes `this.click()`.
    /// This does not check actionability, retry actions, or wait for navigation.
    /// Cancellation does not undo input already dispatched. Once the CDP input
    /// pair starts, its release is attempted even after press failure or cancellation.
    /// Each input command has its own deadline capped at 30 seconds, preserving
    /// a shorter handle deadline. Received JavaScript result/exception object IDs
    /// are released best-effort; error details retain diagnostic IDs, not ownership.
    pub async fn click_with_options(&self, options: ClickOptions) -> Result<()> {
        if options.scroll_into_view {
            self.scroll_into_view().await?;
        }
        match options.method {
            ClickMethod::Cdp => {
                let position = self.position().await?;
                click_cdp(
                    &self.0.session,
                    self.0.context_lease.clone(),
                    position.center_x,
                    position.center_y,
                )
                .await
            }
            ClickMethod::JavaScript => {
                let result = self.0.session.execute_raw("Runtime.callFunctionOn", json!({"objectId":self.0.id,"functionDeclaration":"function() { this.click(); }","userGesture":true,"returnByValue":true})).await?;
                // Own received IDs before interpreting the response. Seed with
                // this handle so an aliased result/exception cannot release it.
                let mut objects = vec![self.clone()];
                for object in [&result["result"], &result["exceptionDetails"]["exception"]] {
                    own_remote_object(&self.0.session, &self.0.context_lease, object, &mut objects);
                }
                let decoded: CallFunctionOnResponse =
                    decode_ref("Runtime.callFunctionOn", &result)?;
                validate_exception("Runtime.callFunctionOn", &result)?;
                if let Some(error) = result.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                if decoded.result.type_.as_str() != "undefined"
                    || result["result"].get("objectId").is_some()
                {
                    return Err(Error::UnexpectedResponse {
                        method: "Runtime.callFunctionOn".into(),
                        payload: result,
                    });
                }
                Ok(())
            }
        }
    }
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
                let _: ResolveNodeResponse = decode_ref("DOM.resolveNode", &response)?;
                validate_exception("DOM.resolveNode", &response)?;
                if let Some(error) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                let element = node_result("DOM.resolveNode", &response, "object", &objects)?
                    .ok_or_else(|| Error::UnexpectedResponse {
                        method: "DOM.resolveNode".into(),
                        payload: response.clone(),
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
                let _: CallFunctionOnResponse = decode_ref("Runtime.callFunctionOn", &validation)?;
                validate_exception("Runtime.callFunctionOn", &validation)?;
                if let Some(error) = validation.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                if unknown_resource {
                    return Err(Error::UnexpectedResponse {
                        method: "Runtime.callFunctionOn".into(),
                        payload: validation,
                    });
                }
                if validation["result"]["type"].as_str() != Some("boolean")
                    || validation["result"]["value"].as_bool() != Some(true)
                    || validation["result"].get("objectId").is_some()
                {
                    return Err(Error::UnexpectedResponse {
                        method: "Runtime.callFunctionOn".into(),
                        payload: validation,
                    });
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

    // Capture every usable ID before decoding other fields. Exception and result
    // may refer to the same object, so share one release owner per distinct ID.
    pub(super) fn own_response_objects(
        &self,
        response: &Value,
        result_key: &str,
        result_by_value: bool,
        objects: &mut Vec<Element>,
    ) -> bool {
        own_response_objects(
            &self.session,
            &self.context_lease,
            response,
            result_key,
            result_by_value,
            objects,
        )
    }
}

// Shared by page acquisitions and calls bound to an already-owned element.
pub(super) fn own_response_objects(
    session: &Session,
    context_lease: &Option<Arc<crate::browser::ContextLease>>,
    response: &Value,
    result_key: &str,
    result_by_value: bool,
    objects: &mut Vec<Element>,
) -> bool {
    let result = &response[result_key];
    let mut known = if result_by_value && result.get("objectId").is_none() {
        true
    } else {
        own_remote_object(session, context_lease, result, objects)
    };
    if let Some(details) = response.get("exceptionDetails") {
        if let Some(exception) = details.get("exception") {
            known &= own_remote_object(session, context_lease, exception, objects);
        } else if !details.is_object() {
            known = false;
        }
    }
    known
}

pub(super) fn own_remote_object(
    session: &Session,
    context_lease: &Option<Arc<crate::browser::ContextLease>>,
    object: &Value,
    objects: &mut Vec<Element>,
) -> bool {
    if let Some(id) = object.get("objectId") {
        let Some(id) = id.as_str().filter(|id| !id.is_empty()) else {
            return false;
        };
        if !objects.iter().any(|owned| owned.object_id().0 == id) {
            objects.push(Element::from_object_id(
                session.clone(),
                RemoteObjectId(id.to_owned()),
                context_lease.clone(),
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

fn is_null_object(object: &Value) -> bool {
    object["type"] == "object"
        && object["subtype"] == "null"
        && object.get("objectId").is_none()
        && object.get("value").is_none_or(Value::is_null)
}

pub(super) fn node_result(
    method: &str,
    response: &Value,
    result_key: &str,
    objects: &[Element],
) -> Result<Option<Element>> {
    let object = &response[result_key];
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
    Err(Error::UnexpectedResponse {
        method: method.into(),
        payload: response.clone(),
    })
}

pub(super) fn validate_exception(method: &str, response: &Value) -> Result<()> {
    // Optional serde fields accept null; a present exception must still be an object.
    if response
        .get("exceptionDetails")
        .is_some_and(|details| !details.is_object())
    {
        return Err(Error::UnexpectedResponse {
            method: method.into(),
            payload: response.clone(),
        });
    }
    if let Some(details) = response.get("exceptionDetails") {
        let _: crate::cdp::runtime::ExceptionDetails = serde::Deserialize::deserialize(details)
            .map_err(|source| Error::Decode {
                method: method.into(),
                payload: response.clone(),
                source,
            })?;
    }
    Ok(())
}

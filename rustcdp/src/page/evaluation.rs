use super::{
    Element, Page,
    element::{own_response_objects, validate_exception},
};
use crate::{
    Error, Result, Session,
    cdp::{
        page::FrameId,
        runtime::{
            RemoteObject, RemoteObjectId,
            commands::{CallFunctionOnResponse, EvaluateResponse},
        },
    },
    error::{decode, decode_ref, with_cleanup},
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

const EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);

/// The JavaScript world in which user source executes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EvaluationWorld {
    /// The document's default world, including its page-defined globals.
    #[default]
    Main,
    /// The library's private world, sharing the DOM but not page globals.
    Isolated,
}

/// Configure evaluation, then choose JSON, discarded, or remote results.
///
/// Source is sent unchanged and its resulting promise is awaited. This does not
/// enable REPL mode or implicitly wrap source in an async function. User source
/// is never retried, including when navigation invalidates its unique context.
///
/// Once a terminal method is polled, an owned task retains the Page through
/// context resolution and evaluation, capped independently at 30 seconds.
/// The session's command timeout separately bounds the caller's wait. Canceling
/// that wait leaves execution and cleanup running; it does not undo side effects.
/// Cleanup and uncertain-acquisition recovery can outlast the execution cap.
/// If a dispatched acquisition has no conclusive outcome, recovery detaches the
/// session, falling back to connection closure. Known handles receive bounded
/// release attempts; cleanup is not guaranteed after runtime shutdown.
#[must_use = "call json().await, run().await, or remote().await to evaluate"]
pub struct EvaluationBuilder<'a> {
    page: &'a Page,
    expression: String,
    frame: Option<FrameId>,
    world: EvaluationWorld,
}

/// Evaluation metadata with ownership of any returned remote reference.
///
/// Primitive results may have no object ID. Dropping this value schedules
/// best-effort release; use [`Self::release`] to observe cleanup errors. This
/// value is intentionally not Clone. Navigation, explicit context disposal,
/// or browser closure can invalidate its reference despite retained ownership.
pub struct RemoteValue {
    metadata: RemoteObject,
    resource: Option<Element>,
}

impl RemoteValue {
    pub(super) fn into_json<T: DeserializeOwned>(mut self, method: &str) -> Result<T> {
        let value = self
            .metadata
            .value
            .take()
            .ok_or_else(|| Error::NonJsonValue(Box::new(self.metadata)))?;
        decode(method, value)
    }

    /// Inspect metadata. Copying an ID does not transfer or duplicate ownership.
    pub fn metadata(&self) -> &RemoteObject {
        &self.metadata
    }

    /// Borrow the owned reference's ID, if this is a reference-bearing result.
    pub fn object_id(&self) -> Option<&RemoteObjectId> {
        self.resource.as_ref().map(Element::object_id)
    }

    /// Consume the value and await release of its remote reference, if any.
    ///
    /// Once release starts, cancellation leaves cleanup running. Release follows
    /// the originating handle's deadline, capped at 30 seconds, without retries.
    pub async fn release(self) -> Result<()> {
        match self.resource {
            Some(resource) => resource.release().await,
            None => Ok(()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum ResultMode {
    Json,
    Discard,
    Remote,
}

impl<'a> EvaluationBuilder<'a> {
    /// Construct a request scoped to a same-process frame.
    pub(super) fn for_frame(page: &'a Page, frame: FrameId, expression: impl Into<String>) -> Self {
        Self {
            page,
            expression: expression.into(),
            frame: Some(frame),
            world: EvaluationWorld::default(),
        }
    }

    /// Select the execution world. Defaults to [`EvaluationWorld::Main`].
    pub fn world(mut self, world: EvaluationWorld) -> Self {
        self.world = world;
        self
    }

    /// Await evaluation and deserialize its by-value JSON representation.
    ///
    /// `undefined`, BigInt, and other results without a JSON value produce
    /// [`Error::NonJsonValue`]; they are not converted to JSON null. Its metadata
    /// is diagnostic: any embedded object IDs have already been released.
    /// JavaScript exceptions and promise rejections remain errors. Serialization
    /// or decoding can fail after source has performed side effects; do not retry
    /// blindly. Use `json::<serde_json::Value>()` for an untyped JSON result.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        self.execute(ResultMode::Json)
            .await?
            .into_json("Runtime.evaluate JSON value")
    }

    /// Await evaluation and discard its result, including `undefined`.
    ///
    /// This accepts void scripts without pretending their result is JSON null.
    /// Results are requested by reference to avoid unnecessary serialization;
    /// any received references are released before successful completion.
    /// Exceptions and promise rejections are propagated, not discarded.
    pub async fn run(self) -> Result<()> {
        self.execute(ResultMode::Discard).await.map(|_| ())
    }

    /// Await evaluation by reference and return owned result metadata.
    ///
    /// Non-JSON primitives are valid results. An unclaimed result is dropped and
    /// released if the caller cancels while the owned task is still executing.
    pub async fn remote(self) -> Result<RemoteValue> {
        self.execute(ResultMode::Remote).await
    }

    async fn execute(self, mode: ResultMode) -> Result<RemoteValue> {
        ExecutionRequest::Expression(self.expression)
            .execute(self.page, self.frame, self.world, mode)
            .await
    }
}

// All requests acquire results through the same owned task. Keep dispatch and
// typed decoding specific to each protocol command, and ownership shared.
pub(super) enum ExecutionRequest {
    Expression(String),
    Function { source: String, args: Vec<Value> },
}

// An element keeps its original remote ID and context. Only page/frame targets
// resolve a current world; there is deliberately no element world override.
enum ExecutionTarget {
    Page {
        page: Box<Page>,
        frame: Option<FrameId>,
        world: EvaluationWorld,
    },
    Element(Element),
}

impl ExecutionTarget {
    fn session(&self) -> &Session {
        match self {
            Self::Page { page, .. } => &page.session,
            Self::Element(element) => &element.0.session,
        }
    }

    fn context_lease(&self) -> &Option<std::sync::Arc<crate::browser::ContextLease>> {
        match self {
            Self::Page { page, .. } => &page.context_lease,
            Self::Element(element) => &element.0.context_lease,
        }
    }

    async fn binding(&self) -> Result<Value> {
        match self {
            Self::Element(element) => Ok(json!({"objectId": element.object_id()})),
            Self::Page { page, frame, world } => {
                let commands = page.as_ref().clone().with_command_timeout(None);
                let context = match (world, frame.as_ref()) {
                    (EvaluationWorld::Main, None) => commands.main_context().await?,
                    (EvaluationWorld::Main, Some(frame)) => {
                        commands.main_context_for(frame).await?
                    }
                    (EvaluationWorld::Isolated, None) => commands.isolated_context().await?,
                    (EvaluationWorld::Isolated, Some(frame)) => {
                        commands.isolated_context_for(frame).await?
                    }
                };
                Ok(json!({"uniqueContextId": context.unique}))
            }
        }
    }
}

impl ExecutionRequest {
    fn method(&self) -> &'static str {
        match self {
            Self::Expression(_) => "Runtime.evaluate",
            Self::Function { .. } => "Runtime.callFunctionOn",
        }
    }

    fn params(&self, binding: &Value, by_value: bool) -> Value {
        let mut params = binding.clone();
        params["silent"] = json!(true);
        params["returnByValue"] = json!(by_value);
        params["awaitPromise"] = json!(true);
        match self {
            Self::Expression(expression) => params["expression"] = json!(expression),
            Self::Function { source, args } => {
                params["functionDeclaration"] = json!(source);
                // Each value is a separate CDP CallArgument, including explicit
                // JSON null. Never interpolate arguments into user source.
                params["arguments"] = args.iter().map(|value| json!({"value": value})).collect();
            }
        }
        params
    }

    fn decode_result(&self, response: &Value) -> Result<RemoteObject> {
        match self {
            Self::Expression(_) => {
                let decoded: EvaluateResponse = decode_ref(self.method(), response)?;
                Ok(decoded.result)
            }
            Self::Function { .. } => {
                let decoded: CallFunctionOnResponse = decode_ref(self.method(), response)?;
                Ok(decoded.result)
            }
        }
    }

    pub(super) async fn execute(
        self,
        page: &Page,
        frame: Option<FrameId>,
        world: EvaluationWorld,
        mode: ResultMode,
    ) -> Result<RemoteValue> {
        self.execute_on(
            ExecutionTarget::Page {
                page: Box::new(page.clone()),
                frame,
                world,
            },
            mode,
            None,
        )
        .await
    }

    pub(super) async fn execute_element(
        self,
        element: &Element,
        mode: ResultMode,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<RemoteValue> {
        self.execute_on(ExecutionTarget::Element(element.clone()), mode, deadline)
            .await
    }

    // Own every received ID before decoding, including aliases of a seeded
    // element. The command receiver remains alive through caller cancellation.
    async fn dispatch(
        &self,
        target: &ExecutionTarget,
        binding: &Value,
        mode: ResultMode,
        objects: &mut Vec<Element>,
        unknown_resource: &mut bool,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<RemoteValue> {
        let method = self.method();
        let by_value = matches!(mode, ResultMode::Json);
        let commands = target.session().clone().with_command_timeout(None);
        // Admission sets this flag synchronously with queueing. A deadline
        // before admission cannot have created remote references.
        *unknown_resource = false;
        let response = match commands
            .execute_raw_tracked(
                method,
                self.params(binding, by_value),
                deadline,
                unknown_resource,
            )
            .await
        {
            Ok(response) => response,
            Err(error @ Error::Protocol(_)) => {
                *unknown_resource = false;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        *unknown_resource = !own_response_objects(
            target.session(),
            target.context_lease(),
            &response,
            "result",
            by_value,
            objects,
        );
        let metadata = self.decode_result(&response)?;
        validate_exception(method, &response)?;
        if let Some(details) = response.get("exceptionDetails") {
            return Err(Error::JavaScript(details.clone()));
        }
        if *unknown_resource {
            return Err(Error::UnexpectedResponse {
                method: method.into(),
                payload: response,
            });
        }
        let resource = if matches!(mode, ResultMode::Remote) {
            match metadata.object_id.as_ref() {
                Some(id) => {
                    let index = objects
                        .iter()
                        .position(|object| object.object_id() == id)
                        .ok_or_else(|| Error::UnexpectedResponse {
                            method: method.into(),
                            payload: response.clone(),
                        })?;
                    Some(objects.swap_remove(index))
                }
                None => None,
            }
        } else {
            None
        };
        Ok(RemoteValue { metadata, resource })
    }

    async fn execute_on(
        self,
        target: ExecutionTarget,
        mode: ResultMode,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<RemoteValue> {
        let timeout = target.session().connection().command_timeout();
        let runtime = target.session().connection().runtime();
        let task = runtime.spawn(async move {
            // Seed ownership before *any* response, including attachment checks.
            // Keep owners outside the timed future for awaited cleanup on expiry.
            let mut objects = match &target {
                ExecutionTarget::Element(element) => vec![element.clone()],
                ExecutionTarget::Page { .. } => Vec::new(),
            };
            let mut unknown_resource = false;
            let result = tokio::time::timeout(EXECUTION_TIMEOUT, async {
                check_execution_deadline(deadline)?;
                let binding = target.binding().await?;
                if matches!(&target, ExecutionTarget::Element(_)) {
                    let check = Self::Function {
                        source: "function() { return this.isConnected; }".into(),
                        args: Vec::new(),
                    };
                    let attached: bool = check
                        .dispatch(
                            &target,
                            &binding,
                            ResultMode::Json,
                            &mut objects,
                            &mut unknown_resource,
                            deadline,
                        )
                        .await?
                        .into_json("Runtime.callFunctionOn element attachment")?;
                    if !attached {
                        return Err(Error::InvalidArgument(
                            "cannot call a detached element".into(),
                        ));
                    }
                }
                // Check inside the owner, after attachment validation, before
                // submitting user code. An expired operation cannot start new user code.
                check_execution_deadline(deadline)?;
                self.dispatch(
                    &target,
                    &binding,
                    mode,
                    &mut objects,
                    &mut unknown_resource,
                    deadline,
                )
                .await
            })
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("evaluating JavaScript".into())));
            if unknown_resource {
                super::recover_session(target.session()).await;
            }
            let mut cleanup = Ok(());
            for object in objects {
                cleanup = with_cleanup(cleanup, object.release().await);
            }
            let result = with_cleanup(result, cleanup);
            // The target keeps an element alive through other-reference cleanup.
            // Await its own release too if cancellation dropped all public owners.
            match target {
                ExecutionTarget::Element(element) => with_cleanup(result, element.release().await),
                ExecutionTarget::Page { page, .. } => {
                    drop(page);
                    result
                }
            }
        });
        let result = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for JavaScript evaluation".into()))??
        } else {
            task.await?
        };
        if result.is_ok() {
            check_execution_deadline(deadline)?;
        }
        result
    }
}

fn check_execution_deadline(deadline: Option<tokio::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        Err(Error::Timeout("calling element function".into()))
    } else {
        Ok(())
    }
}

impl Page {
    /// Configure evaluation in this page's main world.
    ///
    /// ```no_run
    /// # async fn example(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// let title: String = page.evaluate("document.title").json().await?;
    /// page.evaluate("console.log('title read')").run().await?;
    /// let object = page.evaluate("({ title: document.title })").remote().await?;
    /// println!("{title}: {:?}", object.object_id());
    /// object.release().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn evaluate(&self, expression: impl Into<String>) -> EvaluationBuilder<'_> {
        EvaluationBuilder {
            page: self,
            expression: expression.into(),
            frame: None,
            world: EvaluationWorld::default(),
        }
    }
}

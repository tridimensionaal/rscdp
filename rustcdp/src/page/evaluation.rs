use super::{Element, Page};
use crate::{
    Error, Result,
    cdp::{
        page::FrameId,
        runtime::{RemoteObject, RemoteObjectId},
    },
    error::{decode, with_cleanup},
};
use serde::de::DeserializeOwned;
use serde_json::json;
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
enum ResultMode {
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
        let mut result = self.execute(ResultMode::Json).await?;
        let value = result
            .metadata
            .value
            .take()
            .ok_or_else(|| Error::NonJsonValue(Box::new(result.metadata)))?;
        decode("Runtime.evaluate JSON value", value)
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
        let timeout = self.page.session.connection().command_timeout();
        let page = self.page.clone();
        let expression = self.expression;
        let frame = self.frame;
        let world = self.world;
        let task = self.page.session.connection().runtime().spawn(async move {
            let commands = page.clone().with_command_timeout(None);
            // Keep provisional owners outside the timed future so cancellation
            // of that future still permits awaited cleanup of received handles.
            let mut objects = Vec::new();
            let mut unknown_resource = false;
            let result = tokio::time::timeout(EXECUTION_TIMEOUT, async {
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
                let by_value = matches!(mode, ResultMode::Json);
                // Conservatively uncertain once dispatch can begin. A caller's
                // deadline never drops this command's receiver; the owner has
                // its own bound and recovers if that bound expires here.
                unknown_resource = true;
                let response = match commands
                    .session
                    .execute_raw(
                        "Runtime.evaluate",
                        json!({
                            "expression": expression,
                            "uniqueContextId": context.unique,
                            "silent": true,
                            "returnByValue": by_value,
                            "awaitPromise": true,
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
                unknown_resource =
                    !page.own_response_objects(&response, "result", by_value, &mut objects);
                if let Some(details) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(details.clone()));
                }
                let metadata: RemoteObject =
                    decode("Runtime.evaluate result", response["result"].clone())?;
                if unknown_resource {
                    return Err(Error::InvalidArgument(
                        "invalid remote evaluation result identity".into(),
                    ));
                }
                let resource = if matches!(mode, ResultMode::Remote) {
                    match metadata.object_id.as_ref() {
                        Some(id) => {
                            let index = objects
                                .iter()
                                .position(|object| object.object_id() == id)
                                .ok_or_else(|| {
                                    Error::InvalidArgument(
                                        "unowned remote evaluation result".into(),
                                    )
                                })?;
                            Some(objects.swap_remove(index))
                        }
                        None => None,
                    }
                } else {
                    None
                };
                Ok(RemoteValue { metadata, resource })
            })
            .await
            .unwrap_or_else(|_| Err(Error::Timeout("evaluating JavaScript".into())));
            if unknown_resource {
                page.recover_session().await;
            }
            let mut cleanup = Ok(());
            for object in objects {
                cleanup = with_cleanup(cleanup, object.release().await);
            }
            let result = with_cleanup(result, cleanup);
            drop(page);
            result
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("waiting for JavaScript evaluation".into()))??
        } else {
            task.await?
        }
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

use super::{
    Element, EvaluationWorld, Page,
    evaluation::{ExecutionRequest, ResultMode},
};
use crate::{Result, cdp::page::FrameId};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Configure a function call with JSON arguments, then choose JSON or discarded results.
///
/// Function source is sent unchanged to `Runtime.callFunctionOn`, separately
/// from its arguments, and returned promises are awaited. Supply a JavaScript
/// function (for example, `function(value) { return value; }` or `value => value`),
/// not an expression to evaluate. Construction and configuration send no commands.
/// Main world is the default. Each call resolves the page/frame's current unique
/// context; user source is never retried, even after navigation or context loss.
///
/// This uses the same owned execution and cleanup as [`super::EvaluationBuilder`].
/// Once a terminal method is polled, an owned task retains the Page through
/// context resolution and execution, capped independently at 30 seconds. The
/// handle's command timeout separately bounds the caller's wait. Cancellation
/// leaves execution and cleanup running and does not undo side effects. Cleanup
/// and uncertain-acquisition recovery can outlast that cap. Uncertain acquisition
/// detaches the session, falling back to connection closure; known references
/// receive bounded release attempts. Cleanup needs a running Tokio runtime.
#[must_use = "call json().await or run().await to invoke the function"]
pub struct FunctionCallBuilder<'a> {
    page: &'a Page,
    function: String,
    args: Vec<Value>,
    frame: Option<FrameId>,
    world: EvaluationWorld,
}

impl<'a> FunctionCallBuilder<'a> {
    pub(super) fn for_frame(page: &'a Page, frame: FrameId, function: impl Into<String>) -> Self {
        Self {
            frame: Some(frame),
            ..Self::new(page, function)
        }
    }

    fn new(page: &'a Page, function: impl Into<String>) -> Self {
        Self {
            page,
            function: function.into(),
            args: Vec::new(),
            frame: None,
            world: EvaluationWorld::default(),
        }
    }

    /// Append one JSON argument, preserving its position and explicit null values.
    /// Use `serde_json::json!` or `serde_json::to_value(value)?` for Rust values.
    /// Arguments use JavaScript's JSON-compatible values and number precision;
    /// remote references and non-JSON values are not supported.
    pub fn arg(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    /// Append JSON arguments in iteration order, preserving earlier arguments.
    pub fn args(mut self, values: impl IntoIterator<Item = Value>) -> Self {
        self.args.extend(values);
        self
    }

    /// Select the execution world. Defaults to [`EvaluationWorld::Main`].
    pub fn world(mut self, world: EvaluationWorld) -> Self {
        self.world = world;
        self
    }

    /// Await the function and deserialize its by-value JSON result.
    ///
    /// `undefined`, BigInt, and other results without a JSON value produce
    /// [`crate::Error::NonJsonValue`], not JSON null. JavaScript exceptions and
    /// promise rejections remain errors. Diagnostic object IDs are already
    /// released. Serialization or decoding can fail after side effects, so do
    /// not retry blindly. Use `json::<serde_json::Value>()` for an untyped result.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        ExecutionRequest::Function {
            source: self.function,
            args: self.args,
        }
        .execute(self.page, self.frame, self.world, ResultMode::Json)
        .await?
        .into_json("Runtime.callFunctionOn JSON value")
    }

    /// Await the function and discard its result, including `undefined`.
    ///
    /// Results are requested by reference to avoid unnecessary serialization.
    /// Received result/exception references are released before successful
    /// completion. Exceptions and promise rejections are propagated.
    pub async fn run(self) -> Result<()> {
        ExecutionRequest::Function {
            source: self.function,
            args: self.args,
        }
        .execute(self.page, self.frame, self.world, ResultMode::Discard)
        .await
        .map(|_| ())
    }
}

impl Page {
    /// Configure a function call with JSON arguments in this page's main world.
    /// See [`FunctionCallBuilder`] for ownership and cancellation guarantees.
    ///
    /// ```no_run
    /// # async fn example(page: &rscdp::Page) -> rscdp::Result<()> {
    /// let total: u64 = page.call("(a, b) => a + b")
    ///     .args([serde_json::json!(2), serde_json::json!(3)])
    ///     .json().await?;
    /// assert_eq!(total, 5);
    /// page.call("title => { document.title = title; }")
    ///     .arg(serde_json::json!("A quoted 'title' 🦀"))
    ///     .run().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn call(&self, function: impl Into<String>) -> FunctionCallBuilder<'_> {
        FunctionCallBuilder::new(self, function)
    }
}

/// Configure a call bound to an element's existing remote object and world.
///
/// Arguments are sent separately as JSON values and returned promises are awaited.
/// Use a normal function for `this`; arrow functions retain lexical `this`.
/// Construction sends no commands. There is no world setter or element lookup.
/// A detached node is rejected before user source is dispatched. Attachment is
/// checked in a separate command, so DOM mutation can still race the user call.
/// A destroyed context or stale object ID produces a protocol error.
/// User source is never retried, including after a failed attachment check.
///
/// An owned task retains the element through execution and cleanup, even if the
/// caller cancels or drops its last public handle. Returning or throwing `this`
/// cannot release a still-owned element. Execution is capped at 30 seconds; the
/// handle's command timeout separately bounds the caller's wait. Cancellation
/// does not undo side effects. Cleanup and uncertain-acquisition recovery can
/// outlast that cap. As with [`FunctionCallBuilder`], uncertain acquisition
/// detaches the session, falling back to connection closure; known references
/// receive bounded release attempts. Cleanup needs a running Tokio runtime.
#[must_use = "call json().await or run().await to invoke the function"]
pub struct ElementCallBuilder<'a> {
    element: &'a Element,
    function: String,
    args: Vec<Value>,
    deadline: Option<tokio::time::Instant>,
}

impl<'a> ElementCallBuilder<'a> {
    pub(super) fn new(element: &'a Element, function: impl Into<String>) -> Self {
        Self {
            element,
            function: function.into(),
            args: Vec::new(),
            deadline: None,
        }
    }

    pub(super) fn deadline(mut self, deadline: Option<tokio::time::Instant>) -> Self {
        self.deadline = deadline;
        self
    }

    /// Append one JSON argument, preserving its position and explicit null values.
    /// Use `serde_json::json!` or `serde_json::to_value(value)?` for Rust values.
    /// Arguments use JavaScript's JSON-compatible values and number precision;
    /// remote references and non-JSON values are not supported.
    pub fn arg(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    /// Append JSON arguments in iteration order, preserving earlier arguments.
    pub fn args(mut self, values: impl IntoIterator<Item = Value>) -> Self {
        self.args.extend(values);
        self
    }

    /// Await the function and deserialize its by-value JSON result.
    ///
    /// Results without JSON values, including `undefined` and BigInt, produce
    /// [`crate::Error::NonJsonValue`]. Exceptions and promise rejections remain
    /// errors. Received diagnostic IDs do not transfer ownership to the caller.
    /// Serialization or decoding can fail after side effects; do not retry blindly.
    pub async fn json<T: DeserializeOwned>(self) -> Result<T> {
        ExecutionRequest::Function {
            source: self.function,
            args: self.args,
        }
        .execute_element(self.element, ResultMode::Json, self.deadline)
        .await?
        .into_json("Runtime.callFunctionOn JSON value")
    }

    /// Await the function and discard its result, including `undefined`.
    ///
    /// Results are requested by reference. Newly received result/exception IDs
    /// are released before successful completion; aliases of the original element
    /// share its ownership. Exceptions and promise rejections are propagated.
    pub async fn run(self) -> Result<()> {
        ExecutionRequest::Function {
            source: self.function,
            args: self.args,
        }
        .execute_element(self.element, ResultMode::Discard, self.deadline)
        .await
        .map(|_| ())
    }
}

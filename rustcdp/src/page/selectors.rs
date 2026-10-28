use super::{
    Element, Page,
    element::{node_result, validate_exception},
};
use crate::{
    Error, Result,
    cdp::{page::FrameId, runtime::commands::EvaluateResponse},
    error::decode_ref,
};
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

/// Options for [`Page::wait_for_selector_count_stable`] and [`Page::wait_for_xpath_count_stable`].
///
/// Stability means the sampled number of matching elements stays nonzero and
/// unchanged. It does not track element identity, visibility, or position.
#[derive(Debug, Clone)]
pub struct CountWaitOptions {
    /// Deadline for observing a stable count. Defaults to 5 seconds.
    ///
    /// Expiration returns [`Error::Timeout`]. Success has no additional delay.
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
}

impl Default for CountWaitOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            polling_interval: Duration::from_millis(100),
            stability_time: Duration::from_secs(2),
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

    /// Find the first CSS match without polling, or return [`Error::ElementNotFound`]
    /// if absent. Invalid selectors and acquisition errors propagate unchanged.
    /// Uses the owned acquisition and cancellation policy of [`Self::element_by_selector`].
    pub async fn require_element(&self, selector: &str) -> Result<Element> {
        self.element_by_selector(selector)
            .await?
            .ok_or_else(|| Error::ElementNotFound(selector.to_owned()))
    }

    /// Wait for the first present CSS match and return its owned handle.
    /// Polls immediately, then 100 milliseconds after each unsuccessful acquisition.
    /// Only recognized navigation/context-change errors are retried; other errors
    /// propagate. The timeout includes polling and acquisition and expires with
    /// [`Error::Timeout`]; a shorter command timeout can fail an acquisition earlier.
    ///
    /// This checks presence only, not visibility, stability, or actionability.
    /// For application readiness, wait for a selector the page publishes when
    /// ready, such as `#results[data-ready=true]`. A matching element can still
    /// be hidden, moving, or covered; this is not an automatic click-readiness check.
    /// The element can become stale after acquisition. Cancellation or expiration
    /// stops polling but leaves an in-flight owned acquisition running under its
    /// independent 30-second limit and recovery/cleanup bounds; see
    /// [`Self::element_by_selector`].
    ///
    /// ```no_run
    /// # async fn focus_customer(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// use std::time::Duration;
    ///
    /// let customer = page
    ///     .wait_for_element("input[name=customer]", Duration::from_secs(5))
    ///     .await?;
    /// customer.focus().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn wait_for_element(&self, selector: &str, timeout: Duration) -> Result<Element> {
        let expression = format!(
            "document.querySelector({})",
            serde_json::to_string(selector)?
        );
        self.wait_for_expression_in_frame(expression, selector, timeout, None)
            .await
    }

    pub(super) async fn wait_for_expression_in_frame(
        &self,
        expression: String,
        query: &str,
        timeout: Duration,
        frame: Option<FrameId>,
    ) -> Result<Element> {
        tokio::time::timeout(timeout, async {
            loop {
                match self
                    .element_for_expression_in_frame(expression.clone(), frame.clone())
                    .await
                {
                    Ok(Some(element)) => return Ok(element),
                    Ok(None) => {}
                    Err(error) if is_context_change(&error) => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| Error::Timeout(format!("waiting for element: {query}")))?
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
                let _: EvaluateResponse = decode_ref("Runtime.evaluate", &response)?;
                validate_exception("Runtime.evaluate", &response)?;
                if let Some(error) = response.get("exceptionDetails") {
                    return Err(Error::JavaScript(error.clone()));
                }
                node_result("Runtime.evaluate", &response, "result", &objects)
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

    /// Find the first XPath node without polling, or return [`Error::ElementNotFound`]
    /// if absent. Invalid XPath and acquisition errors propagate unchanged.
    /// Uses the owned acquisition and cancellation policy of [`Self::element_by_xpath`].
    pub async fn require_xpath_element(&self, xpath: &str) -> Result<Element> {
        self.element_by_xpath(xpath)
            .await?
            .ok_or_else(|| Error::ElementNotFound(xpath.to_owned()))
    }

    /// Wait for the first present XPath node and return its owned handle.
    /// Uses the polling, timeout, context-change retry, and cancellation policy
    /// of [`Self::wait_for_element`]. Invalid XPath and other errors propagate.
    /// This checks presence only, not visibility, stability, or actionability;
    /// XPath may select non-element nodes, as in [`Self::element_by_xpath`].
    pub async fn wait_for_xpath_element(&self, xpath: &str, timeout: Duration) -> Result<Element> {
        let expression = format!(
            "document.evaluate({}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue",
            serde_json::to_string(xpath)?
        );
        self.wait_for_expression_in_frame(expression, xpath, timeout, None)
            .await
    }

    /// Wait for a stable, nonzero XPath match count, as in [`Self::wait_for_selector_count_stable`].
    pub async fn wait_for_xpath_count_stable(
        &self,
        xpath: &str,
        options: CountWaitOptions,
    ) -> Result<()> {
        let xpath = serde_json::to_string(xpath)?;
        let expression = format!(
            "document.evaluate({xpath}, document, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null).snapshotLength"
        );
        self.wait_for_count(&expression, options).await
    }

    /// Wait for a sampled, stable, nonzero number of CSS matches.
    /// Expiration returns [`Error::Timeout`]. Replacing nodes without changing
    /// their count still satisfies this condition; it does not imply readiness.
    /// Polling retries recognized navigation/context-loss errors and resets the
    /// stability interval; other errors propagate. See [`CountWaitOptions`] for sampling.
    /// A timeout stops polling but can leave an owned evaluation finishing in
    /// the background. If its remote outcome stays unknown, recovery may detach
    /// the session; see [`Page::evaluate`] for execution and cleanup bounds.
    pub async fn wait_for_selector_count_stable(
        &self,
        selector: &str,
        options: CountWaitOptions,
    ) -> Result<()> {
        let selector = serde_json::to_string(selector)?;
        self.wait_for_count(
            &format!("document.querySelectorAll({selector}).length"),
            options,
        )
        .await
    }

    async fn wait_for_count(&self, expression: &str, options: CountWaitOptions) -> Result<()> {
        if options.polling_interval.is_zero() {
            return Err(Error::InvalidArgument(
                "polling interval must be nonzero".into(),
            ));
        }
        tokio::time::timeout(options.timeout, async {
            let mut previous = 0;
            let mut stable = Instant::now();
            loop {
                let count = match self
                    .evaluate(expression)
                    .world(super::EvaluationWorld::Isolated)
                    .json::<serde_json::Value>()
                    .await
                {
                    Ok(value) => value.as_u64().ok_or_else(|| Error::UnexpectedResponse {
                        method: "Runtime.evaluate JSON value".into(),
                        payload: value,
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
        .await
        .map_err(|_| Error::Timeout("waiting for a stable nonzero selector count".into()))?
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

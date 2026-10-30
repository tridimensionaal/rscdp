use super::{Element, EvaluationWorld, Frame, Page, selectors::is_context_change};
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

mod action;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const POLLING_INTERVAL: Duration = Duration::from_millis(100);

// Resolve and inspect in one synchronous call. Only JSON crosses the protocol;
// no node reference is retained between samples. Arguments are separate from
// source, and the isolated world avoids page-defined query/style overrides.
const SAMPLE: &str = r#"(selector, checkVisibility) => {
    const matches = document.querySelectorAll(selector);
    const count = matches.length;
    let visible = false;
    if (count === 1 && checkVisibility) {
        const element = matches[0];
        const visibility = getComputedStyle(element).visibility;
        visible = element.isConnected
            && visibility !== 'hidden' && visibility !== 'collapse'
            && Array.from(element.getClientRects()).some(rect => rect.width > 0 && rect.height > 0);
    }
    return { count, visible };
}"#;

/// The sampled DOM state required by [`Locator::wait_for`].
/// More than one matching element is an ambiguity error for every state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementState {
    /// Exactly one element matches, regardless of visibility.
    Attached,
    /// No element matches in the existing page/frame document.
    Detached,
    /// Exactly one connected element has nonempty rendered geometry and visible
    /// computed visibility. Opacity, viewport clipping, covering elements, enabled
    /// state, and stability are not checked.
    Visible,
    /// No element matches, or exactly one element does not satisfy `Visible`.
    Hidden,
}

/// An owned CSS selector recipe for one page or frame, not a retained DOM node.
///
/// Clones retain the underlying page/frame ownership and copy configuration.
/// Each wait queries the current document in the library's isolated world and
/// resolves matches afresh, so DOM replacement does not require a new locator.
/// This does not search inside other frames or pierce shadow roots. A removed
/// frame or process swap is not adopted automatically; acquire a new frame.
#[derive(Clone)]
pub struct Locator {
    target: Target,
    selector: String,
    timeout: Duration,
}

#[derive(Clone)]
enum Target {
    Page(Page),
    Frame(Frame),
}

#[derive(Deserialize)]
struct Sample {
    count: u64,
    visible: bool,
}

impl Locator {
    pub(super) fn for_frame(frame: Frame, selector: impl Into<String>) -> Self {
        Self {
            target: Target::Frame(frame),
            selector: selector.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Set the budget for each subsequent wait on this locator (default: 30 seconds).
    /// A terminal future starts its own budget when first polled. Zero expires
    /// before any protocol work; a deadline overflow is an invalid argument.
    /// Existing clones keep their own configuration.
    #[must_use]
    pub fn timeout(mut self, duration: Duration) -> Self {
        self.timeout = duration;
        self
    }

    /// Wait for a strict, freshly sampled CSS match to reach the requested state.
    ///
    /// Samples immediately, then 100 milliseconds after each unsuccessful sample.
    /// More than one match returns [`Error::InvalidArgument`] for every state;
    /// invalid CSS and other JavaScript errors propagate. Only recognized context
    /// changes are retried. Frame disappearance returns [`Error::FrameNotFound`],
    /// including when waiting for `Hidden` or `Detached`: a lost document does not
    /// prove that its selector is absent. Success describes one sampled instant,
    /// not a guarantee against subsequent DOM changes or a check of click readiness.
    ///
    /// One deadline covers frame validation, context lookup, queries, and polling.
    /// Shorter command timeouts can fail earlier and are not retried. Cancellation
    /// or expiration stops polling but leaves an in-flight owned function call
    /// completing under [`super::FunctionCallBuilder`]'s execution, recovery, and
    /// cleanup bounds. No remote element is retained by the locator.
    pub async fn wait_for(&self, state: ElementState) -> Result<()> {
        let deadline = self.operation_deadline()?;
        tokio::time::timeout_at(deadline, async {
            loop {
                self.check_deadline(deadline, state)?;
                let sample = self.sample(state).await;
                // Tokio may poll a ready inner future before its timeout. Never
                // accept a successful observation after our operation deadline.
                self.check_deadline(deadline, state)?;
                match sample {
                    Ok(sample) => {
                        if sample.count > 1 {
                            return Err(Error::InvalidArgument(format!(
                                "CSS locator {:?} matched {} elements; expected at most one",
                                self.selector, sample.count,
                            )));
                        }
                        let ready = match state {
                            ElementState::Attached => sample.count == 1,
                            ElementState::Detached => sample.count == 0,
                            ElementState::Visible => sample.count == 1 && sample.visible,
                            ElementState::Hidden => sample.count == 0 || !sample.visible,
                        };
                        if ready {
                            return Ok(());
                        }
                    }
                    Err(error) if is_context_change(&error) => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(POLLING_INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_| Err(self.expired(state)))
    }

    pub(super) fn operation_deadline(&self) -> Result<Instant> {
        Instant::now().checked_add(self.timeout).ok_or_else(|| {
            Error::InvalidArgument("locator timeout exceeds the monotonic deadline range".into())
        })
    }

    // Count and acquire in the same synchronous isolated-world expression.
    // Callers supply the operation deadline and decide which readiness is needed.
    pub(super) async fn resolve(&self) -> Result<Option<Element>> {
        let selector = serde_json::to_string(&self.selector)?;
        let expression = format!(
            "(() => {{ const selector = {selector}; const matches = document.querySelectorAll(selector); \
             if (matches.length > 1) throw new Error('CSS locator ' + JSON.stringify(selector) + \
             ' matched ' + matches.length + ' elements; expected at most one'); \
             return matches[0] || null; }})()"
        );
        match &self.target {
            Target::Page(page) => page.element_for_expression(expression).await,
            Target::Frame(frame) => {
                frame.info().await?;
                frame.element_for_expression(expression).await
            }
        }
    }

    async fn sample(&self, state: ElementState) -> Result<Sample> {
        let call = match &self.target {
            Target::Page(page) => page.call(SAMPLE),
            Target::Frame(frame) => {
                // A missing frame is not an empty document. This also prevents
                // endless context retries for a frame already known to be gone.
                // Removal after the snapshot is handled by the next sample.
                frame.info().await?;
                frame.call(SAMPLE)
            }
        };
        call.world(EvaluationWorld::Isolated)
            .args([
                json!(self.selector),
                json!(matches!(
                    state,
                    ElementState::Visible | ElementState::Hidden
                )),
            ])
            .json()
            .await
    }

    fn check_deadline(&self, deadline: Instant, state: ElementState) -> Result<()> {
        if Instant::now() >= deadline {
            Err(self.expired(state))
        } else {
            Ok(())
        }
    }

    fn expired(&self, state: ElementState) -> Error {
        Error::Timeout(format!(
            "waiting for CSS locator {:?} to be {state:?}",
            self.selector,
        ))
    }
}

impl Page {
    /// Create an owned CSS locator for this page's current document.
    /// Construction performs no protocol work. Matches are resolved afresh in
    /// the isolated world when a wait is polled; see [`Locator::wait_for`].
    ///
    /// ```no_run
    /// # async fn example(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// use rustcdp::page::ElementState;
    /// use std::time::Duration;
    /// let results = page.locator("#results").timeout(Duration::from_secs(5));
    /// results.wait_for(ElementState::Visible).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn locator(&self, selector: impl Into<String>) -> Locator {
        Locator {
            target: Target::Page(self.clone()),
            selector: selector.into(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

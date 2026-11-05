use super::{Locator, POLLING_INTERVAL, action::is_detached};
use crate::{Error, Result, page::selectors::is_context_change};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

const READINESS: &str = r#"function(selector) {
    if (!this.isConnected) return { status: 'detached' };
    const matches = this.ownerDocument.querySelectorAll(selector);
    if (matches.length > 1) return { status: 'unsupported', reason: 'fill locator matched multiple elements' };
    if (matches.length !== 1 || matches[0] !== this) return { status: 'detached' };
    const view = this.ownerDocument.defaultView;
    const supported = this instanceof view.HTMLTextAreaElement ||
        (this instanceof view.HTMLInputElement && ['text', 'search', 'tel', 'url', 'password'].includes(this.type));
    if (!supported) return { status: 'unsupported', reason: 'fill requires a textarea or text/search/tel/url/password input' };
    const style = view.getComputedStyle(this);
    if (style.visibility === 'hidden' || style.visibility === 'collapse' ||
        !Array.from(this.getClientRects()).some(r => r.width > 0 && r.height > 0))
        return { status: 'wait', reason: 'control is not visible' };
    if (this.matches(':disabled')) return { status: 'wait', reason: 'control is disabled' };
    if (this.readOnly) return { status: 'wait', reason: 'control is readonly' };
    return { status: 'ready' };
}"#;

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum FillState {
    Ready,
    Wait { reason: String },
    Detached,
    Unsupported { reason: String },
}

impl Locator {
    /// Resolve a unique visible, enabled, writable native text control and replace
    /// its text. Supports the same textarea/input kinds as [`crate::page::Element::fill`].
    /// Hidden, disabled and readonly controls are waited for; unsupported controls
    /// and ambiguous selectors fail. A replaced node is resolved afresh before editing.
    ///
    /// One timeout covers lookup, readiness, focus, selection and insertion. Once
    /// editing starts, it is never replayed. Cancellation may leave focus/selection
    /// or dispatched input in effect; owned input cleanup can outlast the deadline.
    /// No pointer hit test, blur, change event or submission is synthesized.
    pub async fn fill(&self, text: &str) -> Result<()> {
        let deadline = self.operation_deadline()?;
        let mut last = "acquiring a unique control".to_owned();
        let result = tokio::time::timeout_at(deadline, async {
            loop {
                self.check_fill_deadline(deadline, &last)?;
                let element = match self.resolve().await {
                    Ok(Some(element)) => element,
                    Ok(None) => {
                        last = "control is not attached".into();
                        tokio::time::sleep(POLLING_INTERVAL).await;
                        continue;
                    }
                    Err(error) if is_context_change(&error) => {
                        self.check_frame_session(&error).await?;
                        last = "execution context changed during lookup".into();
                        tokio::time::sleep(POLLING_INTERVAL).await;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                self.check_fill_deadline(deadline, &last)?;
                let readiness = element
                    .call(READINESS)
                    .arg(json!(self.selector))
                    .json::<FillState>()
                    .await;
                self.check_fill_deadline(deadline, &last)?;
                match readiness {
                    Ok(FillState::Ready) => {
                        last = "waiting for text insertion".into();
                        return element.fill_until(text, Some(deadline)).await;
                    }
                    Ok(FillState::Wait { reason }) => last = reason,
                    Ok(FillState::Detached) => {
                        last = "control detached or no longer matches".into()
                    }
                    Ok(FillState::Unsupported { reason }) => {
                        return Err(Error::InvalidArgument(reason));
                    }
                    Err(error) if is_context_change(&error) || is_detached(&error) => {
                        self.check_frame_session(&error).await?;
                        last = "control detached or its context changed".into();
                    }
                    Err(error) => return Err(error),
                }
                drop(element);
                tokio::time::sleep(POLLING_INTERVAL).await;
            }
        })
        .await;
        match result {
            Ok(Ok(())) if Instant::now() >= deadline => Err(self.fill_expired(&last)),
            Ok(Err(Error::Timeout(_))) | Err(_) => Err(self.fill_expired(&last)),
            Ok(result) => result,
        }
    }

    fn check_fill_deadline(&self, deadline: Instant, last: &str) -> Result<()> {
        if Instant::now() >= deadline {
            Err(self.fill_expired(last))
        } else {
            Ok(())
        }
    }

    fn fill_expired(&self, last: &str) -> Error {
        Error::Timeout(format!("filling CSS locator {:?}: {last}", self.selector))
    }
}

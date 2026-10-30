use super::{Locator, POLLING_INTERVAL, Target};
use crate::{
    Error, Result,
    page::{Element, TargetKind, input::click_cdp_before, selectors::is_context_change},
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;

const READINESS: &str = include_str!("click.js");

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum Readiness {
    Ready { x: f64, y: f64 },
    Wait { reason: String },
    Detached,
    Unsupported { reason: String },
    Ambiguous { count: u64 },
}

// Bound to the acquired node; no selector re-resolution or input inside this call.
// The selector is checked only to confirm that this node is still the unique match.
pub(super) async fn ready_point(
    element: &Element,
    selector: &str,
    deadline: Instant,
) -> Result<Readiness> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::Timeout(
            "click deadline expired before readiness".into(),
        ));
    }
    element
        .call(READINESS)
        .args([
            json!(selector),
            json!(remaining.as_millis().min(250) as u64),
        ])
        .json()
        .await
}

impl Locator {
    /// Wait for a unique, visible, natively enabled CSS match and click once.
    ///
    /// Scrolls the acquired element into view, requires unchanged geometry in
    /// two consecutive animation frames, then hit-tests a bounded set of points
    /// within its viewport and potential overflow-clipped rectangles. Chrome's
    /// hit test decides which ancestors actually clip. A hit on a descendant is
    /// accepted. Same-origin iframe borders, offsets, clipping and ancestor hit
    /// tests are included. Iframe targets, inaccessible ancestry, transformed or
    /// padded iframes, transforms on or above overflow containers and offset/pinch-zoomed
    /// visual viewports are unsupported and fail explicitly. This is a
    /// conservative readiness check, not an atomic guarantee against DOM changes.
    ///
    /// One absolute timeout includes lookup, readiness and input waiting. Only
    /// recognized context loss or a detached/replaced match can re-resolve before
    /// input. Input is never retried, including after an error or timeout. The
    /// owned press task checks expiry before starting; once pressing starts it
    /// attempts release even after failure, caller cancellation or deadline expiry.
    /// Release retains its existing independent bound and may outlast this wait.
    /// Cancellation can leave an in-flight scroll/readiness call completing; its
    /// animation-frame wait has a local timer and performs no input.
    pub async fn click(&self) -> Result<()> {
        let deadline = self.operation_deadline()?;
        let mut last = "acquiring a unique element".to_owned();
        let result = tokio::time::timeout_at(deadline, async {
            self.check_click_deadline(deadline, &last)?;
            let page = match &self.target {
                Target::Page(page) => page,
                Target::Frame(frame) => frame.page(),
            };
            if page.target_kind != TargetKind::Tab {
                return Err(Error::InvalidArgument(
                    "locator click on an iframe target is unsupported".into(),
                ));
            }
            loop {
                self.check_click_deadline(deadline, &last)?;
                let element = match self.resolve().await {
                    Ok(Some(element)) => element,
                    Ok(None) => {
                        last = "element is not attached".into();
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
                self.check_click_deadline(deadline, &last)?;
                let readiness = ready_point(&element, &self.selector, deadline).await;
                self.check_click_deadline(deadline, &last)?;
                match readiness {
                    Ok(Readiness::Ready { x, y }) => {
                        last = "waiting for input dispatch and release".into();
                        // This is the only dispatch path. Return its outcome;
                        // never go back around the resolution loop afterward.
                        return click_cdp_before(&element, x, y, deadline).await;
                    }
                    Ok(Readiness::Wait { reason }) => last = reason,
                    Ok(Readiness::Detached) => {
                        last = "element detached or no longer matches".into()
                    }
                    Ok(Readiness::Unsupported { reason }) => {
                        return Err(Error::InvalidArgument(reason));
                    }
                    Ok(Readiness::Ambiguous { count }) => {
                        return Err(Error::InvalidArgument(format!(
                            "CSS locator {:?} matched {count} elements; expected at most one",
                            self.selector,
                        )));
                    }
                    Err(error) if is_context_change(&error) || is_detached(&error) => {
                        self.check_frame_session(&error).await?;
                        last = "element detached or its execution context changed".into();
                    }
                    Err(error) => return Err(error),
                }
                drop(element);
                tokio::time::sleep(POLLING_INTERVAL).await;
            }
        })
        .await;
        match result {
            Ok(Ok(())) if Instant::now() >= deadline => Err(self.click_expired(&last)),
            Ok(Err(Error::Timeout(_))) | Err(_) => Err(self.click_expired(&last)),
            Ok(result) => result,
        }
    }

    fn check_click_deadline(&self, deadline: Instant, last: &str) -> Result<()> {
        if Instant::now() >= deadline {
            Err(self.click_expired(last))
        } else {
            Ok(())
        }
    }

    fn click_expired(&self, last: &str) -> Error {
        Error::Timeout(format!("clicking CSS locator {:?}: {last}", self.selector))
    }

    async fn check_frame_session(&self, error: &Error) -> Result<()> {
        if let Target::Frame(frame) = &self.target
            && let Error::Protocol(protocol) = error
            && (protocol
                .message
                .to_ascii_lowercase()
                .contains("no frame for given id")
                || protocol
                    .message
                    .to_ascii_lowercase()
                    .contains("frame with the given id was not found"))
        {
            // If still in the frame tree but not executable in this session,
            // do not guess an OOPIF's coordinate space or adopt another target.
            frame.info().await?;
            return Err(Error::InvalidArgument("frame is not executable in this session; out-of-process or changing iframe targets are unsupported".into()));
        }
        Ok(())
    }
}

fn is_detached(error: &Error) -> bool {
    // B2's explicit attachment validation, not arbitrary user InvalidArguments.
    matches!(error, Error::InvalidArgument(message) if message == "cannot call a detached element")
}

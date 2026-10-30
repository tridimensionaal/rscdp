use super::Element;
use crate::{Error, Result};
use serde_json::json;

impl Element {
    /// Select the unique option whose value exactly matches `value` in a native
    /// single-select control. Empty values are supported; values are not trimmed.
    ///
    /// Rejects non-selects, disconnected or disabled controls (including disabled
    /// fieldsets), multiple-selects, missing/duplicate values, and disabled options
    /// or optgroups before changing selection. Duplicate values are ambiguous even
    /// if one matching option is disabled.
    ///
    /// Uses the native `selectedIndex` setter, then dispatches bubbling `input`
    /// and `change` events, even if the option was already selected. These events
    /// are synthetic (`isTrusted` is false); `input` is also composed. Selection
    /// and attachment are verified after setting and after each event. If an
    /// event handler detaches the control or reverses/replaces the selection,
    /// returns [`Error::InvalidArgument`] without dispatching remaining events.
    /// Verification errors can follow side effects; there is no rollback or retry.
    ///
    /// Execution uses [`Self::call`]: the original element is retained through
    /// caller cancellation and cleanup. Cancellation does not undo a dispatched
    /// selection or event. No fresh element is resolved behind the caller's back.
    ///
    /// ```no_run
    /// # async fn example(element: &rustcdp::page::Element) -> rustcdp::Result<()> {
    /// element.select_option("express").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn select_option(&self, value: &str) -> Result<()> {
        self.select_option_until(value, None).await
    }

    pub(super) async fn select_option_until(
        &self,
        value: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<()> {
        let error: Option<String> = self
            .call(
                r#"function(value) {
                    const view = this.ownerDocument && this.ownerDocument.defaultView;
                    if (!view || !(this instanceof view.HTMLSelectElement))
                        return 'select_option requires a native select element';
                    if (!this.isConnected)
                        return 'select_option requires a connected select element';
                    if (this.multiple)
                        return 'select_option requires a single-select control';
                    if (this.matches(':disabled'))
                        return 'select_option requires an enabled select element';
                    const matches = Array.from(this.options).filter(option => option.value === value);
                    if (matches.length === 0)
                        return 'select_option found no option with the requested value';
                    if (matches.length !== 1)
                        return 'select_option found duplicate options with the requested value';
                    const option = matches[0];
                    if (option.matches(':disabled'))
                        return 'select_option requires an enabled option and optgroup';
                    const verify = () => {
                        if (!this.isConnected)
                            return 'select_option control detached during selection';
                        if (this.multiple || this.options[this.selectedIndex] !== option || option.value !== value)
                            return 'select_option selection changed during selection';
                        return null;
                    };
                    const setter = Object.getOwnPropertyDescriptor(view.HTMLSelectElement.prototype, 'selectedIndex').set;
                    setter.call(this, option.index);
                    let error = verify();
                    if (error !== null) return error;
                    this.dispatchEvent(new view.Event('input', { bubbles: true, composed: true }));
                    error = verify();
                    if (error !== null) return error;
                    this.dispatchEvent(new view.Event('change', { bubbles: true }));
                    return verify();
                }"#,
            )
            .deadline(deadline)
            .arg(json!(value))
            .json()
            .await?;
        match error {
            Some(message) => Err(Error::InvalidArgument(message)),
            None => Ok(()),
        }
    }
}

impl Element {
    /// Ensure this native checkbox matches `checked`, using at most one click.
    ///
    /// An already-correct checkbox succeeds without scrolling or input, including
    /// when hidden, disabled or covered. Radio buttons and custom ARIA controls
    /// are rejected. Otherwise waits for visibility, native enabled state, stable
    /// geometry and a clear hit-test point using [`super::Locator::click`]'s
    /// readiness checks and conservative iframe/clipping/transform limits.
    /// The original handle is retained; a detached or replaced node is an error,
    /// never a request to find another element.
    ///
    /// After the native click, verifies that this same connected checkbox has
    /// the requested state. A prevented/reverted change is an error, with no
    /// retry or rollback. DOM changes between checks and input, or delayed
    /// handler changes after verification, remain possible.
    ///
    /// One 30-second deadline covers readiness, input and verification; shorter
    /// handle command timeouts can fail earlier. Cancellation or expiry does not
    /// undo input. Once pressing starts, release is attempted under its existing
    /// independent bound and can outlast the caller's wait.
    pub async fn set_checked(&self, checked: bool) -> Result<()> {
        use super::{
            input::click_cdp_before,
            locator::action::{Readiness, ready_point},
        };
        use std::time::Duration;
        use tokio::time::Instant;

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = "checking native checkbox state".to_owned();
        let result = tokio::time::timeout_at(deadline, async {
            loop {
                check_checkbox_deadline(deadline, &last)?;
                let readiness = ready_point(self, None, Some(checked), deadline).await?;
                check_checkbox_deadline(deadline, &last)?;
                match readiness {
                    Readiness::Checked => return Ok(()),
                    Readiness::Ready { x, y } => {
                        // Elements retain a Session rather than a Page. Confirm
                        // its input coordinate space without resolving a new node.
                        let target = self
                            .0
                            .session
                            .connection()
                            .execute(
                                &crate::cdp::target::commands::GetTargetInfo::default()
                                    .with_target_id(self.0.session.target_id().clone()),
                            )
                            .await?
                            .target_info;
                        if target.type_ != "page" {
                            return Err(Error::InvalidArgument(
                                "set_checked input on an iframe target is unsupported".into(),
                            ));
                        }
                        last = "waiting for input dispatch and release".into();
                        check_checkbox_deadline(deadline, &last)?;
                        click_cdp_before(self, x, y, deadline).await?;
                        last = "verifying checkbox state after input".into();
                        check_checkbox_deadline(deadline, &last)?;
                        return self.verify_checked(checked, deadline).await;
                    }
                    Readiness::Wait { reason } => last = reason,
                    Readiness::Detached => {
                        return Err(Error::InvalidArgument(
                            "cannot set checked state on a detached element".into(),
                        ));
                    }
                    Readiness::Unsupported { reason } => {
                        return Err(Error::InvalidArgument(reason));
                    }
                    Readiness::Ambiguous { .. } => {
                        return Err(Error::InvalidArgument(
                            "unexpected selector ambiguity for a direct checkbox handle".into(),
                        ));
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        match result {
            Ok(Ok(())) if Instant::now() >= deadline => Err(checkbox_expired(&last)),
            Ok(Err(Error::Timeout(_))) | Err(_) => Err(checkbox_expired(&last)),
            Ok(result) => result,
        }
    }

    pub(super) async fn verify_checked(
        &self,
        checked: bool,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let error: Option<String> = self.call(r#"function(checked) {
            const view = this.ownerDocument && this.ownerDocument.defaultView;
            if (!view || !(this instanceof view.HTMLInputElement) || this.type !== 'checkbox')
                return 'set_checked target is no longer a native checkbox';
            if (!this.isConnected)
                return 'set_checked target detached after input';
            if (this.checked !== checked)
                return 'set_checked did not reach the requested state; the click may have been prevented or reverted';
            return null;
        }"#).deadline(Some(deadline)).arg(json!(checked)).json().await?;
        match error {
            Some(message) => Err(Error::InvalidArgument(message)),
            None => Ok(()),
        }
    }
}

fn check_checkbox_deadline(deadline: tokio::time::Instant, last: &str) -> Result<()> {
    if tokio::time::Instant::now() >= deadline {
        Err(checkbox_expired(last))
    } else {
        Ok(())
    }
}

fn checkbox_expired(last: &str) -> Error {
    Error::Timeout(format!("setting checkbox state: {last}"))
}

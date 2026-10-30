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

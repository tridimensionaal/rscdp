use super::Locator;
use crate::{Error, Result};
use tokio::time::Instant;

impl Locator {
    /// Wait for one attached native single-select, then select its exact option
    /// value. Hidden selects are supported; this does not simulate pointer input.
    /// Disabled controls/options and invalid values are rejected before mutation,
    /// as with [`crate::page::Element::select_option`].
    ///
    /// One deadline covers lookup and selection. The node is resolved afresh while
    /// waiting for attachment; selection and its input/change events are never
    /// retried once admitted. Cancellation does not undo dispatched changes.
    pub async fn select_option(&self, value: &str) -> Result<()> {
        let deadline = self.operation_deadline()?;
        let element = self.attached_until(deadline).await?;
        let result =
            tokio::time::timeout_at(deadline, element.select_option_until(value, Some(deadline)))
                .await
                .map_err(|_| self.form_expired("selecting an option"))?;
        if result.is_ok() && Instant::now() >= deadline {
            return Err(self.form_expired("selecting an option"));
        }
        result
    }

    fn form_expired(&self, action: &str) -> Error {
        Error::Timeout(format!("{action} for CSS locator {:?}", self.selector))
    }
}

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

    /// Wait for one attached native file input and select files on Chrome's host.
    /// Hidden inputs are supported. Empty `files` clears the selection; multiple
    /// files require a `multiple` input. See [`crate::page::Element::set_input_files`]
    /// for path syntax, remote-host behavior and owned cleanup.
    ///
    /// Paths are validated before lookup. One deadline covers attachment,
    /// validation and setter admission. The setter is never retried; cancellation
    /// cannot undo a selection already admitted to the connection.
    pub async fn set_input_files(&self, files: &[std::path::PathBuf]) -> Result<()> {
        let deadline = self.operation_deadline()?;
        if Instant::now() >= deadline {
            return Err(self.form_expired("setting input files"));
        }
        let files = crate::page::form::protocol_file_paths(files)?;
        let element = self.attached_until(deadline).await?;
        let result =
            tokio::time::timeout_at(deadline, element.set_input_files_until(files, deadline))
                .await
                .map_err(|_| self.form_expired("setting input files"))?;
        if result.is_ok() && Instant::now() >= deadline {
            return Err(self.form_expired("setting input files"));
        }
        result
    }

    fn form_expired(&self, action: &str) -> Error {
        Error::Timeout(format!("{action} for CSS locator {:?}", self.selector))
    }
}

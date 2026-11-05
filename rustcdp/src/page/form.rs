use super::Element;
use crate::{Error, Result, cdp::dom::commands::SetFileInputFiles, error::with_cleanup};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use tokio::time::Instant;

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
    /// selection or event. The element is never resolved afresh.
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

// Validate nonempty selection before CDP dispatch. Chrome ignores an empty
// DOM.setFileInputFiles list, so clearing uses the native value setter instead.
const PREPARE_FILE_INPUT: &str = r#"function(count) {
    const view = this.ownerDocument && this.ownerDocument.defaultView;
    if (!view || !(this instanceof view.HTMLInputElement) || this.type !== 'file')
        return 'set_input_files requires a native file input';
    if (!this.isConnected)
        return 'set_input_files requires an attached file input';
    if (count > 1 && !this.multiple)
        return 'set_input_files requires multiple for more than one file';
    if (count === 0) {
        const verify = () => {
            if (!this.isConnected || this.type !== 'file')
                return 'set_input_files control detached or changed type during clearing';
            if (this.files.length !== 0 || this.value !== '')
                return 'set_input_files selection changed during clearing';
            return null;
        };
        const setter = Object.getOwnPropertyDescriptor(view.HTMLInputElement.prototype, 'value').set;
        setter.call(this, '');
        let error = verify();
        if (error !== null) return error;
        this.dispatchEvent(new view.Event('input', { bubbles: true, composed: true }));
        error = verify();
        if (error !== null) return error;
        this.dispatchEvent(new view.Event('change', { bubbles: true }));
        return verify();
    }
    return null;
}"#;

impl Element {
    /// Select files on this native file input, or clear its selection with `&[]`.
    /// Hidden inputs are supported. More than one file requires `multiple`.
    ///
    /// Paths must be absolute UTF-8 strings without NUL, on **Chrome's host**.
    /// POSIX paths, Windows drive-rooted paths, and UNC paths are recognized
    /// regardless of the client OS (including `\\?\` drive/UNC forms). Windows
    /// drive-relative paths such as `C:file.csv` and device paths are rejected.
    /// Paths are sent unchanged: no canonicalization, local existence check, or
    /// transfer of file bytes occurs. The caller must choose paths meaningful to
    /// Chrome's OS and filesystem. Selection does not submit or upload a form.
    ///
    /// Nonempty selection uses `DOM.setFileInputFiles` with this element's
    /// existing object ID. Validation and dispatch are separate commands, so
    /// scripts can still change the control between them. Clearing uses the
    /// native `value` setter and dispatches synthetic bubbling `input` (also
    /// composed) and `change` events. Attachment and empty selection are checked
    /// after the setter and each event; handlers that replace or repopulate the
    /// control produce an error without replay.
    /// No visibility, scrolling, or pointer-readiness check is performed.
    ///
    /// An owned operation retains the element through validation and dispatch,
    /// even after caller cancellation. The caller's wait and deadline for
    /// starting the setter share a 30-second budget, shortened by the handle's
    /// command timeout. An already-started CDP setter retains the element while
    /// awaiting its response, capped at 30 seconds (or the shorter handle timeout).
    /// For clearing, the owned function execution keeps the independent 30-second
    /// cap described by [`super::ElementCallBuilder`].
    /// Cleanup may outlast those limits and needs a running Tokio runtime.
    /// Cancellation or timeout does not undo selection; the operation never
    /// retries or resolves a replacement element.
    pub async fn set_input_files(&self, files: &[PathBuf]) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        self.set_input_files_until(protocol_file_paths(files)?, deadline)
            .await
    }

    // Locator can pass its existing absolute deadline. In particular, cancellation
    // of the caller's wait must not let delayed validation dispatch a setter after
    // that deadline. Once dispatched, await the bounded response with an owner.
    pub(super) async fn set_input_files_until(
        &self,
        files: Vec<String>,
        deadline: Instant,
    ) -> Result<()> {
        let limit = Duration::from_secs(30);
        let timeout = self
            .0
            .session
            .connection()
            .command_timeout()
            .unwrap_or(limit)
            .min(limit);
        let deadline = deadline.min(Instant::now() + timeout);
        check_file_deadline(deadline)?;
        let element = self.clone();
        let session = self.0.session.clone().with_command_timeout(Some(timeout));
        let task = self.0.session.connection().runtime().spawn(async move {
            let result = async {
                check_file_deadline(deadline)?;
                let validation: Option<String> = tokio::time::timeout_at(
                    deadline,
                    element
                        .call(PREPARE_FILE_INPUT)
                        .deadline(Some(deadline))
                        .arg(json!(files.len()))
                        .json(),
                )
                .await
                .map_err(|_| file_timeout())??;
                if let Some(message) = validation {
                    return Err(Error::InvalidArgument(message));
                }
                check_file_deadline(deadline)?;
                if files.is_empty() {
                    return Ok(());
                }
                session
                    .execute_before(
                        &SetFileInputFiles::new(files).with_object_id(element.object_id().clone()),
                        Some(deadline),
                    )
                    .await?;
                Ok(())
            }
            .await;
            with_cleanup(result, element.release().await)
        });
        let result = tokio::time::timeout_at(deadline, task)
            .await
            .map_err(|_| file_timeout())??;
        result?;
        // Tokio polls the inner future before its timer; a ready result can
        // otherwise report success even after the shared deadline has expired.
        check_file_deadline(deadline)
    }
}

fn file_timeout() -> Error {
    Error::Timeout("setting input files".into())
}

fn check_file_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(file_timeout())
    } else {
        Ok(())
    }
}

pub(super) fn protocol_file_paths(files: &[PathBuf]) -> Result<Vec<String>> {
    files
        .iter()
        .map(|path| {
            let path = path.to_str().ok_or_else(|| {
                Error::InvalidArgument("file paths must be UTF-8 for Chrome".into())
            })?;
            if path.contains('\0') || !is_absolute_chrome_path(path) {
                return Err(Error::InvalidArgument(
                    "file paths must be absolute on Chrome's host and contain no NUL".into(),
                ));
            }
            Ok(path.to_owned())
        })
        .collect()
}

fn is_absolute_chrome_path(path: &str) -> bool {
    // Path::is_absolute() interprets the *client* OS's syntax. A Linux client
    // can address Windows Chrome and vice versa, so inspect the protocol string.
    if path.starts_with('/') {
        return true;
    }
    let (windows, verbatim) = match path.strip_prefix(r"\\?\") {
        Some(path) => (path, true),
        None => (path, false),
    };
    let bytes = windows.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return true;
    }
    let unc = if verbatim {
        windows.strip_prefix(r"UNC\")
    } else {
        windows.strip_prefix(r"\\")
    };
    let Some(unc) = unc else {
        return false;
    };
    let mut parts = unc.split(['\\', '/']);
    // Both server and share are required. Exclude device namespace prefixes
    // and dot components, which do not establish an absolute UNC share.
    let valid = |part: Option<&str>| matches!(part, Some(s) if !matches!(s, "" | "." | ".." | "?"));
    valid(parts.next()) && valid(parts.next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_host_paths_are_absolute_without_local_filesystem_resolution() {
        for path in [
            "/remote-only/雪/report.csv",
            "/remote-only/../report.csv",
            r"C:\remote-only\report.csv",
            "Z:/remote-only/report.csv",
            r"\\server\share\report.csv",
            r"\\?\C:\remote-only\report.csv",
            r"\\?\UNC\server\share\report.csv",
        ] {
            assert_eq!(protocol_file_paths(&[PathBuf::from(path)]).unwrap(), [path]);
        }
        assert!(protocol_file_paths(&[]).unwrap().is_empty());
    }

    #[test]
    fn ambiguous_relative_and_nul_paths_are_rejected() {
        for path in [
            "",
            "file.csv",
            "../file.csv",
            "C:file.csv",
            "C:",
            r"\file.csv",
            r"\\server",
            r"\\server\",
            r"\\\share\file.csv",
            r"\\.\pipe\file",
            r"\\?\file.csv",
            r"\\?\UNC\server",
            "/remote/bad\0.csv",
        ] {
            assert!(
                matches!(
                    protocol_file_paths(&[PathBuf::from(path)]),
                    Err(Error::InvalidArgument(_))
                ),
                "{path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_conversion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let path = PathBuf::from(OsString::from_vec(b"/remote/bad-\xff.csv".to_vec()));
        assert!(matches!(
            protocol_file_paths(&[path]),
            Err(Error::InvalidArgument(_))
        ));
    }
}

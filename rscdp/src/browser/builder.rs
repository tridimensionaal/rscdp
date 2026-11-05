use super::Browser;
use crate::{
    Error, Result,
    connection::ConnectionOptions,
    launcher::{
        LaunchOptions,
        args::{validate_proxy_args, validated_switch_name},
        startup_deadline, validated_profile_prefix,
    },
    proxy::Proxy,
};
use std::{ffi::OsString, path::PathBuf, time::Duration};

/// Configure and launch Chrome with [`Browser::builder`].
///
/// Chrome is visible by default. Setters replace earlier values; [`Self::arg`]
/// and [`Self::args`] append arguments in order, preserving non-Unicode values.
/// Explicit typed settings cannot also be supplied as raw flags, even with the
/// same value. Validation happens in [`Self::build`] before any process starts.
///
/// | Setting | Default | Chrome switch |
/// | --- | --- | --- |
/// | [`headless`](Self::headless) | Off | `--headless=new` |
/// | [`window_size`](Self::window_size) | Chrome default | `--window-size` |
/// | [`devtools`](Self::devtools) | Off | `--auto-open-devtools-for-tabs` |
/// | [`executable`](Self::executable) | Discover Chrome / `CHROME_PATH` | — |
/// | [`profile_dir`](Self::profile_dir) | Temporary profile | `--user-data-dir` |
/// | [`startup_timeout`](Self::startup_timeout) | 15 seconds total process startup | — |
/// | [`connection_options`](Self::connection_options) | [`ConnectionOptions::default`] | — |
/// | [`command_timeout`](Self::command_timeout) | Some(30 seconds) | — |
/// | [`proxy`](Self::proxy) | No configured proxy | `--proxy-server` |
/// | [`base_temp_dir`](Self::base_temp_dir) | OS temporary directory | — |
/// | [`dir_prefix`](Self::dir_prefix) | `tmp` | — |
///
/// The launcher manages remote-debugging flags and the user-data directory;
/// these cannot be passed through raw arguments. Other raw switches are passed
/// to Chrome without interpreting their values or guaranteeing browser support.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct BrowserBuilder {
    options: LaunchOptions,
    headless: Option<bool>,
    window_size: Option<(u32, u32)>,
    devtools: Option<bool>,
}

impl BrowserBuilder {
    /// Configure process routing and optional authentication for managed pages.
    /// Replaces an earlier proxy. Build retains the configuration; launch arms
    /// workers without navigating. Raw proxy routing flags conflict, but a raw
    /// `--proxy-bypass-list` is allowed. See [`Browser::launch_with_proxy`].
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.options.proxy = Some(proxy);
        self
    }

    /// Run without visible windows when true. Defaults to false.
    pub fn headless(mut self, enabled: bool) -> Self {
        self.headless = Some(enabled);
        self
    }

    /// Set initial browser window dimensions, not the page's emulated viewport.
    /// Both dimensions must be greater than zero.
    pub fn window_size(mut self, width: u32, height: u32) -> Self {
        self.window_size = Some((width, height));
        self
    }

    /// Open DevTools automatically for tabs. Defaults to false.
    /// Enabling both this setting and [`Self::headless`] is rejected.
    pub fn devtools(mut self, enabled: bool) -> Self {
        self.devtools = Some(enabled);
        self
    }

    /// Select an executable instead of discovering Chrome or using `CHROME_PATH`.
    pub fn executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.executable = Some(path.into());
        self
    }

    /// Use a persistent user-data directory, retained after browser shutdown.
    /// Concurrent rscdp launches of the same canonical directory are rejected.
    /// A persistent sibling lock file requires write access to its parent directory.
    pub fn profile_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.profile_dir = Some(path.into());
        self
    }

    /// Set one process-startup budget, including profile preparation, spawn,
    /// endpoint discovery, handshake and PID verification. Defaults to 15 seconds.
    /// Zero and unrepresentable deadlines are rejected before I/O. Synchronous
    /// OS work cannot be preempted; already-started filesystem work may overrun
    /// the budget. Awaited failure cleanup can extend total elapsed time.
    /// Initial-page attachment after process startup has independent bounds.
    pub fn startup_timeout(mut self, timeout: Duration) -> Self {
        self.options.startup_timeout = timeout;
        self
    }

    /// Replace all connection settings, including any earlier command timeout.
    /// Limits are validated before launch I/O. The handshake timeout is capped by
    /// the remaining startup budget; PID verification uses the startup deadline.
    pub fn connection_options(mut self, options: ConnectionOptions) -> Self {
        self.options.connection_options = options;
        self
    }

    /// Set the initial connection's command timeout, preserving its other settings.
    /// None disables caller command deadlines, not startup, transport, or owned
    /// acquisition/cleanup bounds. A later `connection_options` call replaces this.
    pub fn command_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.options.connection_options.command_timeout = timeout;
        self
    }

    /// Set the parent directory for temporary profiles.
    /// Defaults to [`std::env::temp_dir`].
    /// Ignored when [`Self::profile_dir`] supplies a persistent profile.
    pub fn base_temp_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.base_temp_dir = path.into();
        self
    }

    /// Set the temporary-profile prefix. Defaults to `tmp`; empty is allowed.
    /// Path separators are rejected by build and launch for temporary profiles.
    /// Ignored when a persistent profile is configured.
    pub fn dir_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.options.dir_prefix = Some(prefix.into());
        self
    }

    /// Append one Chrome argument. No shell parsing or quoting is performed.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.options.args.push(arg.into());
        self
    }

    /// Append Chrome arguments in iteration order, preserving non-Unicode values.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.options.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Validate settings and produce options without starting Chrome.
    ///
    /// Rejects zero window dimensions, typed headless plus DevTools, reserved
    /// launcher flags, the `--` terminator, and raw flags duplicating explicitly
    /// set typed options. Switch checks account for surrounding whitespace and
    /// Windows switch spelling; the original arguments are preserved.
    /// Also validates connection limits and temporary-profile prefixes, and
    /// rejects zero or unrepresentable startup deadlines. Validation does
    /// not start the countdown; launch revalidates and starts its own deadline.
    pub fn build(mut self) -> Result<LaunchOptions> {
        startup_deadline(self.options.startup_timeout)?;
        self.options.connection_options.validate()?;
        validated_profile_prefix(&self.options)?;
        if self.options.proxy.is_some() {
            validate_proxy_args(&self.options.args)?;
        }
        if matches!(self.window_size, Some((0, _)) | Some((_, 0))) {
            return Err(Error::InvalidArgument(
                "window dimensions must be positive".into(),
            ));
        }
        if self.headless == Some(true) && self.devtools == Some(true) {
            return Err(Error::InvalidArgument(
                "DevTools requires a visible browser".into(),
            ));
        }
        for arg in &self.options.args {
            let Some(name) = validated_switch_name(arg)? else {
                continue;
            };
            let duplicate = match name.as_str() {
                "headless" => self.headless.is_some(),
                "window-size" => self.window_size.is_some(),
                "auto-open-devtools-for-tabs" => self.devtools.is_some(),
                _ => false,
            };
            if duplicate {
                return Err(Error::InvalidArgument(format!(
                    "{} is configured by both a builder setter and a raw argument",
                    name
                )));
            }
        }
        if self.headless == Some(true) {
            self.options.args.push("--headless=new".into());
        }
        if let Some((width, height)) = self.window_size {
            self.options
                .args
                .push(format!("--window-size={width},{height}").into());
        }
        if self.devtools == Some(true) {
            self.options
                .args
                .push("--auto-open-devtools-for-tabs".into());
        }
        Ok(self.options)
    }

    /// Validate configuration, launch Chrome, and attach to its initial page.
    /// Uses the same process ownership and cleanup as [`Browser::launch`].
    pub async fn launch(self) -> Result<Browser> {
        Browser::launch(self.build()?).await
    }
}

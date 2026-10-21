use crate::{Connection, Error, Result, connection::ConnectionOptions, error::with_cleanup};
use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;
use tokio::time::Instant;

pub(crate) mod args;

pub(crate) fn startup_deadline(timeout: Duration) -> Result<Instant> {
    if timeout.is_zero() {
        return Err(Error::InvalidArgument(
            "startup_timeout must be nonzero".into(),
        ));
    }
    Instant::now().checked_add(timeout).ok_or_else(|| {
        Error::InvalidArgument(
            "startup_timeout exceeds the supported monotonic deadline range".into(),
        )
    })
}

fn startup_remaining(deadline: Instant, stage: &str) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| Error::Timeout(format!("starting Chrome: {stage}")))
}

pub(crate) fn validated_profile_prefix(options: &LaunchOptions) -> Result<&str> {
    let prefix = options.dir_prefix.as_deref().unwrap_or("tmp");
    if options.profile_dir.is_none() && prefix.contains(['/', '\\']) {
        return Err(Error::InvalidArgument(
            "profile prefix must not contain path separators".into(),
        ));
    }
    Ok(prefix)
}

fn lock_profile(profile: &Path) -> std::io::Result<File> {
    let name = profile.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "profile must not be a filesystem root",
        )
    })?;
    let mut lock_name = OsString::from(".rustcdp-");
    lock_name.push(name);
    lock_name.push(".lock");
    // The sibling survives profile removal/recreation. Never unlink it: another
    // opener must not lock a replacement inode while this one is still held.
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(profile.with_file_name(lock_name))?;
    file.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "profile is already owned by another rustcdp launch: {}",
                profile.display()
            ),
        ),
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

fn browser_endpoint(contents: &[u8]) -> Option<String> {
    let mut lines = std::str::from_utf8(contents).ok()?.lines();
    let port = lines
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let path = lines.next()?;
    let id = path.strip_prefix("/devtools/browser/")?;
    if id.is_empty() || id.chars().any(|c| c.is_whitespace() || "/?#".contains(c)) {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

fn fresh_endpoint(contents: &[u8], previous: Option<&[u8]>) -> Option<String> {
    if previous == Some(contents) {
        return None;
    }
    let endpoint = browser_endpoint(contents)?;
    if previous.and_then(browser_endpoint).as_ref() == Some(&endpoint) {
        return None;
    }
    Some(endpoint)
}

/// Chrome executable, arguments, profile location, startup deadline, and connection settings.
/// The launcher selects a debugging port through Chrome and spawns at most one
/// child per invocation. Use [`crate::Browser::connect`] for
/// an existing fixed endpoint. Headless mode is not enabled automatically.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Explicit executable; otherwise discover Chrome, honoring `CHROME_PATH`.
    pub executable: Option<PathBuf>,
    /// Additional Chrome flags, for example `--headless=new`.
    /// Profile and remote-debugging flags are reserved for the launcher.
    /// The `--` terminator and Windows `single-argument` switch are rejected.
    pub args: Vec<OsString>,
    /// A fixed profile is retained after shutdown. Concurrent rustcdp launches
    /// using the same canonical directory are rejected by a persistent sibling
    /// `.rustcdp-<profile-name>.lock` file. The parent must be writable.
    /// Chrome's own singleton files and any existing DevToolsActivePort are left intact.
    pub profile_dir: Option<PathBuf>,
    /// Parent of temporary profiles; defaults to [`std::env::temp_dir`].
    pub base_temp_dir: PathBuf,
    /// `None` uses "tmp"; an empty prefix is preserved.
    pub dir_prefix: Option<String>,
    /// One startup budget from the first poll through profile preparation, spawn,
    /// endpoint discovery, handshake, and PID verification. Defaults to 15 seconds.
    /// Zero and unrepresentable monotonic deadlines are rejected before I/O.
    /// Synchronous OS work cannot be preempted, and already-started filesystem
    /// work may overrun this budget. Failure cleanup is awaited outside the
    /// budget, so returning can take longer. Caller cancellation uses Drop
    /// cleanup. Browser's subsequent initial-page setup has independent bounds.
    pub startup_timeout: Duration,
    /// Transport limits and initial command timeout; defaults to [`ConnectionOptions::default`].
    /// Validated before launch I/O. The handshake uses the smaller of its connect
    /// timeout and the remaining startup budget. PID verification uses the startup
    /// deadline; the returned connection retains the configured command timeout.
    pub connection_options: ConnectionOptions,
    /// Process routing and optional credentials for Browser-managed pages.
    /// Use [`crate::Browser::launch`] or its builder. The process-only
    /// [`BrowserProcess::launch`] rejects this field because it owns no workers.
    pub proxy: Option<crate::proxy::Proxy>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            args: Vec::new(),
            profile_dir: None,
            base_temp_dir: std::env::temp_dir(),
            dir_prefix: None,
            startup_timeout: Duration::from_secs(15),
            connection_options: ConnectionOptions::default(),
            proxy: None,
        }
    }
}

/// Owns exactly one child and, when applicable, its temporary profile.
/// Call `close` to await cleanup. Drop schedules a kill/reap/cleanup fallback.
/// Endpoint ownership is checked using SystemInfo.getProcessInfo. The executable
/// must remain the browser process (wrappers must exec, not spawn another PID).
/// The profile lock coordinates rustcdp only; it does not replace Chrome's lock.
#[must_use = "retain this process owner while using Chrome; await close() to reap it and clean up"]
pub struct BrowserProcess {
    child: Option<Child>,
    temporary: Option<TempDir>,
    profile_lock: Option<File>,
    cleanup: Option<tokio::sync::oneshot::Receiver<std::io::Result<()>>>,
    profile: PathBuf,
    endpoint: String,
}

impl BrowserProcess {
    pub async fn launch(options: LaunchOptions) -> Result<(Self, Connection)> {
        let deadline = startup_deadline(options.startup_timeout)?;
        options.connection_options.validate()?;
        if options.proxy.is_some() {
            return Err(Error::InvalidArgument(
                "typed proxy configuration requires Browser::launch; BrowserProcess owns no authentication workers".into(),
            ));
        }
        for arg in &options.args {
            args::validated_switch_name(arg)?;
        }
        let prefix = validated_profile_prefix(&options)?;
        // Owners stay outside timed work: an ordinary failure awaits cleanup,
        // while cancellation still reaches their Drop fallbacks.
        let mut process = Self {
            child: None,
            temporary: None,
            profile_lock: None,
            cleanup: None,
            profile: PathBuf::new(),
            endpoint: String::new(),
        };
        let mut candidate = None;
        let started = async {
            startup_remaining(deadline, "discovering executable")?;
            let executable = options.executable.clone().map_or_else(find_chrome, Ok)?;
            startup_remaining(deadline, "preparing profile")?;
            process.profile = if let Some(path) = &options.profile_dir {
                // Await mutating filesystem work rather than abandon a worker
                // that could still be creating the directory during cleanup.
                tokio::fs::create_dir_all(path).await?;
                startup_remaining(deadline, "preparing profile")?;
                let profile = tokio::fs::canonicalize(path).await?;
                startup_remaining(deadline, "locking profile")?;
                profile
            } else {
                tokio::fs::create_dir_all(&options.base_temp_dir).await?;
                startup_remaining(deadline, "creating temporary profile")?;
                process.temporary = Some(
                    tempfile::Builder::new()
                        .prefix(prefix)
                        .tempdir_in(&options.base_temp_dir)?,
                );
                // Creation, canonicalization and try-lock form a synchronous
                // acquisition section. Own the directory before fallible work,
                // and establish its lock before checking expiry/removing it.
                match std::fs::canonicalize(process.temporary.as_ref().expect("created profile").path()) {
                    Ok(profile) => profile,
                    Err(error) => {
                        // The exposed path may already be owned by another
                        // launcher. Preserve it because we have no lock yet.
                        if let Some(directory) = process.temporary.take() {
                            let _ = directory.keep();
                        }
                        return Err(error.into());
                    }
                }
            };
            process.profile_lock = Some(match lock_profile(&process.profile) {
                Ok(lock) => lock,
                Err(error) => {
                    // Another launch may own this directory now. Never remove
                    // it without the lock, even after our deadline has expired.
                    if let Some(directory) = process.temporary.take() {
                        let _ = directory.keep();
                    }
                    return Err(error.into());
                }
            });
            startup_remaining(deadline, "preparing locked profile")?;
            // Cleanup could have removed the path before we acquired its lock.
            std::fs::create_dir_all(&process.profile)?;
            startup_remaining(deadline, "reading previous endpoint")?;
            let port_file = process.profile.join("DevToolsActivePort");
            let previous_port_file = match tokio::fs::read(&port_file).await {
                Ok(contents) => Some(contents),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            startup_remaining(deadline, "spawning browser")?;
            let mut command = Command::new(executable);
            let mut profile_arg = OsString::from("--user-data-dir=");
            profile_arg.push(&process.profile);
            command
                .arg(profile_arg)
                .arg("--remote-debugging-port=0")
                .args([
                    "--no-first-run",
                    "--no-service-autorun",
                    "--no-default-browser-check",
                    "--disable-backgrounding-occluded-windows",
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            command.args(&options.args).arg("about:blank");
            startup_remaining(deadline, "spawning browser")?;
            process.child = Some(
                command
                    .spawn()
                    .map_err(|error| Error::Launch(format!("could not spawn Chrome: {error}")))?,
            );
            startup_remaining(deadline, "discovering endpoint")?;
            tokio::time::timeout_at(deadline, async {
                loop {
                    startup_remaining(deadline, "discovering endpoint")?;
                    if let Some(status) = process.child.as_mut().expect("launched child").try_wait()? {
                        return Err(Error::Launch(format!("Chrome exited with {status}")));
                    }
                    startup_remaining(deadline, "reading endpoint")?;
                    let endpoint = match tokio::fs::read(&port_file).await {
                        Ok(contents) => fresh_endpoint(&contents, previous_port_file.as_deref()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                        Err(e) => return Err(e.into()),
                    };
                    if let Some(endpoint) = endpoint {
                        let remaining = startup_remaining(deadline, "connecting to endpoint")?;
                        match options.connection_options.clone()
                            .connect_timeout(options.connection_options.connect_timeout.min(remaining))
                            .connect(&endpoint).await {
                            Ok(connection) => {
                                // Retain the connection outside this timed future
                                // before any PID verification await can expire.
                                candidate = Some(connection);
                                startup_remaining(deadline, "verifying browser PID")?;
                                let connection = candidate.as_ref().expect("connected browser")
                                    .clone().with_command_timeout(None);
                                let info = connection.execute(
                                    &crate::cdp::system_info::commands::GetProcessInfo::default(),
                                ).await?;
                                let owned = info.process_info.iter().any(|info| {
                                    info.type_ == "browser"
                                        && Some(info.id) == process.id().map(i64::from)
                                });
                                if !owned {
                                    return Err(Error::Launch(
                                        "debugging endpoint does not belong to the spawned browser PID".into(),
                                    ));
                                }
                                // Do not publish an exited child or a late success.
                                if let Some(status) = process.child.as_mut().expect("launched child").try_wait()? {
                                    return Err(Error::Launch(format!("Chrome exited with {status}")));
                                }
                                startup_remaining(deadline, "finishing startup")?;
                                process.endpoint = endpoint;
                                return Ok(());
                            }
                            // Discovery can race Chrome's listener becoming ready.
                            // Retry only the handshake, not spawning, under the same deadline.
                            Err(Error::Http(_) | Error::WebSocket(_)) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }).await.map_err(|_| Error::Timeout("starting Chrome: endpoint discovery or PID verification".into()))?
        }.await;
        match started {
            Ok(()) => Ok((process, candidate.expect("verified browser connection"))),
            Err(error) => {
                if let Some(connection) = candidate {
                    connection.close().await;
                }
                with_cleanup(Err(error), process.close().await)
            }
        }
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile
    }
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Reap the child before removing its temporary profile. Fixed profiles remain.
    /// Also removes an owned temporary profile when startup failed before spawning.
    /// On Unix, allow up to three seconds after TERM before forcing termination.
    /// Canceling the wait leaves cleanup running; a later call can await it.
    /// The profile lock is held through reaping and temporary-directory removal.
    /// The sibling lock file remains after cleanup to preserve its identity.
    /// If termination fails, the lock is retained until the host application exits
    /// and the temporary directory is preserved to avoid reusing a live profile.
    pub async fn close(&mut self) -> Result<()> {
        if self.child.is_some() || self.temporary.is_some() || self.profile_lock.is_some() {
            let cleanup = Cleanup {
                child: self.child.take(),
                temporary: self.temporary.take(),
                profile_lock: self.profile_lock.take(),
            };
            let (reply, done) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name("rustcdp-cleanup".into())
                .spawn(move || {
                    let _ = reply.send(cleanup.run());
                })?;
            self.cleanup = Some(done);
        }
        if let Some(done) = &mut self.cleanup {
            // Keep completion owned here if the caller cancels this await.
            let result = done.await;
            self.cleanup = None;
            result.map_err(|_| Error::Launch("cleanup worker stopped".into()))??;
        }
        Ok(())
    }
}

struct Cleanup {
    child: Option<Child>,
    temporary: Option<TempDir>,
    profile_lock: Option<File>,
}

impl Cleanup {
    fn run(mut self) -> std::io::Result<()> {
        let result = reap(
            self.child.take(),
            self.temporary.take(),
            self.profile_lock.take(),
        );
        #[cfg(feature = "tracing")]
        tracing::debug!(target: "rustcdp", success = result.is_ok(), "browser cleanup finished");
        result
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.child.is_some() || self.temporary.is_some() || self.profile_lock.is_some() {
            let _ = reap(
                self.child.take(),
                self.temporary.take(),
                self.profile_lock.take(),
            );
        }
    }
}

fn reap(
    child: Option<Child>,
    temporary: Option<TempDir>,
    profile_lock: Option<File>,
) -> std::io::Result<()> {
    if let Some(mut child) = child
        && let Err(error) = terminate(&mut child)
    {
        // Never delete a profile while its process could still be using it.
        if let Some(directory) = temporary {
            let _ = directory.keep();
        }
        // Without confirmed termination, retain exclusivity until this host
        // process exits rather than allow another library launch into the profile.
        if let Some(lock) = profile_lock {
            std::mem::forget(lock);
        }
        return Err(error);
    }
    // The lock is outside the directory being removed, so it can remain held
    // through deletion on Windows too. Explicit reuse must wait for all removal.
    if let Some(directory) = temporary {
        let path = directory.keep();
        // Chromium descendants can finish writing after the browser is reaped.
        // Retry the observed DirectoryNotEmpty race, with a fixed upper bound.
        for attempt in 0..20 {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::DirectoryNotEmpty && attempt < 19 =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
    }
    drop(profile_lock);
    Ok(())
}

fn terminate(child: &mut Child) -> std::io::Result<()> {
    if child.try_wait()?.is_none() {
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal, kill_process};
            match kill_process(Pid::from_child(child), Signal::TERM) {
                Ok(()) => {
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    while child.try_wait()?.is_none() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                Err(rustix::io::Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if child.try_wait()?.is_none() {
            child.kill()?;
        }
    }
    child.wait()?;
    Ok(())
}

impl Drop for BrowserProcess {
    fn drop(&mut self) {
        if self.child.is_some() || self.temporary.is_some() || self.profile_lock.is_some() {
            let cleanup = Cleanup {
                child: self.child.take(),
                temporary: self.temporary.take(),
                profile_lock: self.profile_lock.take(),
            };
            // The owning guard also cleans up if spawning the thread fails.
            let _ = std::thread::Builder::new()
                .name("rustcdp-cleanup".into())
                .spawn(move || {
                    let _ = cleanup.run();
                });
        }
    }
}

pub fn find_chrome() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CHROME_PATH") {
        return Ok(path.into());
    }
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            for name in [
                "google-chrome",
                "chromium",
                "chromium-browser",
                "chrome",
                "google-chrome-stable",
                "chrome.exe",
            ] {
                candidates.push(directory.join(name));
            }
        }
    }
    if cfg!(target_os = "macos") {
        candidates.extend([
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
        ]);
    }
    if cfg!(windows) {
        for base in [
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "LOCALAPPDATA",
            "PROGRAMW6432",
        ] {
            if let Some(base) = std::env::var_os(base) {
                for name in [
                    "Google/Chrome/Application/chrome.exe",
                    "Google/Chrome Beta/Application/chrome.exe",
                    "Google/Chrome Canary/Application/chrome.exe",
                ] {
                    candidates.push(PathBuf::from(&base).join(name));
                }
            }
        }
    }
    candidates
        .into_iter()
        .find(|path| {
            if !path.is_file() {
                return false;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                true
            }
        })
        .ok_or_else(|| {
            Error::Launch("Chrome not found; set CHROME_PATH or LaunchOptions::executable".into())
        })
}

use crate::{Connection, Error, Result, error::with_cleanup};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub executable: Option<PathBuf>,
    pub args: Vec<OsString>,
    /// A fixed profile is retained after shutdown.
    pub profile_dir: Option<PathBuf>,
    pub base_temp_dir: PathBuf,
    /// `None` uses "tmp"; an empty prefix is preserved.
    pub dir_prefix: Option<String>,
    pub startup_timeout: Duration,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            executable: None,
            args: Vec::new(),
            profile_dir: None,
            base_temp_dir: ".temp_chrome_profiles".into(),
            dir_prefix: None,
            startup_timeout: Duration::from_secs(15),
        }
    }
}

/// Owns exactly one child and, when applicable, its temporary profile.
/// Call `close` to await cleanup. Drop schedules a kill/reap/cleanup fallback.
pub struct BrowserProcess {
    child: Option<Child>,
    temporary: Option<TempDir>,
    cleanup: Option<tokio::sync::oneshot::Receiver<std::io::Result<()>>>,
    profile: PathBuf,
    endpoint: String,
}

impl BrowserProcess {
    pub async fn launch(options: LaunchOptions) -> Result<(Self, Connection)> {
        let mut retries = 0;
        loop {
            match Self::launch_once(options.clone()).await {
                Ok(browser) => return Ok(browser),
                Err(
                    Error::Launch(_) | Error::Timeout(_) | Error::Http(_) | Error::WebSocket(_),
                ) if retries < 2 => {
                    retries += 1;
                    #[cfg(feature = "tracing")]
                    tracing::debug!(target: "rustcdp", attempt = retries + 1, "retrying browser launch");
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn launch_once(options: LaunchOptions) -> Result<(Self, Connection)> {
        let executable = options.executable.clone().map_or_else(find_chrome, Ok)?;
        let (profile, temporary) = if let Some(path) = &options.profile_dir {
            tokio::fs::create_dir_all(path).await?;
            (tokio::fs::canonicalize(path).await?, None)
        } else {
            let prefix = options.dir_prefix.as_deref().unwrap_or("tmp");
            if prefix.contains(['/', '\\']) {
                return Err(Error::InvalidArgument(
                    "profile prefix must not contain path separators".into(),
                ));
            }
            tokio::fs::create_dir_all(&options.base_temp_dir).await?;
            let directory = tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(&options.base_temp_dir)?;
            (
                tokio::fs::canonicalize(directory.path()).await?,
                Some(directory),
            )
        };
        let port_file = profile.join("DevToolsActivePort");
        match tokio::fs::remove_file(&port_file).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut command = Command::new(executable);
        let mut profile_arg = OsString::from("--user-data-dir=");
        profile_arg.push(&profile);
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
        for arg in &options.args {
            let name = arg.to_string_lossy();
            if name.starts_with("--user-data-dir") || name.starts_with("--remote-debugging-") {
                return Err(Error::InvalidArgument(
                    "use LaunchOptions fields for profile/debugging settings".into(),
                ));
            }
            command.arg(arg);
        }
        command.arg("about:blank");
        let mut process = Self {
            child: Some(
                command
                    .spawn()
                    .map_err(|error| Error::Launch(format!("could not spawn Chrome: {error}")))?,
            ),
            temporary,
            cleanup: None,
            profile,
            endpoint: String::new(),
        };
        let connected = tokio::time::timeout(options.startup_timeout, async {
            loop {
                if let Some(status) = process
                    .child
                    .as_mut()
                    .and_then(|child| child.try_wait().transpose())
                    .transpose()?
                {
                    return Err(Error::Launch(format!("Chrome exited with {status}")));
                }
                let endpoint = match tokio::fs::read_to_string(&port_file).await {
                    Ok(contents) => {
                        let mut lines = contents.lines();
                        lines
                            .next()
                            .and_then(|s| s.parse::<u16>().ok())
                            .zip(lines.next())
                            .filter(|(_, path)| path.starts_with("/devtools/browser/"))
                            .map(|(port, path)| format!("ws://127.0.0.1:{port}{path}"))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e.into()),
                };
                if let Some(endpoint) = endpoint {
                    match Connection::connect(&endpoint).await {
                        Ok(connection) => {
                            process.endpoint = endpoint;
                            return Ok(connection);
                        }
                        Err(Error::Http(_) | Error::WebSocket(_)) => {}
                        Err(error) => return Err(error),
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| Error::Timeout("starting Chrome".into()))
        .and_then(|r| r);
        match connected {
            Ok(connection) => Ok((process, connection)),
            Err(error) => with_cleanup(Err(error), process.close().await),
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

    pub async fn close(&mut self) -> Result<()> {
        if let Some(child) = self.child.take() {
            let cleanup = Cleanup {
                child: Some(child),
                temporary: self.temporary.take(),
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
}

impl Cleanup {
    fn run(mut self) -> std::io::Result<()> {
        if let Some(child) = self.child.take() {
            let result = reap(child, self.temporary.take());
            #[cfg(feature = "tracing")]
            tracing::debug!(target: "rustcdp", success = result.is_ok(), "browser cleanup finished");
            result
        } else {
            Ok(())
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = reap(child, self.temporary.take());
        }
    }
}

fn reap(mut child: Child, temporary: Option<TempDir>) -> std::io::Result<()> {
    if let Err(error) = terminate(&mut child) {
        // Never delete a profile while its process could still be using it.
        if let Some(directory) = temporary {
            let _ = directory.keep();
        }
        return Err(error);
    }
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
        if let Some(child) = self.child.take() {
            let cleanup = Cleanup {
                child: Some(child),
                temporary: self.temporary.take(),
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

//! Real-browser ownership checks; run explicitly with `--ignored` and CHROME_PATH.
use rscdp::{Browser, Error, Result};
use std::path::{Path, PathBuf};

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
}

fn identity(browser: &Browser) -> (PathBuf, u32) {
    let process = browser
        .process()
        .expect("launched browser owns its process");
    (process.profile_dir().to_owned(), process.id().unwrap())
}

fn assert_cleaned(profile: &Path, _pid: u32) {
    assert!(!profile.exists(), "temporary profile survived Drop");
    #[cfg(target_os = "linux")]
    assert!(
        !Path::new(&format!("/proc/{_pid}")).exists(),
        "Chrome was not reaped"
    );
}

async fn failing_operation(browser: Browser) -> Result<()> {
    let _owner = browser;
    Err::<(), _>(Error::InvalidArgument("intentional failure".into()))?;
    Ok(())
}

#[test]
#[ignore = "requires Chrome; verifies synchronous process cleanup"]
fn browser_cleanup_survives_early_return_task_abort_and_runtime_shutdown() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let browser = launch().await.unwrap();
        let (profile, pid) = identity(&browser);
        assert!(failing_operation(browser).await.is_err());
        assert_cleaned(&profile, pid);

        let browser = launch().await.unwrap();
        let (profile, pid) = identity(&browser);
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _owner = browser;
            started.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_cleaned(&profile, pid);
    });
    let browser = runtime.block_on(launch()).unwrap();
    let (profile, pid) = identity(&browser);
    drop(runtime);
    drop(browser);
    assert_cleaned(&profile, pid);
}

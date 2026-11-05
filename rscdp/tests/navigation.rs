//! Real-browser navigation regressions; run with `--ignored` and CHROME_PATH.
use rscdp::{
    Browser, Page, Result,
    cdp::{
        page::{commands::GetFrameTree, events::FrameNavigated},
        target::commands::GetTargets,
    },
    page::{ClickMethod, ClickOptions, WaitUntil},
};
use std::time::Duration;

#[path = "support/navigation_server.rs"]
mod navigation_server;
use navigation_server::NavigationServer;

const TIMEOUT: Duration = Duration::from_secs(5);

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .arg("--site-per-process")
        .command_timeout(Some(TIMEOUT))
        .launch()
        .await
}

async fn observe_click(page: &Page, url: &str, milestone: WaitUntil, parented: bool) -> Result<()> {
    let tree = page
        .session()
        .execute(&GetFrameTree::new())
        .await?
        .frame_tree;
    let root = tree.frame;
    assert_eq!(root.parent_id.is_some(), parented);
    assert!(
        !root.loader_id.0.is_empty(),
        "initial document has no loader"
    );
    assert!(
        !tree.child_frames.unwrap_or_default().is_empty(),
        "fixture needs a nested child"
    );
    let link = page.require_element("#next").await?;
    let mut commits = page.session().listen::<FrameNavigated>();
    let navigation = page
        .expect_navigation()
        .wait_until(milestone)
        .timeout(TIMEOUT)
        .prepare()
        .await?;
    let deadline = navigation.deadline();
    // JavaScript click avoids native-input coordinate mapping across OOPIFs.
    tokio::time::timeout_at(
        deadline,
        link.click_with_options(ClickOptions {
            method: ClickMethod::JavaScript,
            scroll_into_view: false,
        }),
    )
    .await
    .expect("navigation click exceeded its deadline")?;
    drop(link);
    // Prove the action committed the intended document independently of the
    // observer under test, including when that observer rejects the commit.
    let committed = tokio::time::timeout_at(deadline, async {
        loop {
            let frame = commits.recv().await?.frame;
            if frame.id == root.id {
                return Ok::<_, rscdp::Error>(frame);
            }
        }
    })
    .await
    .expect("click did not commit the selected frame")?;
    assert_eq!(committed.parent_id, root.parent_id);
    assert_eq!(committed.url, url);
    assert!(!committed.loader_id.0.is_empty());
    assert_ne!(committed.loader_id, root.loader_id);

    let observed = navigation.wait().await?;
    assert_eq!(observed.frame_id, root.id);
    assert_eq!(observed.loader_id, Some(committed.loader_id));
    assert_eq!(observed.url, url);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; verifies attached iframe roots may have parents"]
async fn attached_iframe_navigation_observer_accepts_parented_root() -> Result<()> {
    let server = NavigationServer::start();
    let mut browser = launch().await?;
    let mut failures = Vec::new();
    for milestone in [WaitUntil::Commit, WaitUntil::Load] {
        // Reload the parent to create a fresh iframe document for each milestone.
        browser
            .page()
            .navigate(server.url("127.0.0.1", "/"))
            .timeout(TIMEOUT)
            .send()
            .await?;
        let targets = browser.connection().execute(&GetTargets::new()).await?;
        let target = targets
            .target_infos
            .into_iter()
            .find(|info| info.type_ == "iframe" && info.url == server.url("localhost", "/child1"))
            .expect("loaded localhost iframe was not exposed as an OOPIF target");
        let iframe = browser
            .page()
            .attach_frame_target(&target.target_id)
            .await?;
        if let Err(error) = observe_click(
            &iframe,
            &server.url("localhost", "/child2"),
            milestone,
            true,
        )
        .await
        {
            failures.push(format!("{milestone:?}: {error}"));
        }
        iframe.session().detach().await?;
    }
    browser.close().await?;
    drop(server);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; verifies top-level frame and loader selection"]
async fn top_level_navigation_observer_still_reports_selected_document() -> Result<()> {
    let server = NavigationServer::start();
    let mut browser = launch().await?;
    for milestone in [WaitUntil::Commit, WaitUntil::Load] {
        browser
            .page()
            .navigate(server.url("127.0.0.1", "/child1"))
            .timeout(TIMEOUT)
            .send()
            .await?;
        observe_click(
            browser.page(),
            &server.url("127.0.0.1", "/child2"),
            milestone,
            false,
        )
        .await?;
    }
    browser.close().await?;
    drop(server);
    Ok(())
}

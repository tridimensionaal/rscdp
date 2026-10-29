//! Real-browser CSS locator checks; run explicitly with `--ignored` and CHROME_PATH.
use rustcdp::{
    Browser, Error, Page, Result,
    page::{ElementState, Locator},
};
use serde_json::json;
use std::time::Duration;

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
}

async fn set_html(page: &Page, html: &str) -> Result<()> {
    page.call("html => { document.body.innerHTML = html; }")
        .arg(json!(html))
        .run()
        .await
}

fn locator(page: &Page, selector: &str) -> Locator {
    page.locator(selector).timeout(Duration::from_secs(5))
}

#[tokio::test]
#[ignore = "requires Chrome; verifies attached/hidden/visible geometry in an isolated world"]
async fn states_distinguish_presence_geometry_and_visibility() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<div id='target' style='display:none'>Ready</div>").await?;
    // Main-world monkey-patching must not corrupt the locator's DOM query.
    page.evaluate("document.querySelectorAll = () => { throw new Error('main world query'); };")
        .run()
        .await?;
    let target = locator(page, "#target");
    target.wait_for(ElementState::Attached).await?;
    target.wait_for(ElementState::Hidden).await?;
    assert!(matches!(
        target
            .clone()
            .timeout(Duration::from_millis(150))
            .wait_for(ElementState::Visible)
            .await,
        Err(Error::Timeout(_))
    ));
    for style in [
        "visibility:hidden; width:20px; height:20px",
        "visibility:collapse; width:20px; height:20px",
        "width:0; height:0; overflow:hidden",
    ] {
        page.call("style => { document.getElementById('target').style.cssText = style; }")
            .arg(json!(style))
            .run()
            .await?;
        target.wait_for(ElementState::Attached).await?;
        target.wait_for(ElementState::Hidden).await?;
    }
    page.call("() => { document.getElementById('target').style.cssText = 'opacity:0; width:20px; height:20px'; }")
        .run().await?;
    target.wait_for(ElementState::Visible).await?;
    // Geometry and computed visibility are not viewport or pointer readiness.
    page.call("() => { document.getElementById('target').style.cssText = 'position:absolute; left:-1000px; width:20px; height:20px'; }")
        .run().await?;
    target.wait_for(ElementState::Visible).await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies fresh resolution on node replacement and disappearance"]
async fn waits_follow_replaced_nodes_and_disappearance() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<div id='target' hidden>Old</div>").await?;
    let target = locator(page, "#target");
    let visible = target.wait_for(ElementState::Visible);
    tokio::pin!(visible);
    // Poll the lazy future while the only match is hidden. Keep the same future
    // alive across the mutation instead of starting a new wait afterward.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut visible)
            .await
            .is_err()
    );
    set_html(page, "<div id='target'>Replacement</div>").await?;
    visible.await?;
    let detached = target.wait_for(ElementState::Detached);
    tokio::pin!(detached);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut detached)
            .await
            .is_err()
    );
    set_html(page, "").await?;
    detached.await?;
    target.wait_for(ElementState::Hidden).await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies strict uniqueness and immediate invalid-selector errors"]
async fn ambiguity_and_invalid_selectors_are_not_retried() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(
        page,
        "<div class='duplicate' hidden></div><div class='duplicate' hidden></div>",
    )
    .await?;
    for state in [
        ElementState::Attached,
        ElementState::Visible,
        ElementState::Hidden,
        ElementState::Detached,
    ] {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            locator(page, ".duplicate").wait_for(state),
        )
        .await
        .expect("ambiguity must fail without waiting for the locator deadline");
        let Err(Error::InvalidArgument(message)) = result else {
            panic!("expected an ambiguity error, got {result:?}");
        };
        assert!(message.contains(".duplicate") && message.contains("2"));
    }
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        locator(page, "[").wait_for(ElementState::Hidden),
    )
    .await
    .expect("invalid CSS must fail without waiting for the locator deadline");
    assert!(matches!(result, Err(Error::JavaScript(_))));
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies checked deadlines, lazy budgets, reuse, and clone independence"]
async fn deadlines_start_when_polled_and_apply_to_each_wait() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<div id='target'>Ready</div>").await?;
    let target = locator(page, "#target");
    assert!(matches!(
        target
            .clone()
            .timeout(Duration::ZERO)
            .wait_for(ElementState::Attached)
            .await,
        Err(Error::Timeout(_))
    ));
    // Zero expires before querying even when the selector is invalid.
    assert!(matches!(
        page.locator("[")
            .timeout(Duration::ZERO)
            .wait_for(ElementState::Hidden)
            .await,
        Err(Error::Timeout(_))
    ));
    assert!(matches!(
        target
            .clone()
            .timeout(Duration::MAX)
            .wait_for(ElementState::Attached)
            .await,
        Err(Error::InvalidArgument(_))
    ));
    // The zero/overflow clone configuration must not mutate the original.
    target.wait_for(ElementState::Attached).await?;
    let lazy = target.clone().timeout(Duration::from_secs(1));
    let wait = lazy.wait_for(ElementState::Attached);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    wait.await?;

    let absent = page.locator("#later").timeout(Duration::from_millis(200));
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        absent.wait_for(ElementState::Attached),
    )
    .await
    .expect("absent locator must expire within its budget");
    let Err(Error::Timeout(message)) = result else {
        panic!("expected a deadline error, got {result:?}");
    };
    assert!(message.contains("#later") && message.contains("Attached"));
    set_html(page, "<div id='later'>Now ready</div>").await?;
    absent.wait_for(ElementState::Attached).await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies frame-scoped waits, navigation, and removed-frame errors"]
async fn frame_locators_follow_their_document_but_never_substitute_a_removed_frame() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.call(
        r#"() => new Promise(resolve => {
        document.body.innerHTML = '<div id="parent-only">Parent</div>';
        const frame = document.createElement('iframe');
        frame.id = 'child';
        frame.onload = () => resolve();
        frame.srcdoc = '<div id="target" hidden>Before</div>';
        document.body.append(frame);
    })"#,
    )
    .run()
    .await?;
    let root = page.main_frame().await?;
    let child = root.children().await?.remove(0);
    let target = child.locator("#target").timeout(Duration::from_secs(5));
    target.wait_for(ElementState::Attached).await?;
    child
        .locator("#parent-only")
        .wait_for(ElementState::Detached)
        .await?;
    locator(page, "#target")
        .wait_for(ElementState::Hidden)
        .await?;
    let visible = target.wait_for(ElementState::Visible);
    tokio::pin!(visible);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut visible)
            .await
            .is_err()
    );
    page.call(
        r#"() => new Promise(resolve => {
        const frame = document.getElementById('child');
        frame.onload = () => resolve();
        frame.srcdoc = '<div id="target">After</div>';
    })"#,
    )
    .run()
    .await?;
    visible.await?;
    let id = child.id().clone();
    drop(child);
    // Locator owns the Frame recipe, but cannot adopt a different replacement
    // iframe just because its DOM id and contents match the previous one.
    page.call(
        r#"() => new Promise(resolve => {
        document.getElementById('child').remove();
        const frame = document.createElement('iframe');
        frame.id = 'child';
        frame.onload = () => resolve();
        frame.srcdoc = '<div id="target">Different frame</div>';
        document.body.append(frame);
    })"#,
    )
    .run()
    .await?;
    for state in [
        ElementState::Attached,
        ElementState::Hidden,
        ElementState::Detached,
    ] {
        assert!(
            matches!(target.wait_for(state).await, Err(Error::FrameNotFound(missing)) if missing == id)
        );
    }
    browser.close().await
}

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

async fn clicks(page: &Page) -> Result<u64> {
    page.evaluate("Number(document.body.dataset.clicks || 0)")
        .json()
        .await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies movement, native fieldset disabling, and pointer covering"]
async fn click_waits_for_stability_enabled_state_and_hit_testing() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, r#"<fieldset disabled><button id="target" style="position:absolute;left:20px;top:20px;width:120px;height:50px"><span>Buy</span></button></fieldset><div id="cover" style="position:fixed;inset:0;z-index:99"></div>"#).await?;
    page.call(
        r#"() => {
        document.body.dataset.clicks = '0';
        const button = document.getElementById('target');
        button.onclick = () => document.body.dataset.clicks++;
        window.moving = true;
        let x = 20;
        const move = () => {
            if (!window.moving) return;
            button.style.left = (x = x === 20 ? 80 : 20) + 'px';
            requestAnimationFrame(move);
        };
        requestAnimationFrame(move);
    }"#,
    )
    .run()
    .await?;
    let target = locator(page, "#target");
    let click = target.click();
    tokio::pin!(click);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut click)
            .await
            .is_err()
    );
    assert_eq!(clicks(page).await?, 0);
    page.call("() => { document.querySelector('fieldset').disabled = false; document.getElementById('cover').remove(); }").run().await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut click)
            .await
            .is_err()
    );
    assert_eq!(clicks(page).await?, 0);
    page.call("() => { window.moving = false; }").run().await?;
    click.await?;
    assert_eq!(clicks(page).await?, 1);
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies viewport clipping and descendant hit acceptance"]
async fn click_uses_a_point_inside_the_visible_clipped_rectangle() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, r#"<button id="target" style="position:fixed;left:-180px;top:40px;width:200px;height:60px;padding:0;border:0"><span style="display:block;width:100%;height:100%">Buy</span></button>"#).await?;
    page.call("() => { document.getElementById('target').onclick = e => { document.body.dataset.clicks = '1'; document.body.dataset.x = e.clientX; }; }").run().await?;
    locator(page, "#target").click().await?;
    assert_eq!(clicks(page).await?, 1);
    let x: f64 = page
        .evaluate("Number(document.body.dataset.x)")
        .json()
        .await?;
    assert!((0.0..20.0).contains(&x));
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies fresh strict resolution after replacement during readiness"]
async fn click_re_resolves_a_replaced_element_before_dispatch() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<button id='target' disabled>Old</button>").await?;
    page.call("() => { document.getElementById('target').onclick = () => { document.body.dataset.oldClick = 'yes'; }; }").run().await?;
    let target = locator(page, "#target");
    let click = target.click();
    tokio::pin!(click);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut click)
            .await
            .is_err()
    );
    page.call(
        r#"() => {
        const old = document.getElementById('target');
        old.disabled = false;
        requestAnimationFrame(() => {
            const replacement = document.createElement('button');
            replacement.id = 'target';
            replacement.textContent = 'New';
            replacement.onclick = () => document.body.dataset.clicks = '1';
            old.replaceWith(replacement);
        });
    }"#,
    )
    .run()
    .await?;
    click.await?;
    assert_eq!(clicks(page).await?, 1);
    assert!(
        !page
            .evaluate("document.body.dataset.oldClick === 'yes'")
            .json::<bool>()
            .await?
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies nested iframe offsets, borders, and ancestor hit testing"]
async fn click_in_nested_frames_checks_the_parent_overlay() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.call(r#"() => new Promise(resolve => {
        document.body.dataset.clicks = '0';
        const outer = document.createElement('iframe');
        outer.style.cssText = 'margin:65px;border:7px solid black;width:360px;height:260px';
        outer.onload = () => {
            const inner = outer.contentDocument.createElement('iframe');
            inner.style.cssText = 'margin:25px;border:5px solid black;width:220px;height:150px';
            inner.onload = () => {
                inner.contentDocument.querySelector('button').onclick = () => document.body.dataset.clicks++;
                resolve();
            };
            inner.srcdoc = '<button style="margin:20px;width:100px;height:50px">Buy</button>';
            outer.contentDocument.body.append(inner);
        };
        outer.srcdoc = '<body></body>';
        document.body.append(outer);
        const cover = document.createElement('div');
        cover.id = 'cover';
        cover.style.cssText = 'position:fixed;inset:0;z-index:99';
        document.body.append(cover);
    })"#).run().await?;
    let root = page.main_frame().await?;
    let outer = root.children().await?.remove(0);
    let inner = outer.children().await?.remove(0);
    let target = inner.locator("button").timeout(Duration::from_secs(5));
    let click = target.click();
    tokio::pin!(click);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut click)
            .await
            .is_err()
    );
    assert_eq!(clicks(page).await?, 0);
    page.call("() => document.getElementById('cover').remove()")
        .run()
        .await?;
    click.await?;
    assert_eq!(clicks(page).await?, 1);
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies unsupported iframe transforms and cross-origin ancestry"]
async fn click_rejects_unsupported_frame_coordinate_mappings() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for sandbox in [false, true] {
        page.call(
            r#"sandbox => new Promise(resolve => {
            document.body.innerHTML = '';
            const frame = document.createElement('iframe');
            if (sandbox) frame.setAttribute('sandbox', 'allow-scripts');
            else frame.style.transform = 'scale(1.2)';
            frame.onload = () => resolve();
            frame.srcdoc = '<button>Buy</button>';
            document.body.append(frame);
        })"#,
        )
        .arg(json!(sandbox))
        .run()
        .await?;
        let root = page.main_frame().await?;
        let children = root.children().await?;
        let target = if let Some(child) = children.into_iter().next() {
            child.locator("button")
        } else {
            // Chrome can isolate a sandboxed frame into a separate target.
            // Explicitly attach it and exercise the target-kind refusal too.
            let info = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let targets = page
                        .session()
                        .connection()
                        .execute(&rustcdp::cdp::target::commands::GetTargets::new())
                        .await?;
                    if let Some(info) = targets
                        .target_infos
                        .into_iter()
                        .find(|info| info.type_ == "iframe")
                    {
                        break Ok::<_, Error>(info);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("sandboxed iframe target was not discovered")?;
            page.attach_frame_target(&info.target_id)
                .await?
                .locator("button")
        };
        let error = target
            .timeout(Duration::from_secs(3))
            .click()
            .await
            .unwrap_err();
        assert!(matches!(error, Error::InvalidArgument(_)), "{error}");
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies pre-dispatch deadlines and selector/readiness diagnostics"]
async fn click_deadlines_reject_zero_overflow_and_covered_targets() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(
        page,
        "<button id='target'>Buy</button><div style='position:fixed;inset:0;z-index:99'></div>",
    )
    .await?;
    for selector in ["#target", "["] {
        assert!(matches!(
            page.locator(selector).timeout(Duration::ZERO).click().await,
            Err(Error::Timeout(_))
        ));
    }
    assert!(matches!(
        page.locator("#target").timeout(Duration::MAX).click().await,
        Err(Error::InvalidArgument(_))
    ));
    let error = page
        .locator("#target")
        .timeout(Duration::from_millis(500))
        .click()
        .await
        .unwrap_err();
    let Error::Timeout(message) = error else {
        panic!("{error}");
    };
    assert!(
        message.contains("#target") && message.contains("covered"),
        "{message}"
    );
    assert_eq!(clicks(page).await?, 0);
    set_html(
        page,
        "<button class='duplicate'>One</button><button class='duplicate'>Two</button>",
    )
    .await?;
    assert!(matches!(
        locator(page, ".duplicate").click().await,
        Err(Error::JavaScript(_))
    ));
    assert!(matches!(
        locator(page, "[").click().await,
        Err(Error::JavaScript(_))
    ));
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies timeout after press never replays input and release continues"]
async fn click_timeout_after_dispatch_still_releases_once() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<button id='target'>Buy</button>").await?;
    page.call(
        r#"() => {
        document.body.dataset.presses = '0';
        document.body.dataset.releases = '0';
        document.body.dataset.clicks = '0';
        const button = document.getElementById('target');
        button.onmousedown = () => {
            document.body.dataset.presses++;
            const end = performance.now() + 1500;
            while (performance.now() < end) {}
        };
        button.onmouseup = () => document.body.dataset.releases++;
        button.onclick = () => document.body.dataset.clicks++;
    }"#,
    )
    .run()
    .await?;
    let error = page
        .locator("#target")
        .timeout(Duration::from_millis(700))
        .click()
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Timeout(_)), "{error}");
    // A subsequent command can wait behind the deliberately stalled renderer.
    // Poll until the owned release arrives; do not start another locator action.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let released: u64 = page
                .evaluate("Number(document.body.dataset.releases)")
                .json()
                .await?;
            if released == 1 {
                break Ok::<(), Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("owned mouse release did not finish")?;
    assert_eq!(
        page.evaluate("Number(document.body.dataset.presses)")
            .json::<u64>()
            .await?,
        1
    );
    assert_eq!(clicks(page).await?, 1);
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies bounded RAF sampling does not detach a usable session"]
async fn click_with_unavailable_animation_frames_times_out_without_detaching() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<button id='target'>Buy</button>").await?;
    // Simulate RAF throttling in the library's world; local timers still work.
    page.evaluate(
        "globalThis.savedRAF = requestAnimationFrame; globalThis.requestAnimationFrame = () => 0;",
    )
    .world(rustcdp::page::EvaluationWorld::Isolated)
    .run()
    .await?;
    let result = page
        .locator("#target")
        .timeout(Duration::from_millis(600))
        .click()
        .await;
    assert!(matches!(result, Err(Error::Timeout(_))));
    page.evaluate(
        "globalThis.requestAnimationFrame = globalThis.savedRAF; delete globalThis.savedRAF;",
    )
    .world(rustcdp::page::EvaluationWorld::Isolated)
    .run()
    .await?;
    // A still-running final sample also has a 250ms local timer. Allow it to
    // settle, then exercise the same session instead of merely checking a flag.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(page.call("() => 42").json::<u64>().await?, 42);
    assert!(!page.session().is_closed());
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies overflow candidates respect actual containing blocks"]
async fn overflow_candidates_follow_browser_hit_testing() -> Result<()> {
    let browser = launch().await?;
    let page = browser.page();
    for position in ["fixed", "absolute", "static"] {
        set_html(page, &format!(r#"<div style="overflow:hidden;width:10px;height:40px">
            <button id="target" style="position:{position};left:100px;top:100px;width:200px;height:30px;padding:0;border:0">Click</button>
        </div>"#)).await?;
        page.call("() => { document.body.dataset.clicks = '0'; document.getElementById('target').onclick = () => document.body.dataset.clicks++; }").run().await?;
        page.locator("#target")
            .timeout(Duration::from_secs(1))
            .click()
            .await?;
        assert_eq!(clicks(page).await?, 1, "{position}");
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; verifies transforms above overflow clips fail explicitly"]
async fn overflow_clip_with_transformed_ancestor_is_rejected() -> Result<()> {
    let browser = launch().await?;
    let page = browser.page();
    set_html(page, r#"<div style="transform:scale(2);transform-origin:0 0;width:100px">
        <div style="overflow:hidden;width:100px;height:50px">
            <button id="target" style="display:block;margin-left:70px;width:20px;height:20px;padding:0;border:0">X</button>
        </div>
    </div>"#).await?;
    assert!(
        matches!(page.locator("#target").timeout(Duration::from_secs(1)).click().await,
        Err(Error::InvalidArgument(message)) if message.contains("transformed clipping"))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; verifies fill readiness, replacement, Unicode and empty replacement"]
async fn fill_waits_for_a_replacement_input_then_replaces_unicode_and_empty_text() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<input id='target' hidden disabled value='original'>").await?;
    page.call(
        r#"() => {
        globalThis.oldInput = document.getElementById('target');
        document.body.dataset.edits = '0';
        document.body.oninput = () => document.body.dataset.edits++;
    }"#,
    )
    .run()
    .await?;
    let target = locator(page, "#target");
    let text = "mañana 雪 🦀 'quoted'";
    let fill = target.fill(text);
    tokio::pin!(fill);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut fill)
            .await
            .is_err()
    );
    page.call(
        r#"() => {
        const replacement = document.createElement('input');
        replacement.id = 'target';
        replacement.disabled = true;
        oldInput.replaceWith(replacement);
    }"#,
    )
    .run()
    .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(150), &mut fill)
            .await
            .is_err()
    );
    assert_eq!(
        page.evaluate("Number(document.body.dataset.edits)")
            .json::<u64>()
            .await?,
        0
    );
    page.call("() => { document.getElementById('target').disabled = false; }")
        .run()
        .await?;
    fill.await?;
    assert_eq!(
        page.evaluate("[document.getElementById('target').value, oldInput.value, Number(document.body.dataset.edits)]")
            .json::<serde_json::Value>().await?,
        json!([text, "original", 1])
    );
    target.fill("").await?;
    assert_eq!(
        page.evaluate(
            "[document.getElementById('target').value, Number(document.body.dataset.edits)]"
        )
        .json::<serde_json::Value>()
        .await?,
        json!(["", 2])
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies readonly waits and unsupported fill types fail without editing"]
async fn fill_times_out_on_readonly_and_rejects_unsupported_controls() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<input id='target' readonly value='keep'>").await?;
    assert!(matches!(
        page.locator("#target")
            .timeout(Duration::from_millis(300))
            .fill("changed")
            .await,
        Err(Error::Timeout(_))
    ));
    assert_eq!(
        page.evaluate("document.getElementById('target').value")
            .json::<String>()
            .await?,
        "keep"
    );
    for html in [
        "<input id='target' type='checkbox'>",
        "<input id='target' type='number' value='42'>",
        "<div id='target' contenteditable>keep</div>",
    ] {
        set_html(page, html).await?;
        page.call("() => { document.body.dataset.effects = '0'; document.body.oninput = document.body.onfocusin = () => document.body.dataset.effects++; }")
            .run().await?;
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            locator(page, "#target").fill("changed"),
        )
        .await
        .expect("unsupported control must fail before the readiness deadline");
        assert!(
            matches!(result, Err(Error::InvalidArgument(_))),
            "{result:?}"
        );
        assert_eq!(
            page.evaluate("Number(document.body.dataset.effects)")
                .json::<u64>()
                .await?,
            0
        );
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies cancellation after insertion starts never causes a second edit"]
async fn canceling_fill_after_its_first_input_event_does_not_repeat_the_edit() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    set_html(page, "<input id='target' value='old'>").await?;
    // Subscribe before triggering a native edit. A binding event reaches Rust
    // while the input handler still holds the renderer, making abort observable.
    let mut events = page.session().subscribe_with(
        rustcdp::connection::EventOptions::default().methods(["Runtime.bindingCalled"]),
    )?;
    page.session()
        .execute_raw("Runtime.addBinding", json!({"name": "fillEdited"}))
        .await?;
    page.call(
        r#"() => {
        document.body.dataset.edits = '0';
        document.getElementById('target').oninput = () => {
            document.body.dataset.edits++;
            fillEdited(document.body.dataset.edits);
            const end = performance.now() + 1500;
            while (performance.now() < end) {}
        };
    }"#,
    )
    .run()
    .await?;
    let target = locator(page, "#target");
    let task = tokio::spawn(async move { target.fill("new 雪").await });
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("fill never produced its first input event")?;
    assert_eq!(event.params["name"], "fillEdited");
    assert_eq!(event.params["payload"], "1");
    assert!(
        !task.is_finished(),
        "input handler must still hold the dispatched edit"
    );
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // This command waits behind the deliberately stalled input handler. The
    // original owned insertion can finish, but must not dispatch another edit.
    assert_eq!(
        page.evaluate(
            "[document.getElementById('target').value, Number(document.body.dataset.edits)]"
        )
        .json::<serde_json::Value>()
        .await?,
        json!(["new 雪", 1])
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(400), events.recv())
            .await
            .is_err(),
        "a canceled fill emitted another edit"
    );
    assert_eq!(
        page.evaluate("Number(document.body.dataset.edits)")
            .json::<u64>()
            .await?,
        1
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies a late focus response cannot admit text insertion"]
async fn fill_deadline_during_focus_prevents_late_insertion() -> Result<()> {
    let browser = launch().await?;
    let page = browser.page();
    page.session()
        .execute_raw("Page.bringToFront", json!({}))
        .await?;
    assert!(page.evaluate("document.hasFocus()").json::<bool>().await?);
    set_html(page, "<input id='target' value='old'>").await?;
    let mut events = page.session().subscribe_with(
        rustcdp::connection::EventOptions::default().methods(["Runtime.bindingCalled"]),
    )?;
    page.session()
        .execute_raw("Runtime.addBinding", json!({"name": "fillFocused"}))
        .await?;
    page.call(
        r#"() => {
        document.body.dataset.edits = '0';
        const input = document.getElementById('target');
        input.oninput = () => document.body.dataset.edits++;
        input.onfocus = () => {
            fillFocused('focused');
            const end = performance.now() + 1500;
            while (performance.now() < end) {}
        };
    }"#,
    )
    .run()
    .await?;
    let input = page.locator("#target").timeout(Duration::from_millis(700));
    let task = tokio::spawn(async move { input.fill("new").await });
    let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("focus was not dispatched")?;
    assert_eq!(event.params["name"], "fillFocused");
    assert!(matches!(task.await.unwrap(), Err(Error::Timeout(_))));
    // This evaluation queues behind the focus handler. The owned preparation
    // may finish, but its expired deadline must prevent a later InsertText.
    assert_eq!(
        page.evaluate(
            "[document.getElementById('target').value, Number(document.body.dataset.edits)]"
        )
        .json::<serde_json::Value>()
        .await?,
        json!(["old", 0])
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        page.evaluate("document.getElementById('target').value")
            .json::<String>()
            .await?,
        "old"
    );
    Ok(())
}

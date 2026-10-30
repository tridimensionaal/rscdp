//! Real-browser native form checks; run explicitly with `--ignored` and CHROME_PATH.
use rustcdp::{Browser, Error, Result};
use serde_json::{Value, json};

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies exact values and native input/change events"]
async fn select_option_preserves_exact_values_and_dispatches_ordered_events() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    let value = "'\"\\\n雪 🦀'); throw new Error('injected'); //";
    page.call(r#"function(value) {
        document.body.innerHTML = '<select id=target><option value=keep>Keep</option><option>Choose</option><option value="">Empty</option></select>';
        const select = document.querySelector('#target');
        select.options[1].value = value;
        globalThis.formEvents = [];
        for (const type of ['input', 'change']) {
            document.body.addEventListener(type, event => {
                formEvents.push([event.type, event.target.value, event.bubbles, event.isTrusted]);
            });
        }
    }"#).arg(json!(value)).run().await?;
    let element = page.require_element("#target").await?;
    element.select_option(value).await?;
    assert_eq!(
        page.evaluate("document.querySelector('#target').selectedIndex")
            .json::<u64>()
            .await?,
        1
    );
    assert_eq!(
        page.evaluate("globalThis.formEvents")
            .json::<Value>()
            .await?,
        json!([
            ["input", value, true, false],
            ["change", value, true, false],
        ])
    );
    element.select_option("").await?;
    assert_eq!(
        page.evaluate("document.querySelector('#target').selectedIndex")
            .json::<u64>()
            .await?,
        2
    );
    assert_eq!(
        page.evaluate("globalThis.formEvents.slice(2)")
            .json::<Value>()
            .await?,
        json!([["input", "", true, false], ["change", "", true, false],])
    );
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies invalid selection leaves state unchanged and emits no events"]
async fn select_option_rejects_invalid_controls_and_values_before_mutation() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    let fixtures = [
        ("wrong node", "<input id=target value=keep>", "next"),
        (
            "multiple",
            "<select id=target multiple><option value=keep selected>Keep</option><option value=next>Next</option></select>",
            "next",
        ),
        (
            "disabled select",
            "<select id=target disabled><option value=keep>Keep</option><option value=next>Next</option></select>",
            "next",
        ),
        (
            "disabled fieldset",
            "<fieldset disabled><select id=target><option value=keep>Keep</option><option value=next>Next</option></select></fieldset>",
            "next",
        ),
        (
            "disabled option",
            "<select id=target><option value=keep>Keep</option><option value=next disabled>Next</option></select>",
            "next",
        ),
        (
            "disabled optgroup",
            "<select id=target><option value=keep>Keep</option><optgroup label=Group disabled><option value=next>Next</option></optgroup></select>",
            "next",
        ),
        (
            "missing value",
            "<select id=target><option value=keep>Keep</option><option value=next>Next</option></select>",
            "missing",
        ),
        (
            "duplicate value",
            "<select id=target><option value=keep>Keep</option><option value=next>First</option><option value=next disabled>Second</option></select>",
            "next",
        ),
        (
            "exact value only",
            "<select id=target><option value=keep>Keep</option><option value=' next '>Next</option></select>",
            "next",
        ),
    ];
    for (label, html, value) in fixtures {
        page.call(
            r#"function(html) {
            document.body.innerHTML = html;
            globalThis.formEventCount = 0;
            const target = document.querySelector('#target');
            for (const type of ['input', 'change']) {
                target.addEventListener(type, () => globalThis.formEventCount++);
            }
        }"#,
        )
        .arg(json!(html))
        .run()
        .await?;
        let snapshot = "({ value: document.querySelector('#target').value, selected: Array.from(document.querySelector('#target').options || [], option => option.selected), events: globalThis.formEventCount })";
        let before = page.evaluate(snapshot).json::<Value>().await?;
        let element = page.require_element("#target").await?;
        let error = element.select_option(value).await.unwrap_err();
        assert!(
            matches!(error, Error::InvalidArgument(_)),
            "{label}: {error:?}"
        );
        assert_eq!(
            page.evaluate(snapshot).json::<Value>().await?,
            before,
            "{label}"
        );
        element.release().await?;
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies disconnected controls do not mutate or retarget"]
async fn select_option_rejects_a_disconnected_handle_and_preserves_its_replacement() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(r#"
        document.body.innerHTML = '<select id=target><option value=keep>Keep</option><option value=next>Next</option></select>';
        globalThis.formTarget = document.querySelector('#target');
        globalThis.formEventCount = 0;
        formTarget.addEventListener('input', () => formEventCount++);
        formTarget.addEventListener('change', () => formEventCount++);
    "#).run().await?;
    let element = page.require_element("#target").await?;
    page.evaluate("formTarget.replaceWith(formTarget.cloneNode(true))")
        .run()
        .await?;
    assert!(matches!(
        element.select_option("next").await,
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(
        page.evaluate(
            "[formTarget.value, document.querySelector('#target').value, formEventCount]"
        )
        .json::<Value>()
        .await?,
        json!(["keep", "keep", 0])
    );
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies handler reversal/detachment fails without replay"]
async fn select_option_reports_handler_reversal_or_detachment_without_replaying() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for event_type in ["input", "change"] {
        for detach in [false, true] {
            page.call(r#"function(eventType, detach) {
                document.body.innerHTML = '<select id=target><option value=keep>Keep</option><option value=next>Next</option></select>';
                globalThis.formTarget = document.querySelector('#target');
                globalThis.formEvents = [];
                for (const type of ['input', 'change']) {
                    formTarget.addEventListener(type, () => {
                        formEvents.push(type);
                        if (type === eventType) {
                            if (detach) formTarget.remove();
                            else formTarget.value = 'keep';
                        }
                    });
                }
            }"#).args([json!(event_type), json!(detach)]).run().await?;
            let element = page.require_element("#target").await?;
            let error = element.select_option("next").await.unwrap_err();
            assert!(
                matches!(error, Error::InvalidArgument(_)),
                "{event_type}, detach={detach}: {error:?}"
            );
            assert_eq!(
                page.evaluate("globalThis.formEvents")
                    .json::<Value>()
                    .await?,
                if event_type == "input" {
                    json!(["input"])
                } else {
                    json!(["input", "change"])
                }
            );
            assert_eq!(
                page.evaluate("[formTarget.isConnected, formTarget.value]")
                    .json::<Value>()
                    .await?,
                if detach {
                    json!([false, "next"])
                } else {
                    json!([true, "keep"])
                }
            );
            element.release().await?;
        }
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies locator attachment, hidden native selects and deadlines"]
async fn select_locator_waits_for_attachment_and_supports_hidden_controls() -> Result<()> {
    use std::time::Duration;
    let browser = launch().await?;
    let page = browser.page();
    let select = page.locator("#digest").timeout(Duration::from_secs(5));
    let selection = select.select_option("weekly");
    tokio::pin!(selection);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut selection)
            .await
            .is_err()
    );
    page.call("() => { document.body.innerHTML = '<select id=digest hidden><option>daily</option><option>weekly</option></select>'; }").run().await?;
    selection.await?;
    assert_eq!(
        page.evaluate("document.getElementById('digest').value")
            .json::<String>()
            .await?,
        "weekly"
    );
    assert!(matches!(
        page.locator("#digest")
            .timeout(Duration::ZERO)
            .select_option("daily")
            .await,
        Err(Error::Timeout(_))
    ));
    page.call("() => { document.getElementById('digest').disabled = true; }")
        .run()
        .await?;
    assert!(matches!(
        select.select_option("daily").await,
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(
        page.evaluate("document.getElementById('digest').value")
            .json::<String>()
            .await?,
        "weekly"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; already-correct disabled, covered and hidden checkboxes receive no input"]
async fn set_checked_skips_input_when_already_correct() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(r#"
        document.body.innerHTML = '<input id=yes type=checkbox checked disabled><input id=no type=checkbox style="display:none"><div style="position:fixed;inset:0;z-index:1000"></div>';
        globalThis.checkboxClicks = 0;
        document.addEventListener('click', () => checkboxClicks++);
    "#).run().await?;
    for (selector, checked) in [("#yes", true), ("#no", false)] {
        let element = page.require_element(selector).await?;
        element.set_checked(checked).await?;
        page.locator(selector).set_checked(checked).await?;
        element.release().await?;
    }
    assert_eq!(page.evaluate("checkboxClicks").json::<u64>().await?, 0);
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies one native click per checkbox state change"]
async fn set_checked_changes_state_once_and_uses_native_input() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(r#"
        document.body.innerHTML = '<input id=target type=checkbox style="width:30px;height:30px">';
        globalThis.checkboxEvents = [];
        const target = document.querySelector('#target');
        for (const type of ['click', 'input', 'change'])
            target.addEventListener(type, event => checkboxEvents.push([type, event.isTrusted, target.checked]));
    "#).run().await?;
    let element = page.require_element("#target").await?;
    element.set_checked(true).await?;
    page.locator("#target").set_checked(false).await?;
    page.locator("#target").set_checked(true).await?;
    // The direct handle must work without the selector that acquired it.
    page.evaluate("document.querySelector('#target').removeAttribute('id')")
        .run()
        .await?;
    element.set_checked(false).await?;
    assert_eq!(
        page.evaluate("checkboxEvents").json::<Value>().await?,
        json!([
            ["click", true, true],
            ["input", true, true],
            ["change", true, true],
            ["click", true, false],
            ["input", true, false],
            ["change", true, false],
            ["click", true, true],
            ["input", true, true],
            ["change", true, true],
            ["click", true, false],
            ["input", true, false],
            ["change", true, false],
        ])
    );
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; rejects radio, text and ARIA controls even when state appears correct"]
async fn set_checked_rejects_non_native_checkboxes_without_input() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for html in [
        "<input id=target type=radio checked>",
        "<input id=target type=text>",
        "<div id=target role=checkbox aria-checked=true tabindex=0>Custom</div>",
    ] {
        page.call(
            r#"function(html) {
            document.body.innerHTML = html;
            globalThis.checkboxClicks = 0;
            document.querySelector('#target').addEventListener('click', () => checkboxClicks++);
        }"#,
        )
        .arg(json!(html))
        .run()
        .await?;
        let element = page.require_element("#target").await?;
        for checked in [false, true] {
            assert!(matches!(
                element.set_checked(checked).await,
                Err(Error::InvalidArgument(_))
            ));
            assert!(matches!(
                page.locator("#target").set_checked(checked).await,
                Err(Error::InvalidArgument(_))
            ));
        }
        assert_eq!(page.evaluate("checkboxClicks").json::<u64>().await?, 0);
        element.release().await?;
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; prevented/reverted checkbox changes fail after one click without replay"]
async fn set_checked_reports_prevented_or_reverted_changes_without_replaying() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for locator in [false, true] {
        for prevent in [false, true] {
            page.call(r#"function(prevent) {
                document.body.innerHTML = '<input id=target type=checkbox style="width:30px;height:30px">';
                globalThis.checkboxClicks = 0;
                const target = document.querySelector('#target');
                target.addEventListener('click', event => {
                    checkboxClicks++;
                    if (prevent) event.preventDefault();
                });
                if (!prevent) target.addEventListener('change', () => { target.checked = false; });
            }"#).arg(json!(prevent)).run().await?;
            let element = page.require_element("#target").await?;
            let outcome = if locator {
                page.locator("#target").set_checked(true).await
            } else {
                element.set_checked(true).await
            };
            assert!(
                matches!(outcome, Err(Error::InvalidArgument(_))),
                "{outcome:?}"
            );
            assert_eq!(
                page.evaluate("[document.querySelector('#target').checked, checkboxClicks]")
                    .json::<Value>()
                    .await?,
                json!([false, 1])
            );
            element.release().await?;
        }
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; blocked checkbox changes expire or cancel without delayed input"]
async fn set_checked_respects_readiness_and_cancellation() -> Result<()> {
    use std::time::Duration;
    let mut browser = launch().await?;
    let page = browser.page();
    for disabled in [false, true] {
        page.call(r#"function(disabled) {
            document.body.innerHTML = '<input id=target type=checkbox style="width:30px;height:30px"><div id=cover style="position:fixed;inset:0;z-index:1000"></div>';
            const target = document.querySelector('#target');
            target.disabled = disabled;
            if (disabled) document.querySelector('#cover').remove();
            globalThis.checkboxClicks = 0;
            target.addEventListener('click', () => checkboxClicks++);
        }"#).arg(json!(disabled)).run().await?;
        let element = page.require_element("#target").await?;
        assert!(matches!(
            page.locator("#target")
                .timeout(Duration::from_millis(180))
                .set_checked(true)
                .await,
            Err(Error::Timeout(_))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(180), element.set_checked(true))
                .await
                .is_err()
        );
        page.evaluate("document.querySelector('#target').disabled = false; document.querySelector('#cover')?.remove()").run().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            page.evaluate("[document.querySelector('#target').checked, checkboxClicks]")
                .json::<Value>()
                .await?,
            json!([false, 0])
        );
        element.release().await?;
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; direct handles retain identity while locators can resolve replacements before input"]
async fn set_checked_does_not_retarget_a_direct_handle() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(r#"
        document.body.innerHTML = '<input id=target type=checkbox disabled style="width:30px;height:30px">';
        globalThis.oldCheckbox = document.querySelector('#target');
        globalThis.checkboxClicks = 0;
        document.body.addEventListener('click', () => checkboxClicks++);
    "#).run().await?;
    let element = page.require_element("#target").await?;
    page.evaluate(
        r#"
        setTimeout(() => {
            const replacement = oldCheckbox.cloneNode(true);
            replacement.disabled = false;
            oldCheckbox.replaceWith(replacement);
        }, 100);
    "#,
    )
    .run()
    .await?;
    assert!(element.set_checked(true).await.is_err());
    assert_eq!(
        page.evaluate(
            "[oldCheckbox.checked, document.querySelector('#target').checked, checkboxClicks]"
        )
        .json::<Value>()
        .await?,
        json!([false, false, 0])
    );
    page.locator("#target").set_checked(true).await?;
    assert_eq!(
        page.evaluate(
            "[oldCheckbox.checked, document.querySelector('#target').checked, checkboxClicks]"
        )
        .json::<Value>()
        .await?,
        json!([false, true, 1])
    );
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; deadline expiration after input starts cannot cause a second checkbox click"]
async fn set_checked_timeout_after_dispatch_never_replays() -> Result<()> {
    use std::time::Duration;
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        r#"
        document.body.innerHTML = '<input id=target type=checkbox style="width:30px;height:30px">';
        globalThis.checkboxClicks = 0;
        document.querySelector('#target').addEventListener('click', () => {
            checkboxClicks++;
            const until = performance.now() + 600;
            while (performance.now() < until) {}
        });
    "#,
    )
    .run()
    .await?;
    assert!(matches!(
        page.locator("#target")
            .timeout(Duration::from_millis(400))
            .set_checked(true)
            .await,
        Err(Error::Timeout(_))
    ));
    // A protocol evaluation waits for the synchronous handler to finish. A later
    // task turn also allows the owned mouse release to complete after timeout.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        page.evaluate("[document.querySelector('#target').checked, checkboxClicks]")
            .json::<Value>()
            .await?,
        json!([true, 1])
    );
    browser.close().await
}

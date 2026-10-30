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

//! Real-browser function-call checks; run explicitly with `--ignored` and CHROME_PATH.
use rustcdp::{Browser, Error, Page, Result, page::EvaluationWorld};
use serde_json::{Value, json};
use std::time::Duration;

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
}

async fn wait_until(page: &Page, expression: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if page.evaluate(expression).json::<bool>().await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| Error::Timeout(format!("waiting for {expression}")))?
}

#[tokio::test]
#[ignore = "requires Chrome; verifies separate JSON arguments and append order"]
async fn arguments_and_promises_round_trip_without_source_interpolation() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    let text = "'\"\\\n雪 🦀'); throw new Error('injected'); //";
    let result: Value = page
        .call("async (...values) => { await Promise.resolve(); return values; }")
        .arg(json!(text))
        .args([Value::Null, json!([1, false, "árbol"])])
        .args([])
        .arg(json!({"quoted\"key": {"value": "λ"}}))
        .json()
        .await?;
    assert_eq!(
        result,
        json!([text, null, [1, false, "árbol"], {"quoted\"key": {"value": "λ"}}])
    );
    assert_eq!(page.call("() => null").json::<Value>().await?, Value::Null);
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies error and discarded-result semantics"]
async fn exceptions_rejections_and_undefined_keep_evaluation_semantics() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for source in [
        "() => { throw new Error('sync failure'); }",
        "async () => { throw new Error('promise failure'); }",
    ] {
        assert!(matches!(
            page.call(source).json::<Value>().await,
            Err(Error::JavaScript(_))
        ));
        assert!(matches!(
            page.call(source).run().await,
            Err(Error::JavaScript(_))
        ));
    }
    page.call("() => undefined").run().await?;
    page.call("() => ({ discarded: true })").run().await?;
    assert!(matches!(
        page.call("() => undefined").json::<Value>().await,
        Err(Error::NonJsonValue(_))
    ));
    // The shared executor must preserve existing expression and remote behavior.
    assert_eq!(page.evaluate("6 * 7").json::<u64>().await?, 42);
    let remote = page.evaluate("({ retained: true })").remote().await?;
    assert!(remote.object_id().is_some());
    remote.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies main/isolated worlds and same-process frame contexts"]
async fn page_and_frame_calls_resolve_the_selected_world() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        r#"new Promise(resolve => {
        globalThis.callMarker = 'parent';
        const frame = document.createElement('iframe');
        frame.id = 'child';
        frame.onload = () => {
            frame.contentWindow.callMarker = 'child';
            resolve();
        };
        frame.srcdoc = '<title>Child</title>';
        document.body.append(frame);
    })"#,
    )
    .run()
    .await?;
    assert_eq!(
        page.call("() => callMarker").json::<String>().await?,
        "parent"
    );
    assert_eq!(
        page.call("() => typeof globalThis.callMarker")
            .world(EvaluationWorld::Isolated)
            .json::<String>()
            .await?,
        "undefined"
    );
    let root = page.main_frame().await?;
    let children = root.children().await?;
    assert_eq!(children.len(), 1);
    let child = &children[0];
    assert_eq!(
        child
            .call("suffix => callMarker + suffix")
            .arg(json!("!"))
            .json::<String>()
            .await?,
        "child!"
    );
    assert_eq!(
        child
            .call("() => [typeof globalThis.callMarker, document.title]")
            .world(EvaluationWorld::Isolated)
            .json::<Value>()
            .await?,
        json!(["undefined", "Child"])
    );
    child
        .call("value => { document.title = value; }")
        .arg(json!("Updated"))
        .run()
        .await?;
    assert_eq!(
        child.evaluate("document.title").json::<String>().await?,
        "Updated"
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies context loss never replays user functions"]
async fn replacing_a_frame_context_does_not_replay_a_pending_call() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        r#"new Promise(resolve => {
        globalThis.callCount = 0;
        const frame = document.createElement('iframe');
        frame.id = 'child';
        frame.onload = () => resolve();
        frame.srcdoc = '<title>Before</title>';
        document.body.append(frame);
    })"#,
    )
    .run()
    .await?;
    let root = page.main_frame().await?;
    let child = root.children().await?.remove(0);
    let pending_frame = child.clone();
    let task = tokio::spawn(async move {
        pending_frame
            .call("() => { parent.callCount++; return new Promise(() => {}); }")
            .run()
            .await
    });
    wait_until(page, "globalThis.callCount === 1").await?;
    assert!(!task.is_finished(), "call must await its returned promise");
    page.evaluate(
        r#"new Promise(resolve => {
        const frame = document.querySelector('#child');
        frame.onload = () => resolve();
        frame.srcdoc = '<title>After</title>';
    })"#,
    )
    .run()
    .await?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("context loss must finish the call")
        .expect("call task panicked");
    assert!(
        outcome.is_err(),
        "destroyed context cannot complete successfully"
    );
    // An explicit later call through the same Frame resolves its new document.
    assert_eq!(
        child.call("() => document.title").json::<String>().await?,
        "After"
    );
    assert_eq!(
        page.evaluate("globalThis.callCount").json::<u64>().await?,
        1
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies late result/exception handles are released after cancellation"]
async fn canceling_a_caller_keeps_late_reference_cleanup_alive() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for reject in [false, true] {
        page.evaluate("globalThis.callStarted = false; globalThis.callCount = 0;")
            .run()
            .await?;
        let caller = page.clone();
        let task = tokio::spawn(async move {
            caller
                .call(
                    r#"function(reject) {
                globalThis.callStarted = true;
                globalThis.callCount++;
                return new Promise((resolve, fail) => {
                    globalThis.finishCall = () => {
                        const value = { cleanupProbe: true };
                        globalThis.callWeak = new WeakRef(value);
                        if (reject) fail(value); else resolve(value);
                        delete globalThis.finishCall;
                    };
                });
            }"#,
                )
                .arg(json!(reject))
                .run()
                .await
        });
        wait_until(page, "globalThis.callStarted === true").await?;
        assert!(
            !task.is_finished(),
            "call must still be awaiting its promise"
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        page.evaluate("globalThis.finishCall()").run().await?;
        // Only a WeakRef remains in JS. An unreleased CDP result/exception
        // reference keeps the target alive even after explicit collection.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                page.session()
                    .execute_raw("HeapProfiler.collectGarbage", json!({}))
                    .await?;
                if page
                    .evaluate("globalThis.callWeak.deref() === undefined")
                    .json::<bool>()
                    .await?
                {
                    break Ok::<(), Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("canceled call leaked a late remote reference")?;
        assert_eq!(
            page.evaluate("globalThis.callCount").json::<u64>().await?,
            1
        );
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies this binding, existing world, and JSON arguments"]
async fn element_calls_bind_this_and_preserve_arguments_and_world() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate("document.body.innerHTML = '<div id=target data-label=original></div>'; globalThis.elementWorld = 'main';")
        .run().await?;
    page.evaluate("globalThis.elementWorld = 'isolated'")
        .world(EvaluationWorld::Isolated)
        .run()
        .await?;
    let element = page.require_element("#target").await?;
    let text = "'\"\\\n雪 🦀'); throw new Error('injected'); //";
    let value: Value = element
        .call("async function(...values) { await Promise.resolve(); return [this.getAttribute('data-label'), globalThis.elementWorld, values]; }")
        .arg(json!(text))
        .args([Value::Null, json!([false, "árbol"])])
        .arg(json!({"nested": {"value": "λ"}}))
        .json()
        .await?;
    assert_eq!(
        value,
        json!(["original", "isolated", [text, null, [false, "árbol"], {"nested": {"value": "λ"}}]])
    );
    element
        .call("function(value) { this.setAttribute('data-label', value); }")
        .arg(json!("updated"))
        .run()
        .await?;
    assert_eq!(
        element
            .call("function() { return this.dataset.label; }")
            .json::<String>()
            .await?,
        "updated"
    );
    assert!(matches!(
        element.call("function() {}").json::<Value>().await,
        Err(Error::NonJsonValue(_))
    ));
    assert_eq!(
        element
            .call("function() { return null; }")
            .json::<Value>()
            .await?,
        Value::Null
    );
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies returned/thrown this leaves original ownership intact"]
async fn element_result_and_exception_aliases_preserve_the_original() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate("document.body.innerHTML = '<div id=target></div>'")
        .run()
        .await?;
    let element = page.require_element("#target").await?;
    let original_id = element.object_id().clone();
    for source in [
        "function() { return this; }",
        "async function() { return this; }",
    ] {
        element.call(source).run().await?;
        assert_eq!(element.object_id(), &original_id);
        assert_eq!(
            element
                .call("function() { return this.id; }")
                .json::<String>()
                .await?,
            "target"
        );
    }
    // By-value DOM serialization is browser-defined; regardless of its outcome,
    // it must not invalidate the original remote handle.
    let _ = element
        .call("function() { return this; }")
        .json::<Value>()
        .await;
    for source in [
        "function() { throw this; }",
        "async function() { throw this; }",
    ] {
        assert!(matches!(
            element.call(source).run().await,
            Err(Error::JavaScript(_))
        ));
        assert!(matches!(
            element.call(source).json::<Value>().await,
            Err(Error::JavaScript(_))
        ));
        assert_eq!(
            element
                .call("function() { return this.id; }")
                .json::<String>()
                .await?,
            "target"
        );
    }
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies detached handles fail without running code or retargeting"]
async fn detached_element_calls_fail_without_using_a_replacement() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate("document.body.innerHTML = '<div id=target></div>'")
        .run()
        .await?;
    let element = page.require_element("#target").await?;
    page.evaluate("document.querySelector('#target').remove(); document.body.innerHTML = '<div id=target data-marker=replacement></div>'").run().await?;
    let source = "function() { document.body.dataset.unexpectedCall = 'yes'; return this.id; }";
    for error in [
        element.call(source).run().await.unwrap_err(),
        element.call(source).json::<Value>().await.unwrap_err(),
    ] {
        assert!(
            matches!(error, Error::InvalidArgument(ref message) if message.contains("detached")),
            "{error:?}"
        );
    }
    assert!(page.evaluate("!document.body.hasAttribute('data-unexpected-call') && document.querySelector('#target').dataset.marker === 'replacement'").json::<bool>().await?);
    element.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies cancellation retains this and cleans late result/exception handles"]
async fn canceled_element_calls_retain_this_and_release_late_references() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    for reject in [false, true] {
        page.evaluate("document.body.innerHTML = '<div id=target></div>'; delete document.body.dataset.started;").run().await?;
        let element = page.require_element("#target").await?;
        let original_id = element.object_id().clone();
        let caller = element.clone();
        let task = tokio::spawn(async move {
            caller
                .call(
                    r#"function(reject) {
                document.body.dataset.started = 'yes';
                this.dataset.callCount = String(Number(this.dataset.callCount || 0) + 1);
                globalThis.elementWeak = new WeakRef(this);
                return new Promise((resolve, fail) => {
                    globalThis.finishElementCall = () => {
                        const value = { cleanupProbe: true };
                        globalThis.resultWeak = new WeakRef(value);
                        if (reject) fail(value); else resolve(value);
                        delete globalThis.finishElementCall;
                    };
                });
            }"#,
                )
                .arg(json!(reject))
                .run()
                .await
        });
        wait_until(page, "document.body.dataset.started === 'yes'").await?;
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        // Drop all public owners while the owned execution task still awaits
        // its promise. The original CDP ID must remain usable until it finishes.
        element.release().await?;
        page.evaluate("document.querySelector('#target').remove()")
            .run()
            .await?;
        let still_owned = page
            .session()
            .execute_raw(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": original_id,
                    "functionDeclaration": "function() { return this.dataset.callCount; }",
                    "returnByValue": true,
                }),
            )
            .await?;
        assert_eq!(still_owned["result"]["value"], "1");
        page.evaluate("globalThis.finishElementCall()")
            .world(EvaluationWorld::Isolated)
            .run()
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                page.session().execute_raw("HeapProfiler.collectGarbage", json!({})).await?;
                if page.evaluate("globalThis.elementWeak.deref() === undefined && globalThis.resultWeak.deref() === undefined")
                    .world(EvaluationWorld::Isolated).json::<bool>().await? {
                    break Ok::<(), Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("canceled element call leaked its original or late remote reference")?;
    }
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies element context loss does not replay user code"]
async fn element_context_loss_does_not_replay_a_pending_call() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        r#"new Promise(resolve => {
        document.body.dataset.callCount = '0';
        const frame = document.createElement('iframe');
        frame.id = 'child';
        frame.onload = () => resolve();
        frame.srcdoc = '<div id=target></div>';
        document.body.append(frame);
    })"#,
    )
    .run()
    .await?;
    let root = page.main_frame().await?;
    let child = root.children().await?.remove(0);
    let element = child.require_element("#target").await?;
    let caller = element.clone();
    let task = tokio::spawn(async move {
        caller.call("function() { parent.document.body.dataset.callCount = String(Number(parent.document.body.dataset.callCount) + 1); return new Promise(() => {}); }").run().await
    });
    wait_until(page, "document.body.dataset.callCount === '1'").await?;
    assert!(!task.is_finished());
    page.evaluate(
        r#"new Promise(resolve => {
        const frame = document.querySelector('#child');
        frame.onload = () => resolve();
        frame.srcdoc = '<div id=target>replacement</div>';
    })"#,
    )
    .run()
    .await?;
    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("context loss must finish the element call")
        .expect("call task panicked");
    assert!(outcome.is_err());
    assert!(
        element
            .call("function() { return this.textContent; }")
            .json::<String>()
            .await
            .is_err()
    );
    assert_eq!(
        page.evaluate("document.body.dataset.callCount")
            .json::<String>()
            .await?,
        "1"
    );
    drop(element); // Its context was destroyed, so release may report a stale ID.
    browser.close().await
}

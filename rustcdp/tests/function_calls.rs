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

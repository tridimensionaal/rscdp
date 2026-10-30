//! File-input checks; run explicitly with `--ignored` and CHROME_PATH.
use rustcdp::{Browser, Error, Result};
use serde_json::{Value, json};
use std::path::PathBuf;

async fn launch() -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
}

fn stock_csv() -> PathBuf {
    // The test launches Chrome locally, so this absolute fixture path is shared.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../examples/pages/stock.csv")
        .canonicalize()
        .expect("local stock CSV exists")
}

#[tokio::test]
#[ignore = "requires Chrome; verifies hidden selection, File API content, and clearing"]
async fn hidden_file_input_accepts_files_and_empty_selection_clears() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        "document.body.innerHTML = '<input id=files type=file hidden>';\
         globalThis.fileChanges = 0;\
         document.querySelector('#files').addEventListener('change', () => fileChanges++);",
    )
    .run()
    .await?;
    let input = page.require_element("#files").await?;
    input.set_input_files(&[stock_csv()]).await?;
    let selected: Value = input
        .call("async function() { const file = this.files[0]; return [this.files.length, file.name, await file.text()]; }")
        .json()
        .await?;
    assert_eq!(
        selected,
        json!([
            1,
            "stock.csv",
            include_str!("../../examples/pages/stock.csv")
        ])
    );
    input.set_input_files(&[]).await?;
    assert!(
        input
            .call("function() { return this.files.length === 0 && this.value === ''; }")
            .json::<bool>()
            .await?
    );
    assert_eq!(page.evaluate("fileChanges").json::<u64>().await?, 2);
    input.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies multiple selection and rejection leaves single selection intact"]
async fn multiple_files_require_a_multiple_input() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate("document.body.innerHTML = '<input id=single type=file><input id=multi type=file multiple hidden>'")
        .run().await?;
    let single = page.require_element("#single").await?;
    single.set_input_files(&[stock_csv()]).await?;
    let files = [
        stock_csv(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../examples/pages/import.html")
            .canonicalize()?,
    ];
    assert!(matches!(
        single.set_input_files(&files).await,
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(
        single
            .call("function() { return Array.from(this.files, file => file.name); }")
            .json::<Value>()
            .await?,
        json!(["stock.csv"])
    );
    let multi = page.require_element("#multi").await?;
    multi.set_input_files(&files).await?;
    assert_eq!(
        multi
            .call("function() { return Array.from(this.files, file => file.name); }")
            .json::<Value>()
            .await?,
        json!(["stock.csv", "import.html"])
    );
    multi.set_input_files(&[]).await?;
    assert_eq!(
        multi
            .call("function() { return this.files.length; }")
            .json::<u64>()
            .await?,
        0
    );
    single.release().await?;
    multi.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies native type and attachment checks, without retargeting"]
async fn wrong_types_and_detached_inputs_fail_without_touching_replacements() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        "document.body.innerHTML = '<div id=div></div><input id=text><input id=files type=file>'",
    )
    .run()
    .await?;
    for selector in ["#div", "#text"] {
        let element = page.require_element(selector).await?;
        for files in [vec![], vec![stock_csv()]] {
            assert!(matches!(
                element.set_input_files(&files).await,
                Err(Error::InvalidArgument(_))
            ));
        }
        element.release().await?;
    }
    let stale = page.require_element("#files").await?;
    page.evaluate("document.querySelector('#files').outerHTML = '<input id=files type=file>'; globalThis.fileChanges = 0; document.querySelector('#files').onchange = () => fileChanges++;")
        .run().await?;
    assert!(matches!(
        stale.set_input_files(&[stock_csv()]).await,
        Err(Error::InvalidArgument(_))
    ));
    assert!(
        page.evaluate("fileChanges === 0 && document.querySelector('#files').files.length === 0")
            .json::<bool>()
            .await?
    );
    stale.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies all paths are checked before setter dispatch"]
async fn invalid_paths_preserve_the_previous_selection_and_emit_no_change() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate("document.body.innerHTML = '<input id=files type=file multiple>'")
        .run()
        .await?;
    let input = page.require_element("#files").await?;
    input.set_input_files(&[stock_csv()]).await?;
    page.evaluate("globalThis.fileChanges = 0; document.querySelector('#files').onchange = () => fileChanges++;")
        .run().await?;
    let mut invalid = vec![
        PathBuf::new(),
        PathBuf::from("relative.csv"),
        PathBuf::from("C:relative.csv"),
        PathBuf::from("/tmp/bad\0.csv"),
    ];
    #[cfg(unix)]
    {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        invalid.push(PathBuf::from(OsString::from_vec(
            b"/tmp/invalid-\xff.csv".to_vec(),
        )));
    }
    // Keep the collection mutable on platforms without the non-UTF-8 fixture.
    for invalid in invalid.drain(..) {
        assert!(matches!(
            input.set_input_files(&[stock_csv(), invalid]).await,
            Err(Error::InvalidArgument(_))
        ));
    }
    assert_eq!(
        input
            .call("function() { return Array.from(this.files, file => file.name); }")
            .json::<Value>()
            .await?,
        json!(["stock.csv"])
    );
    assert_eq!(page.evaluate("fileChanges").json::<u64>().await?, 0);
    input.release().await?;
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies locator attachment, validation, and zero-timeout behavior"]
async fn locator_waits_for_hidden_input_and_validates_before_lookup() -> Result<()> {
    use std::time::Duration;

    let mut browser = launch().await?;
    let page = browser.page();
    assert!(matches!(
        page.locator("#absent")
            .timeout(Duration::from_secs(1))
            .set_input_files(&[PathBuf::from("relative.csv")])
            .await,
        Err(Error::InvalidArgument(_))
    ));
    page.evaluate(
        "setTimeout(() => { document.body.innerHTML = '<input id=files type=file hidden>'; }, 100)",
    )
    .run()
    .await?;
    let input = page.locator("#files").timeout(Duration::from_secs(5));
    input.set_input_files(&[stock_csv()]).await?;
    assert!(matches!(
        page.locator("#files")
            .timeout(Duration::ZERO)
            .set_input_files(&[])
            .await,
        Err(Error::Timeout(_))
    ));
    assert_eq!(
        page.evaluate("document.querySelector('#files').files[0].name")
            .json::<String>()
            .await?,
        "stock.csv"
    );
    input.set_input_files(&[]).await?;
    assert_eq!(
        page.evaluate("document.querySelector('#files').files.length")
            .json::<u64>()
            .await?,
        0
    );
    browser.close().await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies clearing reports handler interference without replay"]
async fn clearing_does_not_replay_when_a_handler_repopulates_the_input() -> Result<()> {
    let mut browser = launch().await?;
    let page = browser.page();
    page.evaluate(
        r#"
        document.body.innerHTML = '<input id=files type=file hidden>';
        globalThis.fileChanges = 0;
        document.querySelector('#files').onchange = event => {
            fileChanges++;
            const transfer = new DataTransfer();
            transfer.items.add(new File(['restored'], 'restored.txt'));
            event.target.files = transfer.files;
        };
    "#,
    )
    .run()
    .await?;
    assert!(matches!(
        page.locator("#files").set_input_files(&[]).await,
        Err(Error::InvalidArgument(_))
    ));
    assert_eq!(
        page.evaluate("[fileChanges, document.querySelector('#files').files[0].name]")
            .json::<Value>()
            .await?,
        json!([1, "restored.txt"])
    );
    browser.close().await
}

//! Select a local CSV with CDP and verify its File API preview; no network submission.
mod common;
use rustcdp::{Error, Page, Result};
use serde_json::json;
use std::{path::Path, time::Duration};

async fn preview_stock(page: &Page) -> Result<()> {
    let csv = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pages/stock.csv")
        .canonicalize()?;
    page.navigate(common::page_url("import.html")?.as_str())
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    page.locator("#stock-file").set_input_files(&[csv]).await?;
    page.wait_for_element("#summary[data-state=done]", Duration::from_secs(5))
        .await?;

    let preview = page
        .evaluate(
            r#"({
                file: document.querySelector('#stock-file').files[0].name,
                rows: Array.from(document.querySelectorAll('tbody tr'),
                    row => Array.from(row.cells, cell => cell.textContent)),
                summary: document.querySelector('#summary').textContent
            })"#,
        )
        .json::<serde_json::Value>()
        .await?;
    let expected = json!({
        "file": "stock.csv",
        "rows": [["Notebook", "12"], ["Pencil", "30"], ["Eraser", "8"]],
        "summary": "stock.csv: 3 rows, 50 units total."
    });
    if preview != expected {
        return Err(Error::InvalidArgument(format!(
            "unexpected CSV preview: {preview}"
        )));
    }
    println!("Verified local CSV preview: 3 rows, 50 units total.");

    page.locator("#stock-file").set_input_files(&[]).await?;
    page.wait_for_element("#summary[data-state=idle]", Duration::from_secs(5))
        .await?;
    let cleared = page
        .call(
            "() => [document.querySelector('#stock-file').files.length,
                document.querySelectorAll('tbody tr').length,
                document.querySelector('#summary').textContent]",
        )
        .json::<(usize, usize, String)>()
        .await?;
    if cleared != (0, 0, "Choose a CSV file.".into()) {
        return Err(Error::InvalidArgument(format!(
            "file selection did not clear: {cleared:?}"
        )));
    }
    println!("Cleared file selection and preview.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    preview_stock(browser.page()).await?;
    Ok(())
}

//! Select a local CSV with CDP and verify its File API preview; no network submission.
mod common;
use rustcdp::{Error, Page, Result, cdp::dom::commands::SetFileInputFiles};
use serde_json::json;
use std::{path::Path, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    preview_stock(browser.page()).await?;
    Ok(())
}

async fn preview_stock(page: &Page) -> Result<()> {
    let csv = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pages/stock.csv")
        .canonicalize()?;
    let csv = csv
        .to_str()
        .ok_or_else(|| Error::InvalidArgument("CSV path is not UTF-8".into()))?;
    page.navigate(common::page_url("import.html")?.as_str())
        .timeout(Duration::from_secs(10))
        .send()
        .await?;

    let element = page.require_element("#stock-file").await?;
    page.session()
        .execute(
            &SetFileInputFiles::new(vec![csv.to_owned()])
                .with_object_id(element.object_id().clone()),
        )
        .await?;
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
    Ok(())
}

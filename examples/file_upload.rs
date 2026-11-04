//! Select a local CSV with CDP and read its File API preview; no network submission.
mod common;
use rustcdp::{Page, Result};
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

    let summary: String = page
        .evaluate("document.querySelector('#summary').textContent")
        .json()
        .await?;
    println!("{summary}");

    page.locator("#stock-file").set_input_files(&[]).await?;
    page.wait_for_element("#summary[data-state=idle]", Duration::from_secs(5))
        .await?;
    let summary: String = page
        .call("() => document.querySelector('#summary').textContent")
        .json()
        .await?;
    println!("After clearing: {summary}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    preview_stock(browser.page()).await?;
    Ok(())
}

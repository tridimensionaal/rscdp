//! Fill a local checkout form and submit it through keyboard input.
mod common;
use rscdp::{Page, Result};
use std::time::Duration;

async fn reserve_notebook(page: &Page) -> Result<()> {
    page.navigate(common::page_url("checkout.html")?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    page.locator("input[name=customer]")
        .fill("Ada Muñoz")
        .await?;
    page.press_key("Tab").await?;
    page.insert_text("For your next idea ✨").await?;
    page.press_key("Enter").await?;
    page.wait_for_element("output:not(:empty)", Duration::from_secs(5))
        .await?;
    let receipt = page
        .evaluate("document.querySelector('output').textContent")
        .json::<String>()
        .await?;
    println!("{receipt}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    reserve_notebook(browser.page()).await?;
    Ok(())
}

//! Fill a local checkout form and submit it through keyboard input.
mod common;
use rustcdp::{Error, Page, Result};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = common::launch().await?;
    let result = reserve_notebook(browser.page()).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn reserve_notebook(page: &Page) -> Result<()> {
    page.navigate(common::page_url("checkout.html")?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let customer = page.require_element("input[name=customer]").await?;
    customer.fill("Ada Muñoz").await?;
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
    if receipt != "Reserved for Ada Muñoz: For your next idea ✨" {
        return Err(Error::InvalidArgument(
            "unexpected reservation receipt".into(),
        ));
    }
    Ok(())
}

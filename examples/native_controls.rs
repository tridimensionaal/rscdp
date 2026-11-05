//! Save workshop account preferences with native select and checkbox helpers.
mod common;
use rscdp::{Page, Result, page::ElementState};

async fn save_preferences(page: &Page) -> Result<()> {
    page.navigate(common::page_url("controls.html")?)
        .send()
        .await?;
    page.locator("#digest").select_option("weekly").await?;
    page.locator("#receipts").set_checked(true).await?;
    page.locator("#save").click().await?;
    page.locator("output")
        .wait_for(ElementState::Visible)
        .await?;
    let confirmation: String = page
        .call("() => document.querySelector('output').textContent")
        .json()
        .await?;
    println!("{confirmation}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    save_preferences(browser.page()).await
}

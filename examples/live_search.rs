//! Reuse a locator after a search replaces its matching DOM node.
mod common;
use rustcdp::{Error, Page, Result, page::ElementState};
use std::time::Duration;

async fn choose_workshop(page: &Page) -> Result<()> {
    page.navigate(common::page_url("workshops.html")?)
        .send()
        .await?;
    let reserve = page.locator("#reserve").timeout(Duration::from_secs(5));
    reserve.wait_for(ElementState::Visible).await?;
    page.locator("#query").fill("ceramics").await?;
    // This durable marker avoids having to catch a briefly visible loading message.
    page.locator("#results[data-query=ceramics]")
        .timeout(Duration::from_secs(5))
        .wait_for(ElementState::Visible)
        .await?;
    page.locator("#loading")
        .wait_for(ElementState::Hidden)
        .await?;
    page.locator("#original-card")
        .wait_for(ElementState::Detached)
        .await?;
    // The selector is unchanged, but the original button no longer exists.
    reserve.click().await?;
    let state: (String, bool) = page.call("() => [document.querySelector('#confirmation').textContent, document.querySelector('#loading').hidden]")
        .json().await?;
    if state != ("Reserved: Evening ceramics".into(), true) {
        return Err(Error::InvalidArgument(format!(
            "unexpected search result: {state:?}"
        )));
    }
    println!(
        "{}; loading indicator hidden, original card removed",
        state.0
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    choose_workshop(browser.page()).await
}

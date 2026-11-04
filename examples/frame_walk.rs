//! Explore a nested dispatch board without confusing parent and child documents.
mod common;
use rustcdp::{
    Error, Page, Result,
    page::{ClickMethod, ClickOptions},
};
use serde_json::json;
use std::time::Duration;

async fn update_inventory(page: &Page) -> Result<()> {
    page.navigate(common::page_url("dispatch-board.html")?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    for frame in page.frames().await? {
        println!(
            "{}: {}",
            frame.id().0,
            frame.evaluate("document.title").json::<String>().await?
        );
    }
    let root = page.main_frame().await?;
    let children = root.children().await?;
    let warehouse = children
        .first()
        .ok_or_else(|| Error::InvalidArgument("warehouse frame missing".into()))?;
    let children = warehouse.children().await?;
    let shelf = children
        .first()
        .ok_or_else(|| Error::InvalidArgument("shelf frame missing".into()))?;
    let receive = shelf.require_element("button").await?;
    receive
        .click_with_options(ClickOptions {
            method: ClickMethod::JavaScript,
            scroll_into_view: false,
        })
        .await?;
    let stock = shelf
        .call("selector => Number(document.querySelector(selector).textContent)")
        .arg(json!("output"))
        .json::<u64>()
        .await?;
    println!("Shelf A now holds {stock} notebooks");
    println!(
        "Parent document: {}",
        root.evaluate("document.title").json::<String>().await?
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    update_inventory(browser.page()).await?;
    Ok(())
}

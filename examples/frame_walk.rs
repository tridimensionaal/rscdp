//! Explore a nested dispatch board without confusing parent and child documents.
mod common;
use rustcdp::{
    Error, Page, Result,
    page::{ClickMethod, ClickOptions},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    update_inventory(browser.page()).await?;
    Ok(())
}

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
        .evaluate("Number(document.querySelector('output').textContent)")
        .json::<u64>()
        .await?;
    println!("Shelf A now holds {stock} notebooks");
    if stock != 4 {
        return Err(Error::InvalidArgument("inventory did not update".into()));
    }
    println!(
        "Parent document is still {}",
        root.evaluate("document.title").json::<String>().await?
    );
    Ok(())
}

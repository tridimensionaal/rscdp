//! Explore a nested dispatch board without confusing parent and child documents.
mod common;
use rustcdp::{
    Error, Page, Result,
    page::{ClickMethod, ClickOptions},
};
use std::time::Duration;

fn embed(name: &str, html: &str) -> String {
    let html = html.replace('&', "&amp;").replace('"', "&quot;");
    format!("<iframe name=\"{name}\" srcdoc=\"{html}\"></iframe>")
}

#[tokio::main]
async fn main() -> Result<()> {
    let shelf = "<title>Shelf A</title><h2>Notebooks</h2><output>3</output><button onclick=\"document.querySelector('output').textContent++\">Receive one</button>";
    let warehouse = format!(
        "<title>Warehouse</title><h1>Inventory</h1>{}",
        embed("shelf", shelf)
    );
    let board = format!(
        "<title>Dispatch board</title><h1>Today's dispatch</h1>{}{}",
        embed("warehouse", &warehouse),
        embed("courier", "<title>Courier</title><p>Pickup at 16:00</p>")
    );
    let mut browser = common::launch().await?;
    let result = update_inventory(browser.page(), &board).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn update_inventory(page: &Page, board: &str) -> Result<()> {
    page.navigate(common::html_url(board))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    for frame in page.frames().await? {
        println!(
            "{}: {}",
            frame.id().0,
            frame.evaluate("document.title").await?
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
        .await?;
    println!("Shelf A now holds {stock} notebooks");
    if stock != 4 {
        return Err(Error::InvalidArgument("inventory did not update".into()));
    }
    println!(
        "Parent document is still {}",
        root.evaluate("document.title").await?
    );
    Ok(())
}

//! Combine independent warehouse summaries through one browser connection.
mod common;
use rustcdp::{Browser, Page, Result};

async fn collect_stock(page: &Page, warehouse: &str) -> Result<(String, u64)> {
    let mut url = common::page_url("stock-board.html")?;
    url.query_pairs_mut().append_pair("warehouse", warehouse);
    page.navigate(url).send().await?;
    page.call("() => [document.querySelector('h1').textContent, Number(document.querySelector('output').textContent)]")
        .json().await
}

async fn summarize(browser: &Browser) -> Result<()> {
    let south = browser.new_page().await?;
    let east = browser.new_page().await?;
    // Independent sessions share one connection. No pool or spawned tasks are needed.
    let (north_stock, south_stock, east_stock) = tokio::try_join!(
        collect_stock(browser.page(), "north"),
        collect_stock(&south, "south"),
        collect_stock(&east, "east"),
    )?;
    let stock = [north_stock, south_stock, east_stock];
    south.close().await?;
    east.close().await?;
    println!(
        "{stock:?}: {} notebooks total",
        stock.iter().map(|(_, count)| count).sum::<u64>()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    summarize(&browser).await
}

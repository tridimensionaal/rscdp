//! Attach a second connection, then disconnect without stopping the browser.
mod common;
use rustcdp::{Browser, Error, Result};

async fn inspect_connection(browser: &Browser) -> Result<()> {
    browser
        .page()
        .navigate(common::page_url("report.html")?)
        .send()
        .await?;
    let process = browser
        .process()
        .ok_or_else(|| Error::InvalidArgument("launched browser has no process".into()))?;
    let mut attached = Browser::connect(process.endpoint()).await?;
    let title = attached
        .page()
        .evaluate("document.title")
        .json::<String>()
        .await?;
    if title != "Workshop report" {
        return Err(Error::InvalidArgument(format!("unexpected page: {title}")));
    }
    println!("Read through the second connection: {title}");

    // This handle owns only its connection, so closing it leaves Chrome running.
    attached.close().await?;
    let title = browser
        .page()
        .evaluate("document.title")
        .json::<String>()
        .await?;
    if title != "Workshop report" {
        return Err(Error::InvalidArgument("original page changed".into()));
    }
    println!("Original browser is still usable: {title}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    inspect_connection(&browser).await
}

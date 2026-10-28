//! Inspect an existing browser without taking ownership of its Chrome process.
//! Start Chrome separately with `--remote-debugging-port=9222` and a dedicated
//! `--user-data-dir`, then pass its HTTP or browser WebSocket endpoint here.
//! An optional second argument selects a target ID printed by a previous run.
//! Without it, selection uses the first target ID in sorted order, not the active tab.
use rustcdp::{Browser, Error, Result, cdp::target::TargetId};

async fn inspect_browser(browser: &Browser, target: Option<&TargetId>) -> Result<()> {
    let pages = browser.pages().await?;
    for page in &pages {
        let title = page.evaluate("document.title").json::<String>().await?;
        println!("{}: {title}", page.id());
    }
    let selected = match target {
        Some(id) => pages.iter().find(|page| page.id() == id),
        None => pages.first(),
    }
    .ok_or_else(|| Error::InvalidArgument("requested browser tab was not found".into()))?;
    let url = selected.evaluate("location.href").json::<String>().await?;
    println!("Selected tab {}: {url}", selected.id());

    // The context owns this temporary tab, including cleanup on an early error.
    // Existing tabs are only inspected; they are never navigated or closed.
    let mut inspection = browser.new_context().await?;
    let tab = inspection.page();
    let environment = tab
        .evaluate("({language: navigator.language, width: innerWidth, height: innerHeight})")
        .json::<serde_json::Value>()
        .await?;
    println!("Temporary tab {}: {environment}", tab.id());
    tab.close().await?;
    inspection.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let endpoint = arguments.next().ok_or_else(|| {
        Error::InvalidArgument("usage: connect <http-or-ws-endpoint> [target-id]".into())
    })?;
    let target = arguments.next().map(TargetId);
    if arguments.next().is_some() {
        return Err(Error::InvalidArgument(
            "expected an endpoint and optional target ID".into(),
        ));
    }
    let mut browser = Browser::connect(&endpoint).await?;
    inspect_browser(&browser, target.as_ref()).await?;
    // For an external browser, close awaits disconnection and leaves Chrome running.
    browser.close().await?;
    Ok(())
}

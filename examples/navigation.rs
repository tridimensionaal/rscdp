//! Follow a museum entrance link, then wait for the destination's exhibit to load.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{Error, Page, Result, page::WaitUntil};
use std::time::Duration;

async fn enter_gallery(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("museum.html")).send().await?;
    let entrance = page.require_element("#enter-gallery").await?;

    // Arm before clicking: a fast navigation could finish before a later subscription.
    let arrival = page
        .expect_navigation()
        .wait_until(WaitUntil::DomContentLoaded)
        .timeout(Duration::from_secs(10))
        .prepare()
        .await?;
    entrance.click().await?;
    let destination = arrival.wait().await?;
    if destination.url != server.url("gallery.html") {
        return Err(Error::InvalidArgument(format!(
            "unexpected destination: {}",
            destination.url
        )));
    }

    // DOMContentLoaded doesn't mean the application has finished loading its content.
    // This page publishes an explicit readiness marker; presence alone isn't enough.
    page.wait_for_element("#exhibit[data-ready=true]", Duration::from_secs(5))
        .await?;
    let exhibit = page
        .evaluate("document.querySelector('#exhibit').textContent")
        .json::<String>()
        .await?;
    if exhibit != "Light through glass — Gallery 3" {
        return Err(Error::InvalidArgument(format!(
            "unexpected exhibit: {exhibit}"
        )));
    }
    println!("Arrived at {}: {exhibit}", destination.url);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    enter_gallery(browser.page(), &server).await?;
    Ok(())
}

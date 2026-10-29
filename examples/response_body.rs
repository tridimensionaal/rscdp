//! Observe a cafe menu request and read its completed response body through CDP.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{Error, Page, Result};
use serde_json::Value;
use std::time::Duration;

async fn read_menu(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("cafe.html")).send().await?;
    let menu_response = page
        .expect_response(server.url("api/menu.json"))
        .timeout(Duration::from_secs(10))
        .prepare()
        .await?;
    page.require_element("#load-menu").await?.click().await?;
    let response = menu_response.wait().await?;
    if response.metadata().status != 200 {
        return Err(Error::InvalidArgument(format!(
            "menu request returned HTTP {}",
            response.metadata().status
        )));
    }
    let body = response.body().await?;

    let menu: Value = serde_json::from_slice(&body)?;
    if menu["special"]["name"] != "Orange blossom latte" || menu["special"]["price"] != 4.5 {
        return Err(Error::InvalidArgument(format!("unexpected menu: {menu}")));
    }
    println!(
        "Captured {} response bytes: {}",
        body.len(),
        menu["special"]
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    read_menu(browser.page(), &server).await?;
    Ok(())
}

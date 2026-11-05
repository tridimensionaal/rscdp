//! Observe a cafe menu request and read its completed response body through CDP.
mod common;
#[path = "common/server.rs"]
mod server;
use rscdp::{Error, Page, Result};
use serde_json::Value;
use std::time::Duration;

async fn read_menu(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("cafe.html")).send().await?;
    let menu_response = page
        .expect_response(server.url("api/menu.json"))
        .timeout(Duration::from_secs(10))
        .prepare()
        .await?;
    page.locator("#load-menu").click().await?;
    let response = menu_response.wait().await?;
    if response.metadata().status != 200 {
        return Err(Error::InvalidArgument(format!(
            "menu request returned HTTP {}",
            response.metadata().status
        )));
    }
    let menu = response.json::<Value>().await?;
    println!("Captured menu special: {}", menu["special"]);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    read_menu(browser.page(), &server).await?;
    Ok(())
}

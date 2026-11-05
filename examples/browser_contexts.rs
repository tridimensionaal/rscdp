//! Keep two shoppers' cookie jars isolated within one browser process.
//! Serve real pages on localhost and read each context's cookies in the page.
mod common;
#[path = "common/server.rs"]
mod server;
use rscdp::{
    Browser, BrowserContext, Result,
    cdp::network::{Cookie, CookieParam},
};
use std::time::Duration;

async fn shopping_sessions(browser: &Browser, server: &server::Server) -> Result<()> {
    // Keep the context owners alive while using their pages.
    let mira = browser.new_context().await?;
    let mut leo = browser.new_context().await?;
    prepare_cart(&mira, server, "Mira", "notebook").await?;
    prepare_cart(&leo, server, "Leo", "mug").await?;

    // Same cookie name and domain, different values in independent contexts.
    let mira_cart = cart_item(mira.cookies().await?);
    let leo_cart = cart_item(leo.cookies().await?);
    let parent_cart = cart_item(browser.page().cookies().await?);
    println!("Mira: {mira_cart:?}; Leo: {leo_cart:?}; parent: {parent_cart:?}");

    let preview = mira.new_page().await?;
    preview.navigate(server.url("preview.html")).send().await?;
    let preview_cart = preview
        .evaluate("document.querySelector('#cart').textContent")
        .json::<String>()
        .await?;
    println!(
        "Mira's preview: {preview_cart}; {} tabs in her context",
        mira.pages().await?.len()
    );
    preview.close().await?;

    leo.clear_cookies().await?;
    println!(
        "After clearing Leo's cookies: Leo {:?}; Mira {:?}",
        cart_item(leo.cookies().await?),
        cart_item(mira.cookies().await?)
    );
    leo.close().await?;

    // Disposing Leo's context leaves Mira's page and cookies available.
    let title = mira
        .page()
        .evaluate("document.title")
        .json::<String>()
        .await?;
    println!(
        "After closing Leo's context: {title}, cart {:?}",
        cart_item(mira.cookies().await?)
    );
    Ok(())
}

async fn prepare_cart(
    context: &BrowserContext,
    server: &server::Server,
    shopper: &str,
    item: &str,
) -> Result<()> {
    context
        .set_cookies(&[CookieParam::new("cart", item)
            .with_url(server.url("cart.html"))
            .with_path("/")])
        .await?;
    context
        .page()
        .navigate(server.url(&format!("cart.html?shopper={shopper}")))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let displayed = context
        .page()
        .evaluate("document.querySelector('#cart').textContent")
        .json::<String>()
        .await?;
    println!("{shopper}'s page: {displayed}");
    Ok(())
}

fn cart_item(cookies: Vec<Cookie>) -> Option<String> {
    cookies
        .into_iter()
        .find(|cookie| cookie.name == "cart" && cookie.domain == "127.0.0.1")
        .map(|cookie| cookie.value)
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    shopping_sessions(&browser, &server).await
}

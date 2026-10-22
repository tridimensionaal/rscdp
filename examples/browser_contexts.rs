//! Keep two shoppers' cookie jars isolated within one browser process.
//! Serve real pages on localhost and read each context's cookies in the page.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{
    Browser, BrowserContext, Error, Result,
    cdp::network::{Cookie, CookieParam},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let mut browser = common::launch().await?;
    let result = shopping_sessions(&browser, &server).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn shopping_sessions(browser: &Browser, server: &server::Server) -> Result<()> {
    // Retain this owner for as long as its page is needed.
    let mut mira = browser.new_context().await?;
    let result = second_shopper(browser, &mira, server).await;
    // Always await disposal, including when creating or using the second fails.
    let cleanup = mira.close().await;
    result?;
    cleanup
}

async fn second_shopper(
    browser: &Browser,
    mira: &BrowserContext,
    server: &server::Server,
) -> Result<()> {
    let mut leo = browser.new_context().await?;
    let result = compare_carts(browser, mira, &leo, server).await;
    let cleanup = leo.close().await;
    result?;
    cleanup?;

    // Disposing Leo's context leaves Mira's page and cookies intact.
    let title = mira
        .page()
        .evaluate("document.title")
        .json::<String>()
        .await?;
    if title != "Mira's cart" || cart_item(mira.cookies().await?).as_deref() != Some("notebook") {
        return Err(Error::InvalidArgument(
            "Mira's context did not survive".into(),
        ));
    }
    println!("Leo's context closed; Mira's cart still contains a notebook.");
    Ok(())
}

async fn compare_carts(
    browser: &Browser,
    mira: &BrowserContext,
    leo: &BrowserContext,
    server: &server::Server,
) -> Result<()> {
    prepare_cart(mira, server, "Mira", "notebook").await?;
    prepare_cart(leo, server, "Leo", "mug").await?;

    // Same cookie name and domain, different values in independent contexts.
    let mira_cart = cart_item(mira.cookies().await?);
    let leo_cart = cart_item(leo.cookies().await?);
    let parent_cart = cart_item(browser.page().cookies().await?);
    println!("Mira: {mira_cart:?}; Leo: {leo_cart:?}; parent: {parent_cart:?}");
    if mira_cart.as_deref() != Some("notebook")
        || leo_cart.as_deref() != Some("mug")
        || parent_cart.is_some()
    {
        return Err(Error::InvalidArgument(
            "cart cookies were not isolated".into(),
        ));
    }
    let preview = mira.new_page().await?;
    preview.navigate(server.url("preview.html")).send().await?;
    let preview_cart = preview
        .evaluate("document.querySelector('#cart').textContent")
        .json::<String>()
        .await?;
    if preview_cart != "notebook"
        || cart_item(preview.cookies().await?).as_deref() != Some("notebook")
    {
        return Err(Error::InvalidArgument(
            "preview did not share Mira's cart".into(),
        ));
    }
    let pages = mira.pages().await?;
    if pages.len() != 2 || !pages.iter().any(|page| page.id() == preview.id()) {
        return Err(Error::InvalidArgument("Mira's tabs were not listed".into()));
    }
    println!(
        "Mira's cart and preview share cookies across {} tabs.",
        pages.len()
    );
    preview.close().await?;
    leo.clear_cookies().await?;
    if !leo.cookies().await?.is_empty()
        || cart_item(mira.cookies().await?).as_deref() != Some("notebook")
        || cart_item(browser.page().cookies().await?).is_some()
    {
        return Err(Error::InvalidArgument(
            "clearing Leo's cookies was not isolated".into(),
        ));
    }
    println!("Leo's cookies cleared; Mira's notebook cookie is unchanged.");
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
    if displayed != item {
        return Err(Error::InvalidArgument(
            "page did not read its context's cart cookie".into(),
        ));
    }
    Ok(())
}

fn cart_item(cookies: Vec<Cookie>) -> Option<String> {
    cookies
        .into_iter()
        .find(|cookie| cookie.name == "cart" && cookie.domain == "127.0.0.1")
        .map(|cookie| cookie.value)
}

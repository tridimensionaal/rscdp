//! Keep two shoppers' cookie jars isolated within one browser process.
//! Cookies are seeded through CDP for a fictitious site; no network is needed.
mod common;
use rustcdp::{
    Browser, BrowserContext, Error, Result,
    cdp::network::{Cookie, CookieParam},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = common::launch().await?;
    let result = shopping_sessions(&browser).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn shopping_sessions(browser: &Browser) -> Result<()> {
    // Retain this owner for as long as its page is needed.
    let mut mira = browser.new_context().await?;
    let result = second_shopper(browser, &mira).await;
    // Always await disposal, including when creating or using the second fails.
    let cleanup = mira.close().await;
    result?;
    cleanup
}

async fn second_shopper(browser: &Browser, mira: &BrowserContext) -> Result<()> {
    let mut leo = browser.new_context().await?;
    let result = compare_carts(browser, mira, &leo).await;
    let cleanup = leo.close().await;
    result?;
    cleanup?;

    // Disposing Leo's context leaves Mira's page and cookies intact.
    let title = mira.page().evaluate("document.title").await?;
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
) -> Result<()> {
    prepare_cart(mira, "Mira", "notebook").await?;
    prepare_cart(leo, "Leo", "mug").await?;

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

async fn prepare_cart(context: &BrowserContext, shopper: &str, item: &str) -> Result<()> {
    context
        .set_cookies(&[CookieParam::new("cart", item)
            .with_domain("shop.example")
            .with_path("/")])
        .await?;
    // data: pages cannot access shop.example cookies through document.cookie.
    // Display the context's cookie through the CDP cookie API instead.
    let cart = cart_item(context.cookies().await?);
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>{shopper}'s cart</title>\
         <h1>{shopper}'s cart</h1><p>Cart cookie for shop.example: {cart:?}</p>"
    );
    context
        .page()
        .navigate_and_wait(&common::html_url(&html), Duration::from_secs(10))
        .await
}

fn cart_item(cookies: Vec<Cookie>) -> Option<String> {
    cookies
        .into_iter()
        .find(|cookie| {
            cookie.name == "cart" && cookie.domain.trim_start_matches('.') == "shop.example"
        })
        .map(|cookie| cookie.value)
}

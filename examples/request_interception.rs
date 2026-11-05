//! Continue a stock request, replace a delivery quote, and block analytics.
mod common;
#[path = "common/server.rs"]
mod server;
use base64::{Engine, engine::general_purpose::STANDARD};
use rscdp::{
    Error, Page, Result,
    cdp::{
        fetch::{
            HeaderEntry, RequestPattern, RequestStage,
            commands::{ContinueRequest, Disable, Enable, FailRequest, FulfillRequest},
            events::RequestPaused,
        },
        network::ErrorReason,
    },
    session::TypedEvents,
};
use serde_json::json;
use std::time::Duration;

async fn delivery_options(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("delivery.html")).send().await?;
    // Listen before enabling Fetch. Only this page's local API calls are paused.
    // This demo owns Fetch; do not combine it with managed proxy authentication.
    let mut requests = page.session().listen::<RequestPaused>();
    page.session()
        .execute(&Enable::new().with_patterns(vec![
            RequestPattern::new()
                .with_url_pattern(server.url("api/*"))
                .with_request_stage(RequestStage::Request),
        ]))
        .await?;
    let interception_result = tokio::time::timeout(
        Duration::from_secs(10),
        intercept_and_load(page, &mut requests),
    )
    .await
    .map_err(|_| Error::Timeout("intercepting delivery requests".into()));
    // Disable even on a timeout/error so requests cannot remain paused.
    let fetch_disable_result = page.session().execute(&Disable::new()).await;
    interception_result??;
    fetch_disable_result?;
    Ok(())
}

async fn intercept_and_load(page: &Page, requests: &mut TypedEvents<RequestPaused>) -> Result<()> {
    // Drive the page and handle pauses concurrently; waiting for the page first
    // would deadlock if it needed a response from one of these requests.
    tokio::try_join!(handle_requests(page, requests), load_dashboard(page))?;
    Ok(())
}

async fn handle_requests(page: &Page, requests: &mut TypedEvents<RequestPaused>) -> Result<()> {
    // One click on this page makes exactly these three API calls.
    for _ in 0..3 {
        let request = requests.recv().await?;
        let session = page.session();
        if request.request.url.ends_with("/quote.json") {
            let quote = json!({ "price": 0, "arrival": "today" });
            session
                .execute(
                    &FulfillRequest::new(request.request_id, 200)
                        .with_response_headers(vec![HeaderEntry::new(
                            "Content-Type",
                            "application/json",
                        )])
                        .with_body(STANDARD.encode(quote.to_string())),
                )
                .await?;
            println!("Replaced delivery quote: free delivery today");
        } else if request.request.url.ends_with("/analytics.json") {
            session
                .execute(&FailRequest::new(
                    request.request_id,
                    ErrorReason::BlockedByClient,
                ))
                .await?;
            println!("Blocked analytics request");
        } else {
            session
                .execute(&ContinueRequest::new(request.request_id))
                .await?;
            println!(
                "Continued request to the local server: {}",
                request.request.url
            );
        }
    }
    Ok(())
}

async fn load_dashboard(page: &Page) -> Result<()> {
    page.require_element("#load").await?.click().await?;
    page.wait_for_element("#status[data-state=done]", Duration::from_secs(5))
        .await?;
    let displayed = page
        .evaluate(
            "['stock', 'quote', 'analytics'].map(id => document.getElementById(id).textContent)",
        )
        .json::<Vec<String>>()
        .await?;
    println!("Delivery dashboard: {}", displayed.join("; "));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    delivery_options(browser.page(), &server).await?;
    Ok(())
}

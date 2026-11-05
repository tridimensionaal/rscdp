//! Fail an uncached manifest request offline, then restore the connection and retry.
mod common;
#[path = "common/server.rs"]
mod server;
use rscdp::page::ElementState;
use rscdp::{
    Error, Page, Result,
    cdp::network::{
        NetworkConditions,
        commands::{EmulateNetworkConditionsByRule, OverrideNetworkState},
    },
};
use serde_json::Value;
use std::time::Duration;

async fn load_manifest(page: &Page, server: &server::Server, phase: &str) -> Result<()> {
    // Arm the exact-URL response observer before the initial request or retry.
    let pending = page
        .expect_response(server.url("api/manifest.json"))
        .timeout(Duration::from_secs(10))
        .prepare()
        .await?;
    page.locator("#load-manifest").click().await?;
    let response = pending.wait().await?;
    let status = response.metadata().status;
    if !(200..300).contains(&status) {
        return Err(Error::InvalidArgument(format!(
            "manifest returned HTTP {status}"
        )));
    }
    let manifest = response.json::<Value>().await?;
    page.locator("#status[data-state=ready]")
        .timeout(Duration::from_secs(10))
        .wait_for(ElementState::Visible)
        .await?;
    let (message, online) = page
        .evaluate("[document.querySelector('#status').textContent, navigator.onLine]")
        .json::<(String, bool)>()
        .await?;
    println!("{phase}: HTTP {status}; {message}; navigator.onLine={online}");
    println!("Manifest: {manifest}");
    Ok(())
}

async fn recover_manifest(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("manifest.html"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    // Share reporting across both response observers while changing network state.
    let network = page.session().enable_network().await?;
    load_manifest(page, server, "Online").await?;

    // The per-rule offline flag affects transport; navigator needs its own command.
    // An empty pattern matches every request in this owned page session.
    page.session()
        .execute(&EmulateNetworkConditionsByRule::new(vec![
            NetworkConditions::new("", 0.0, -1.0, -1.0).with_offline(true),
        ]))
        .await?;
    page.session()
        .execute(&OverrideNetworkState::new(true, 0.0, -1.0, -1.0))
        .await?;
    page.locator("#load-manifest").click().await?;
    page.locator("#status[data-state=failed]")
        .timeout(Duration::from_secs(10))
        .wait_for(ElementState::Visible)
        .await?;
    let (message, error_name, online) = page
        .call(
            "() => { const status = document.querySelector('#status');
                return [status.textContent, status.dataset.errorName, navigator.onLine]; }",
        )
        .json::<(String, String, bool)>()
        .await?;
    println!("Offline: {message}; fetch error={error_name}; navigator.onLine={online}");

    // Restore both controls before reusing this same page for the retry.
    page.session()
        .execute(&EmulateNetworkConditionsByRule::new(Vec::new()))
        .await?;
    page.session()
        .execute(&OverrideNetworkState::new(false, 0.0, -1.0, -1.0))
        .await?;
    load_manifest(page, server, "Recovered").await?;
    network.close().await
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    recover_manifest(browser.page(), &server).await
}

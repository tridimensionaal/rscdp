//! Fail an uncached manifest request offline, then restore the connection and retry.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::page::ElementState;
use rustcdp::{
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
    if response.metadata().status != 200 {
        return Err(Error::InvalidArgument(format!(
            "manifest returned HTTP {}",
            response.metadata().status
        )));
    }
    let manifest = response.json::<Value>().await?;
    if manifest["exhibits"].as_array().map(Vec::len) != Some(3) {
        return Err(Error::InvalidArgument(format!(
            "unexpected manifest: {manifest}"
        )));
    }
    page.locator("#status[data-state=ready]")
        .timeout(Duration::from_secs(10))
        .wait_for(ElementState::Visible)
        .await?;
    let displayed = page
        .evaluate("[document.querySelector('#status').textContent, navigator.onLine]")
        .json::<(String, bool)>()
        .await?;
    if displayed != ("3 exhibits available".into(), true) {
        return Err(Error::InvalidArgument(format!(
            "unexpected {phase} state: {displayed:?}"
        )));
    }
    println!("{phase}: 3 exhibits available; navigator.onLine=true");
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
    let failed = page
        .call(
            "() => { const status = document.querySelector('#status');
                return [status.textContent, status.dataset.errorName, navigator.onLine]; }",
        )
        .json::<(String, String, bool)>()
        .await?;
    if failed != ("Manifest request failed".into(), "TypeError".into(), false) {
        return Err(Error::InvalidArgument(format!(
            "unexpected offline failure: {failed:?}"
        )));
    }
    println!("Offline: uncached fetch rejected; navigator.onLine=false");

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

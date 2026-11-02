//! Select a nearby pickup point, then show the permission-denied path.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::page::ElementState;
use rustcdp::{
    Browser, Error, Result,
    cdp::{
        browser::{
            PermissionDescriptor, PermissionSetting,
            commands::{ResetPermissions, SetPermission},
        },
        emulation::commands::{ClearGeolocationOverride, SetGeolocationOverride},
    },
};
use std::time::Duration;

async fn choose_pickup(browser: &Browser, server: &server::Server) -> Result<()> {
    // This owner keeps the page and its permission overrides isolated.
    let mut context = browser.new_context().await?;
    let context_id = context
        .id()
        .ok_or_else(|| Error::InvalidArgument("pickup context is closed".into()))?
        .clone();
    let page = context.page();
    let origin = server.url("").trim_end_matches('/').to_owned();
    browser
        .connection()
        .execute(
            &SetPermission::new(
                PermissionDescriptor::new("geolocation"),
                PermissionSetting::Granted,
            )
            .with_origin(origin.clone())
            .with_browser_context_id(context_id.clone()),
        )
        .await?;
    page.session()
        .execute(
            &SetGeolocationOverride::new()
                .with_latitude(-33.45)
                .with_longitude(-70.66)
                .with_accuracy(10.0),
        )
        .await?;
    page.navigate(server.url("pickup.html"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    page.locator("#find-pickup").click().await?;
    page.locator("#status[data-state=selected]")
        .timeout(Duration::from_secs(10))
        .wait_for(ElementState::Visible)
        .await?;
    let selected = page
        .call(
            "() => { const status = document.querySelector('#status');
                return [status.textContent, Number(status.dataset.latitude),
                    Number(status.dataset.longitude), Number(status.dataset.accuracy)]; }",
        )
        .json::<(String, f64, f64, f64)>()
        .await?;
    if selected != ("Central workshop".into(), -33.45, -70.66, 10.0) {
        return Err(Error::InvalidArgument(format!(
            "unexpected pickup or coordinates: {selected:?}"
        )));
    }
    println!("Granted: Central workshop at -33.45, -70.66 (accuracy 10 m)");

    browser
        .connection()
        .execute(
            &SetPermission::new(
                PermissionDescriptor::new("geolocation"),
                PermissionSetting::Denied,
            )
            .with_origin(origin)
            .with_browser_context_id(context_id.clone()),
        )
        .await?;
    // A new document cannot reuse the previous successful callback or UI.
    page.navigate(server.url("pickup.html?permission=denied"))
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    page.locator("#find-pickup").click().await?;
    page.locator("#status[data-state=denied]")
        .timeout(Duration::from_secs(10))
        .wait_for(ElementState::Visible)
        .await?;
    let denied = page
        .call(
            "() => { const status = document.querySelector('#status');
                return [status.textContent, Number(status.dataset.errorCode)]; }",
        )
        .json::<(String, u64)>()
        .await?;
    if denied != ("Location permission denied".into(), 1) {
        return Err(Error::InvalidArgument(format!(
            "unexpected denied state: {denied:?}"
        )));
    }
    println!("Denied: Location permission denied (code 1)");

    page.session()
        .execute(&ClearGeolocationOverride::new())
        .await?;
    browser
        .connection()
        .execute(&ResetPermissions::new().with_browser_context_id(context_id))
        .await?;
    context.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    choose_pickup(&browser, &server).await
}

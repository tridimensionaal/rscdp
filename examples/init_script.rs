//! Seed application configuration before startup, then remove the registration.
mod common;
use rscdp::{
    Page, Result,
    cdp::page::commands::{AddScriptToEvaluateOnNewDocument, RemoveScriptToEvaluateOnNewDocument},
};

async fn show_welcome(page: &Page, phase: &str) -> Result<()> {
    let (welcome, configured): (String, Option<String>) = page
        .call(
            "() => [document.querySelector('#welcome').textContent,
                window.timetableConfig?.welcome ?? null]",
        )
        .json()
        .await?;
    println!("{phase}: welcome={welcome:?}; startup configuration={configured:?}");
    Ok(())
}

async fn configure_startup(page: &Page) -> Result<()> {
    // Omitting world_name runs this in the application's main world before its scripts.
    let registration = page
        .session()
        .execute(&AddScriptToEvaluateOnNewDocument::new(
            "window.timetableConfig = { welcome: 'Evening timetable' };",
        ))
        .await?;
    for visit in ["first", "second"] {
        let mut url = common::page_url("startup.html")?;
        url.set_query(Some(visit));
        page.navigate(url).send().await?;
        show_welcome(page, visit).await?;
    }
    page.session()
        .execute(&RemoveScriptToEvaluateOnNewDocument::new(
            registration.identifier,
        ))
        .await?;

    // Removal stops future injection; it does not undo the current document's
    // configuration or the welcome that the application read during startup.
    show_welcome(page, "Current document after removal").await?;
    let mut url = common::page_url("startup.html")?;
    url.set_query(Some("after-removal"));
    page.navigate(url).send().await?;
    show_welcome(page, "Next document after removal").await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    configure_startup(browser.page()).await
}

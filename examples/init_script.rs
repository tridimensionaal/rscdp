//! Seed application configuration before startup, then remove the registration.
mod common;
use rustcdp::{
    Error, Page, Result,
    cdp::page::commands::{AddScriptToEvaluateOnNewDocument, RemoveScriptToEvaluateOnNewDocument},
};

async fn check_welcome(page: &Page, expected: &str) -> Result<()> {
    let welcome: String = page
        .call("() => document.querySelector('#welcome').textContent")
        .json()
        .await?;
    if welcome != expected {
        return Err(Error::InvalidArgument(format!(
            "unexpected startup welcome: {welcome:?}; expected {expected:?}"
        )));
    }
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
        check_welcome(page, "Evening timetable").await?;
    }
    page.session()
        .execute(&RemoveScriptToEvaluateOnNewDocument::new(
            registration.identifier,
        ))
        .await?;

    // Removing a registration does not undo the current document's startup state.
    check_welcome(page, "Evening timetable").await?;
    let mut url = common::page_url("startup.html")?;
    url.set_query(Some("after-removal"));
    page.navigate(url).send().await?;
    check_welcome(page, "Daily timetable").await?;
    println!(
        "Two seeded documents; removal preserves the current welcome, then restores the default on navigation."
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    configure_startup(browser.page()).await
}

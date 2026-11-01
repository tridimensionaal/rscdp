//! Adopt an application-created popup and send a seat selection to its opener.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{
    Browser, Error, Page, Result,
    cdp::target::{TargetId, commands::SetDiscoverTargets, events::TargetCreated},
    page::ElementState,
    session::TypedEvents,
};
use std::time::Duration;

async fn book_seat(browser: &Browser, server: &server::Server) -> Result<()> {
    browser
        .page()
        .navigate(server.url("seat-booking.html"))
        .send()
        .await?;
    // Target discovery belongs to the root connection, not the opener's session.
    let mut targets = browser.connection().listen::<TargetCreated>();
    browser
        .connection()
        .execute(&SetDiscoverTargets::new(true))
        .await?;
    open_and_choose(browser, &mut targets).await?;
    browser
        .connection()
        .execute(&SetDiscoverTargets::new(false))
        .await?;
    println!("Seat B7 selected; the seat-picker popup is closed.");
    Ok(())
}

async fn open_and_choose(
    browser: &Browser,
    targets: &mut TypedEvents<TargetCreated>,
) -> Result<()> {
    let parent = browser.page();
    parent.locator("#choose-seat").click().await?;
    let target_id =
        tokio::time::timeout(Duration::from_secs(10), created_popup(targets, parent.id()))
            .await
            .map_err(|_| Error::Timeout("waiting for the seat-picker target".into()))??;
    let popup = browser
        .page_by_id(&target_id)
        .await?
        .ok_or_else(|| Error::InvalidArgument("seat-picker target disappeared".into()))?;
    tokio::time::timeout(Duration::from_secs(10), choose_seat(parent, &popup))
        .await
        .map_err(|_| Error::Timeout("selecting a seat in the popup".into()))??;
    popup.close().await
}

async fn created_popup(
    targets: &mut TypedEvents<TargetCreated>,
    opener: &TargetId,
) -> Result<TargetId> {
    loop {
        let info = targets.recv().await?.target_info;
        if info.type_ == "page" && info.opener_id.as_ref() == Some(opener) {
            return Ok(info.target_id);
        }
    }
}

async fn choose_seat(parent: &Page, popup: &Page) -> Result<()> {
    // A target-created event does not mean its document or application is ready.
    popup
        .locator("#picker[data-state=ready]")
        .wait_for(ElementState::Visible)
        .await?;
    popup.locator("#seat-b7").click().await?;
    parent
        .locator("#selection[data-state=selected]")
        .wait_for(ElementState::Visible)
        .await?;
    let selection: String = parent
        .call("() => document.querySelector('#selection').textContent")
        .json()
        .await?;
    if selection != "Seat B7 selected" {
        return Err(Error::InvalidArgument(format!(
            "unexpected seat selection: {selection}"
        )));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    book_seat(&browser, &server).await
}

//! Save a reading draft, restore it in a new document, and remove only its key.
mod common;
#[path = "common/server.rs"]
mod server;
use rscdp::{
    Page, Result,
    cdp::dom_storage::{
        StorageId,
        commands::{GetDomStorageItems, RemoveDomStorageItem},
    },
};

async fn preserve_draft(page: &Page, server: &server::Server) -> Result<()> {
    let url = server.url("reading-draft.html");
    let draft = "Read The Left Hand of Darkness";
    page.navigate(&url).send().await?;
    page.locator("#draft").fill(draft).await?;
    page.locator("#save").click().await?;
    let saved: String = page
        .call("() => document.querySelector('output').textContent")
        .json()
        .await?;
    println!("Save status: {saved}");

    // Navigating to the same URL creates a new document; the form must restore it.
    page.navigate(&url).send().await?;
    let restored: String = page
        .call("() => document.querySelector('#draft').value")
        .json()
        .await?;
    println!("Restored draft: {restored:?}");

    // Use this server's actual origin, including its dynamically allocated port.
    let origin: String = page.call("() => location.origin").json().await?;
    let storage = StorageId::new(true).with_security_origin(origin);
    // One-shot storage commands do not need DOMStorage.enable.
    let entries = page
        .session()
        .execute(&GetDomStorageItems::new(storage.clone()))
        .await?
        .entries;
    println!("Local storage entries: {entries:?}");
    page.session()
        .execute(&RemoveDomStorageItem::new(storage, "reading-draft"))
        .await?;
    page.navigate(&url).send().await?;
    let cleared: String = page
        .call("() => document.querySelector('#draft').value")
        .json()
        .await?;
    println!("Draft after removing its storage key and navigating: {cleared:?}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    preserve_draft(browser.page(), &server).await
}

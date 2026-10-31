//! Save a reading draft, restore it in a new document, and remove only its key.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{
    Error, Page, Result,
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
    if saved != "Draft saved" {
        return Err(Error::InvalidArgument(format!(
            "draft was not saved: {saved}"
        )));
    }

    // Navigating to the same URL creates a new document; the form must restore it.
    page.navigate(&url).send().await?;
    let restored: String = page
        .call("() => document.querySelector('#draft').value")
        .json()
        .await?;
    if restored != draft {
        return Err(Error::InvalidArgument(format!(
            "unexpected restored draft: {restored}"
        )));
    }

    // Use this server's actual origin, including its dynamically allocated port.
    let origin: String = page.call("() => location.origin").json().await?;
    let storage = StorageId::new(true).with_security_origin(origin);
    // One-shot storage commands do not need DOMStorage.enable.
    let entries = page
        .session()
        .execute(&GetDomStorageItems::new(storage.clone()))
        .await?
        .entries;
    if !entries
        .iter()
        .any(|entry| entry.0 == ["reading-draft", draft])
    {
        return Err(Error::InvalidArgument(
            "saved draft was absent from local storage".into(),
        ));
    }
    page.session()
        .execute(&RemoveDomStorageItem::new(storage, "reading-draft"))
        .await?;
    page.navigate(&url).send().await?;
    let cleared: String = page
        .call("() => document.querySelector('#draft').value")
        .json()
        .await?;
    if !cleared.is_empty() {
        return Err(Error::InvalidArgument(format!(
            "removed draft was restored: {cleared}"
        )));
    }
    println!("Restored {draft:?}; removing its storage key left the next visit empty.");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    preserve_draft(browser.page(), &server).await
}

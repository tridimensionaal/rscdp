//! Rename a reading list with a prompt, then cancel its deletion confirmation.
mod common;
use rustcdp::{
    Error, Page, Result,
    cdp::page::{DialogType, commands::HandleJavaScriptDialog, events::JavascriptDialogOpening},
    page::ElementState,
    session::TypedEvents,
};
use std::time::Duration;

async fn keep_reading_list(page: &Page) -> Result<()> {
    page.navigate(common::page_url("reading-list.html")?)
        .send()
        .await?;
    // Page already owns an enabled Page domain. Register before either click.
    let mut dialogs = page.session().listen::<JavascriptDialogOpening>();
    tokio::time::timeout(Duration::from_secs(10), async {
        // A click can wait for the modal to close, so service dialogs concurrently.
        tokio::try_join!(answer_dialogs(page, &mut dialogs), rename_and_cancel(page))
    })
    .await
    .map_err(|_| Error::Timeout("handling reading-list dialogs".into()))??;
    Ok(())
}

async fn answer_dialogs(
    page: &Page,
    dialogs: &mut TypedEvents<JavascriptDialogOpening>,
) -> Result<()> {
    for _ in 0..2 {
        let dialog = dialogs.recv().await?;
        println!("Dialog: {}", dialog.message);
        let answer = match dialog.type_ {
            DialogType::Prompt => {
                HandleJavaScriptDialog::new(true).with_prompt_text("Weekend reading")
            }
            DialogType::Confirm => HandleJavaScriptDialog::new(false),
            kind => {
                return Err(Error::InvalidArgument(format!(
                    "unsupported dialog: {kind:?}"
                )));
            }
        };
        page.session().execute(&answer).await?;
    }
    Ok(())
}

async fn rename_and_cancel(page: &Page) -> Result<()> {
    page.locator("#rename").click().await?;
    page.locator("#status[data-state=renamed]")
        .wait_for(ElementState::Visible)
        .await?;
    let name: String = page
        .call("() => document.querySelector('h1').textContent")
        .json()
        .await?;
    println!("Renamed list: {name}");
    page.locator("#delete").click().await?;
    page.locator("#status[data-state=kept]")
        .wait_for(ElementState::Visible)
        .await?;
    let (name, books, status): (String, Vec<String>, String) = page
        .call(
            "() => [document.querySelector('h1').textContent,
            Array.from(document.querySelectorAll('#books li'), book => book.textContent),
            document.querySelector('#status').textContent]",
        )
        .json()
        .await?;
    println!("{name}: {books:?}; {status}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    keep_reading_list(browser.page()).await
}

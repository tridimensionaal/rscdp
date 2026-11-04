//! Download a CSV into output/downloads/ and read its saved contents.
mod common;
#[path = "common/server.rs"]
mod server;
use rustcdp::{
    Browser, Error, Result,
    cdp::{
        browser::{
            commands::{SetDownloadBehavior, SetDownloadBehaviorBehavior},
            events::{DownloadProgress, DownloadProgressState, DownloadWillBegin},
        },
        page::FrameId,
    },
    session::TypedEvents,
};
use std::{path::Path, time::Duration};

async fn completed_download(
    beginnings: &mut TypedEvents<DownloadWillBegin>,
    progress: &mut TypedEvents<DownloadProgress>,
    url: &str,
    frame: &FrameId,
) -> Result<String> {
    let download = loop {
        let event = beginnings.recv().await?;
        if event.url == url && &event.frame_id == frame {
            break event;
        }
    };
    loop {
        let event = progress.recv().await?;
        if event.guid != download.guid {
            continue;
        }
        match event.state {
            DownloadProgressState::Completed => return Ok(download.guid),
            DownloadProgressState::InProgress => {}
            state => {
                return Err(Error::InvalidArgument(format!(
                    "download ended with state {}",
                    state.as_str()
                )));
            }
        }
    }
}

async fn export_history(browser: &Browser, server: &server::Server, output: &Path) -> Result<()> {
    let mut context = browser.new_context().await?;
    let context_id = context
        .id()
        .cloned()
        .ok_or_else(|| Error::InvalidArgument("isolated context ID missing".into()))?;
    let page = context.page();
    page.navigate(server.url("reading-export.html"))
        .send()
        .await?;
    let url: String = page
        .call("() => document.querySelector('#export').href")
        .json()
        .await?;
    let frame = page.main_frame().await?;
    // Downloads are browser-root events, not events on the page session.
    let mut beginnings = browser.connection().listen::<DownloadWillBegin>();
    let mut progress = browser.connection().listen::<DownloadProgress>();
    browser
        .connection()
        .execute(
            &SetDownloadBehavior::new(SetDownloadBehaviorBehavior::AllowAndName)
                .with_browser_context_id(context_id)
                .with_download_path(
                    output.to_str().ok_or_else(|| {
                        Error::InvalidArgument("download path must be UTF-8".into())
                    })?,
                )
                .with_events_enabled(true),
        )
        .await?;
    page.locator("#export").click().await?;
    let guid = tokio::time::timeout(
        Duration::from_secs(10),
        completed_download(&mut beginnings, &mut progress, &url, frame.id()),
    )
    .await
    .map_err(|_| Error::Timeout("waiting for reading-history download".into()))??;
    // allowAndName uses the GUID; suggestedFilename and filePath are not authoritative paths.
    let path = output.join(guid);
    let csv = tokio::fs::read_to_string(&path).await?;
    context.close().await?;
    println!("Downloaded {}:\n{csv}", path.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let output = Path::new("output/downloads");
    tokio::fs::create_dir_all(output).await?;
    let output = output.canonicalize()?;
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    export_history(&browser, &server, &output).await
}

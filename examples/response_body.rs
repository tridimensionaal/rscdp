//! Observe a cafe menu request and read its completed response body through CDP.
mod common;
#[path = "common/server.rs"]
mod server;
use base64::{Engine, engine::general_purpose::STANDARD};
use rustcdp::{
    Error, Page, Result,
    cdp::network::{
        RequestId,
        commands::{Enable, GetResponseBody},
        events::{LoadingFailed, LoadingFinished, RequestWillBeSent, ResponseReceived},
    },
    connection::{EventOptions, EventStream},
};
use serde_json::Value;
use std::time::Duration;

async fn read_menu(page: &Page, server: &server::Server) -> Result<()> {
    page.navigate(server.url("cafe.html")).send().await?;
    // One ordered stream preserves the relationship between headers and completion.
    // Subscribe before enabling Network and before the action that starts fetch.
    let mut traffic = page
        .session()
        .subscribe_with(EventOptions::default().methods([
            "Network.requestWillBeSent",
            "Network.responseReceived",
            "Network.loadingFinished",
            "Network.loadingFailed",
        ]))?;
    page.session().execute(&Enable::new()).await?;
    let body = tokio::time::timeout(Duration::from_secs(10), async {
        page.require_element("#load-menu").await?.click().await?;
        completed_body(page, &mut traffic, &server.url("api/menu.json")).await
    })
    .await
    .map_err(|_| Error::Timeout("reading the cafe menu response".into()))??;

    let menu: Value = serde_json::from_slice(&body)?;
    if menu["special"]["name"] != "Orange blossom latte" || menu["special"]["price"] != 4.5 {
        return Err(Error::InvalidArgument(format!("unexpected menu: {menu}")));
    }
    println!(
        "Captured {} response bytes: {}",
        body.len(),
        menu["special"]
    );
    // This example owns the page/session; browser cleanup ends Network reporting.
    Ok(())
}

async fn completed_body(page: &Page, traffic: &mut EventStream, url: &str) -> Result<Vec<u8>> {
    let mut request_id: Option<RequestId> = None;
    let mut response_received = false;
    loop {
        // recv propagates event loss and connection/session closure instead of
        // silently returning an incomplete capture.
        let event = traffic.recv().await?;
        match event.method.as_str() {
            "Network.requestWillBeSent" => {
                let request: RequestWillBeSent = serde_json::from_value(event.params)?;
                if request.request.url == url {
                    request_id = Some(request.request_id);
                }
            }
            "Network.responseReceived" => {
                let response: ResponseReceived = serde_json::from_value(event.params)?;
                if request_id.as_ref() == Some(&response.request_id) {
                    if response.response.status != 200 {
                        return Err(Error::InvalidArgument(format!(
                            "menu request returned HTTP {}",
                            response.response.status
                        )));
                    }
                    response_received = true;
                }
            }
            "Network.loadingFailed" => {
                let failure: LoadingFailed = serde_json::from_value(event.params)?;
                if request_id.as_ref() == Some(&failure.request_id) {
                    return Err(Error::InvalidArgument(format!(
                        "menu request failed: {}",
                        failure.error_text
                    )));
                }
            }
            "Network.loadingFinished" => {
                let finished: LoadingFinished = serde_json::from_value(event.params)?;
                if request_id.as_ref() == Some(&finished.request_id) {
                    if !response_received {
                        return Err(Error::InvalidArgument(
                            "menu response headers were not observed".into(),
                        ));
                    }
                    // Headers alone aren't enough: request the body only after completion.
                    let response = page
                        .session()
                        .execute(&GetResponseBody::new(finished.request_id))
                        .await?;
                    return if response.base64_encoded {
                        Ok(STANDARD.decode(response.body)?)
                    } else {
                        Ok(response.body.into_bytes())
                    };
                }
            }
            _ => {}
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let server = server::Server::start().await?;
    let browser = common::launch().await?;
    read_menu(browser.page(), &server).await?;
    Ok(())
}

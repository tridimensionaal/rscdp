//! Real-browser proxy regressions; run with `--ignored` and CHROME_PATH.
use rscdp::{
    Browser, Result,
    cdp::{fetch::events::RequestPaused, network::events::RequestWillBeSent},
    proxy::ProxyAuthStatus,
};
use serde_json::json;
use std::time::Duration;

#[path = "support/auth_proxy.rs"]
mod auth_proxy;
use auth_proxy::AuthProxy;

const TIMEOUT: Duration = Duration::from_secs(5);

async fn launch(proxy: &AuthProxy) -> Result<Browser> {
    Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .proxy(proxy.proxy())
        .launch()
        .await
}

async fn queued_request_progresses(cancel: bool) -> Result<()> {
    let proxy = AuthProxy::start();
    let mut browser = launch(&proxy).await?;
    let page = browser.page();
    // Keep the cold gate closed: no navigation or successful response first,
    // and no independent Network owner to hide a missing worker acquisition.
    assert_eq!(
        page.evaluate("location.href").json::<String>().await?,
        "about:blank"
    );
    let mut paused = page.session().listen::<RequestPaused>();
    page.call(
        r#"url => {
        globalThis.abortHold = new AbortController();
        globalThis.heldResult = fetch(url, {mode: 'no-cors', signal: abortHold.signal})
            .then(() => 'resolved', error => error.name);
    }"#,
    )
    .arg(json!(proxy.url("/hold")))
    .run()
    .await?;
    proxy.wait_for_authenticated_request("/hold").await?;
    page.call(
        r#"url => {
        globalThis.healthyResult = fetch(url, {mode: 'no-cors'})
            .then(() => 'resolved', error => error.name);
    }"#,
    )
    .arg(json!(proxy.url("/healthy")))
    .run()
    .await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let event = paused.recv().await?;
            if event.request.url == proxy.url("/healthy") {
                assert!(event.response_status_code.is_none());
                return Ok::<(), rscdp::Error>(());
            }
        }
    })
    .await
    .expect("healthy request never reached Fetch interception")?;
    assert_eq!(proxy.authenticated_requests("/healthy"), 0);

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    if cancel {
        page.call("() => { abortHold.abort(); }").run().await?;
    } else {
        proxy.close_without_headers("/hold");
    }
    let held = tokio::time::timeout_at(deadline, page.evaluate("heldResult").json::<String>())
        .await
        .expect("held request promise did not terminate")?;
    assert_eq!(held, if cancel { "AbortError" } else { "TypeError" });
    tokio::time::timeout_at(deadline, async {
        proxy.wait_for_authenticated_request("/healthy").await?;
        assert_eq!(
            page.evaluate("healthyResult").json::<String>().await?,
            "resolved"
        );
        Ok::<(), rscdp::Error>(())
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "healthy request remained paused after held promise returned {held}; authenticated hold={}, healthy={}; worker={:?}",
            proxy.authenticated_requests("/hold"),
            proxy.authenticated_requests("/healthy"),
            page.proxy_auth_status()
        )
    })?;
    assert_eq!(
        proxy.authenticated_requests("/hold"),
        1,
        "held request was replayed"
    );
    assert_eq!(proxy.authenticated_requests("/healthy"), 1);
    assert!(matches!(
        page.proxy_auth_status(),
        Some(
            ProxyAuthStatus::Pending
                | ProxyAuthStatus::Authenticated
                | ProxyAuthStatus::Unchallenged
        )
    ));
    browser.close().await?;
    drop(proxy);
    Ok(())
}

#[tokio::test]
#[ignore = "requires Chrome; verifies cancellation advances cold proxy traffic"]
async fn canceling_authenticated_cold_request_unblocks_next_request() -> Result<()> {
    queued_request_progresses(true).await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies proxy EOF still advances cold proxy traffic"]
async fn proxy_eof_retires_probe_and_unblocks_next_request() -> Result<()> {
    queued_request_progresses(false).await
}

#[tokio::test]
#[ignore = "requires Chrome; verifies authentication cleanup preserves shared Network ownership"]
async fn stopping_proxy_auth_preserves_another_network_guard() -> Result<()> {
    let proxy = AuthProxy::start();
    let mut browser = launch(&proxy).await?;
    let page = browser.page();
    let network = page.session().enable_network().await?;
    let mut requests = page.session().listen::<RequestWillBeSent>();
    page.stop_proxy_auth().await?;
    assert_eq!(page.proxy_auth_status(), Some(ProxyAuthStatus::Stopped));
    let url = proxy.url("/healthy");
    page.call("url => { void fetch(url, {mode: 'no-cors'}).catch(() => {}); }")
        .arg(json!(url))
        .run()
        .await?;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if requests.recv().await?.request.url == url {
                return Ok::<(), rscdp::Error>(());
            }
        }
    })
    .await
    .expect("stopping proxy authentication disabled another owner's Network events")?;
    network.close().await?;
    browser.close().await?;
    drop(proxy);
    Ok(())
}

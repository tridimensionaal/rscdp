use rscdp::{Browser, Error, Page, cdp::network::events::LoadingFinished, page::Response};
use std::time::Duration;

#[path = "support/response_server.rs"]
mod response_server;
use response_server::ResponseServer;

#[tokio::test]
#[ignore = "requires Chrome; verifies arming, shared observation and deadlines"]
async fn response_observers_are_armed_before_navigation() {
    let browser = Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
        .unwrap();
    let page = browser.page();
    assert!(matches!(
        page.expect_response("data:text/html,observed")
            .timeout(Duration::ZERO)
            .prepare()
            .await,
        Err(Error::Timeout(_))
    ));
    let url = "data:text/html,observed";
    let first = page.expect_response(url).prepare().await.unwrap();
    let second = page.expect_response(url).prepare().await.unwrap();
    page.navigate(url).send().await.unwrap();
    let first = first.wait().await.unwrap();
    let second = second.wait().await.unwrap();
    assert_eq!(first.metadata().url, url);
    assert_eq!(first.request_id(), second.request_id());
    assert!(!first.is_redirect());
    assert_eq!(first.session().id(), page.session().id());
    drop(first);
    drop(second);
    let missing = page
        .expect_response("https://not-requested.invalid/")
        .timeout(Duration::from_millis(100))
        .prepare()
        .await
        .unwrap();
    assert!(matches!(missing.wait().await, Err(Error::Timeout(_))));
    let expired = page
        .expect_response(url)
        .timeout(Duration::from_millis(150))
        .prepare()
        .await
        .unwrap();
    page.navigate(url).send().await.unwrap();
    tokio::time::sleep(Duration::from_millis(170)).await;
    assert!(matches!(expired.wait().await, Err(Error::Timeout(_))));
    let closed = page.expect_response(url).prepare().await.unwrap();
    page.close().await.unwrap();
    assert!(matches!(closed.wait().await, Err(Error::SessionClosed(_))));
}

async fn body_fixture() -> (ResponseServer, Browser) {
    let server = ResponseServer::start();
    let browser = Browser::builder()
        .headless(true)
        .arg("--no-sandbox")
        .launch()
        .await
        .unwrap();
    browser
        .page()
        .navigate(server.url("/"))
        .send()
        .await
        .unwrap();
    (server, browser)
}

async fn fetch(page: &Page, url: &str) {
    let url = serde_json::to_string(url).unwrap();
    // Start without awaiting completion; drain the browser-side body, including
    // failed loads, without leaving a rejected promise in the document.
    page.evaluate(format!(
        "void fetch({url}, {{cache: 'no-store'}}).then(r => r.arrayBuffer()).catch(() => {{}})"
    ))
    .run()
    .await
    .unwrap();
}

async fn observe(page: &Page, server: &ResponseServer, path: &str) -> Response {
    let url = server.url(path);
    let prepared = page.expect_response(&url).prepare().await.unwrap();
    fetch(page, &url).await;
    prepared.wait().await.unwrap()
}

async fn completed(page: &Page, server: &ResponseServer, path: &str) -> Response {
    // Independent evidence that completion is already queued on the response's
    // original stream, before its body terminal is called.
    let mut finished = page.session().listen::<LoadingFinished>();
    let response = observe(page, server, path).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if finished.recv().await.unwrap().request_id == *response.request_id() {
                break;
            }
        }
    })
    .await
    .expect("selected request did not finish");
    response
}

#[tokio::test]
#[ignore = "requires Chrome; verifies headers can precede body completion"]
async fn response_body_waits_for_completion() {
    let (server, browser) = body_fixture().await;
    let response = observe(browser.page(), &server, "/delayed").await;
    let body = response.body();
    tokio::pin!(body);
    assert!(
        tokio::time::timeout(Duration::from_millis(75), &mut body)
            .await
            .is_err(),
        "body resolved while the server still withheld its suffix"
    );
    server.release_body();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), body)
            .await
            .unwrap()
            .unwrap(),
        b"delayed body"
    );
}

#[tokio::test]
#[ignore = "requires Chrome; verifies queued completion, decoding and HTTP error bodies"]
async fn completed_response_bodies_decode_bytes_text_and_json() {
    let (server, browser) = body_fixture().await;
    let page = browser.page();
    assert_eq!(
        completed(page, &server, "/binary")
            .await
            .body()
            .await
            .unwrap(),
        b"\x00\xff\x80\x01"
    );
    assert_eq!(
        completed(page, &server, "/text")
            .await
            .text()
            .await
            .unwrap(),
        "hello, café"
    );
    assert_eq!(
        completed(page, &server, "/json")
            .await
            .json::<serde_json::Value>()
            .await
            .unwrap(),
        serde_json::json!({"answer": 42})
    );
    assert!(matches!(
        completed(page, &server, "/binary").await.text().await,
        Err(Error::Utf8(_))
    ));
    let missing = completed(page, &server, "/missing").await;
    assert_eq!(missing.metadata().status, 404);
    assert_eq!(missing.text().await.unwrap(), "useful missing body");
}

#[tokio::test]
#[ignore = "requires Chrome; verifies failures after headers and redirect rejection"]
async fn failed_load_and_redirect_have_no_readable_body() {
    let (server, browser) = body_fixture().await;
    let failed = observe(browser.page(), &server, "/truncated").await;
    server.release_body();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), failed.body())
            .await
            .unwrap(),
        Err(Error::ResponseBody(message)) if !message.is_empty()
    ));

    let redirect = completed(browser.page(), &server, "/redirect").await;
    assert!(redirect.is_redirect());
    assert_eq!(redirect.metadata().status, 302);
    // The redirect's request ID has completed at /text. Reading it must still
    // reject the selected hop, rather than return the destination's body.
    assert!(matches!(
        redirect.body().await,
        Err(Error::ResponseBody(message)) if !message.is_empty()
    ));
}

#[tokio::test]
#[ignore = "requires Chrome; verifies the original deadline applies to late body reads"]
async fn completed_response_body_expires_before_read() {
    let (server, browser) = body_fixture().await;
    let page = browser.page();
    let url = server.url("/text");
    let mut finished = page.session().listen::<LoadingFinished>();
    let budget = Duration::from_secs(2);
    let prepared = page
        .expect_response(&url)
        .timeout(budget)
        .prepare()
        .await
        .unwrap();
    fetch(page, &url).await;
    let response = prepared.wait().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if finished.recv().await.unwrap().request_id == *response.request_id() {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(budget).await;
    assert!(matches!(response.body().await, Err(Error::Timeout(_))));
}

#[tokio::test]
#[ignore = "requires Chrome; verifies overlapping same-URL requests retain their own bodies"]
async fn same_url_responses_keep_selected_request_identity() {
    let (server, browser) = body_fixture().await;
    let page = browser.page();
    let first = observe(page, &server, "/same").await;
    // The first body is still held open when the second request completes.
    let second = completed(page, &server, "/same").await;
    assert_eq!(first.metadata().url, second.metadata().url);
    assert_ne!(first.request_id(), second.request_id());
    assert_eq!(second.text().await.unwrap(), "second");
    server.release_body();
    assert_eq!(first.text().await.unwrap(), "first");
}

use rustcdp::{Browser, Error};
use std::time::Duration;

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

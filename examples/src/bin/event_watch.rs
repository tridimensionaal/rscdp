//! Collect structured console events from a local order-dispatch simulation.
mod common;
use rustcdp::{
    Error, Page, Result,
    cdp::runtime::{commands::Enable, events::ConsoleApiCalled},
};
use std::time::Duration;

const DISPATCH: &str = r#"<!doctype html><title>Dispatch console</title><h1>Order #1042</h1>
<ol id="timeline"></ol><script>
['queued', 'packed', 'ready for collection'].forEach((state, index) => {
  setTimeout(() => {
    const item = document.createElement('li'); item.textContent = state;
    document.querySelector('ol').append(item);
    console.info('dispatch-demo', 1042, state);
  }, index * 150);
});
</script>"#;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = common::launch().await?;
    let result = watch_dispatch(browser.page()).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn watch_dispatch(page: &Page) -> Result<()> {
    page.session().execute(&Enable::new()).await?;
    let mut events = page.session().listen::<ConsoleApiCalled>();
    page.navigate(&common::html_url(DISPATCH)).await?;
    tokio::time::timeout(
        Duration::from_secs(10),
        collect_dispatch_events(&mut events),
    )
    .await
    .map_err(|_| Error::Timeout("waiting for dispatch events".into()))??;
    Ok(())
}

async fn collect_dispatch_events(
    events: &mut rustcdp::session::TypedEvents<ConsoleApiCalled>,
) -> Result<()> {
    let mut received = 0;
    while received < 3 {
        let event = events.recv().await?;
        if event
            .args
            .first()
            .and_then(|arg| arg.value.as_ref())
            .and_then(|v| v.as_str())
            != Some("dispatch-demo")
        {
            continue;
        }
        let values: Vec<_> = event
            .args
            .iter()
            .filter_map(|arg| arg.value.as_ref())
            .collect();
        println!("{}", serde_json::to_string(&values)?);
        received += 1;
    }
    Ok(())
}

use rustcdp::{
    Browser, Error, Result,
    launcher::LaunchOptions,
    proxy::{Proxy, ProxyAuthMode},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let proxy: Proxy = std::env::var("CDP_PROXY")
        .map_err(|_| Error::InvalidArgument("set CDP_PROXY".into()))?
        .parse()?;
    let mut browser = Browser::launch(LaunchOptions {
        args: std::iter::once("--headless=new".into())
            .chain(std::env::args_os().skip(1))
            .collect(),
        ..Default::default()
    })
    .await?;
    let result = async {
        let mut context = browser
            .new_context_with_proxy(proxy, ProxyAuthMode::OnDemand)
            .await?;
        let result = async {
            let url = std::env::var("CDP_URL").unwrap_or_else(|_| "https://example.org".into());
            context
                .page()
                .navigate_and_wait(&url, Duration::from_secs(15))
                .await?;
            context.wait_for_proxy_auth(Duration::from_secs(15)).await?;
            println!("Context authentication: {:?}", context.proxy_auth_status());
            println!(
                "Title: {}",
                context.page().evaluate("document.title").await?
            );
            Ok(())
        }
        .await;
        let closed = context.close().await;
        result.and(closed)
    }
    .await;
    browser.close().await?;
    result
}

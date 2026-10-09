use rustcdp::{
    Browser, Error, Result, launcher::LaunchOptions, proxy::Proxy, proxy_auth::ProxyAuth,
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let proxy: Proxy = std::env::var("CDP_PROXY")
        .map_err(|_| Error::InvalidArgument("set CDP_PROXY to a proxy URL".into()))?
        .parse()?;
    let mut args = vec![
        "--headless=new".into(),
        format!("--proxy-server={}", proxy.server()).into(),
    ];
    args.extend(std::env::args_os().skip(1));
    let mut browser = Browser::launch(LaunchOptions {
        args,
        ..Default::default()
    })
    .await?;
    let result = async {
        let mut auth = ProxyAuth::start(browser.page().session().clone(), proxy).await?;
        let result = async {
            let url = std::env::var("CDP_URL").unwrap_or_else(|_| "https://example.org".into());
            browser
                .page()
                .navigate_and_wait(&url, Duration::from_secs(15))
                .await?;
            auth.wait(Duration::from_secs(15)).await?;
            println!("Proxy authentication: {:?}", auth.status());
            println!(
                "Title: {}",
                browser.page().evaluate("document.title").await?
            );
            Ok(())
        }
        .await;
        let stopped = auth.stop().await;
        result.and(stopped)
    }
    .await;
    browser.close().await?;
    result
}

use rustcdp::{Browser, Result, launcher::LaunchOptions};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let options = LaunchOptions {
        args: std::iter::once("--headless=new".into())
            .chain(std::env::args_os().skip(1))
            .collect(),
        ..Default::default()
    };
    let mut browser = Browser::launch(options).await?;
    let result = async {
        browser
            .page()
            .navigate_and_wait(
                "data:text/html,<title>Rust CDP</title><h1>Hello</h1>",
                Duration::from_secs(10),
            )
            .await?;
        println!(
            "Title: {}",
            browser.page().evaluate("document.title").await?
        );
        let mut context = browser.new_context().await?;
        let result = async {
            context
                .page()
                .navigate_and_wait(
                    "data:text/html,<h1>Isolated context</h1>",
                    Duration::from_secs(10),
                )
                .await?;
            println!("HTML: {}", context.page().html().await?);
            Ok::<_, rustcdp::Error>(())
        }
        .await;
        context.close().await?;
        result
    }
    .await;
    browser.close().await?;
    result
}

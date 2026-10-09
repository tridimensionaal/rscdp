use rustcdp::{
    Browser, Result,
    launcher::LaunchOptions,
    page::{ClickMethod, WaitOptions},
};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = Browser::launch(LaunchOptions {
        args: std::iter::once("--headless=new".into())
            .chain(std::env::args_os().skip(1))
            .collect(),
        ..Default::default()
    })
    .await?;
    let result = async {
        let page = browser.page();
        page.navigate_and_wait(
            "data:text/html,<button onclick=\"this.textContent='Clicked'\">Click me</button>",
            Duration::from_secs(10),
        )
        .await?;
        let found = page
            .wait_for_xpath(
                "//button",
                WaitOptions {
                    stability_time: Duration::ZERO,
                    delay_after: Duration::ZERO,
                    ..Default::default()
                },
            )
            .await?;
        println!("Button found: {found}");
        page.click_xpath("//button", ClickMethod::Cdp, true).await?;
        println!(
            "Button text: {}",
            page.evaluate("document.querySelector('button').textContent")
                .await?
        );
        let nodes = page.nodes_by_xpath("//button", false).await?;
        println!("DOM matches: {}", nodes.len());
        if let Some(node) = nodes.first() {
            page.resolve_node(node)
                .await?
                .click(ClickMethod::JavaScript, false)
                .await?;
        }
        page.press_escape().await?;
        page.flash_point(20.0, 20.0, Duration::from_millis(50), 12.0)
            .await?;
        Ok(())
    }
    .await;
    browser.close().await?;
    result
}

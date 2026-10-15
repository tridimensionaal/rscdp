use rustcdp::{Browser, Result, launcher::LaunchOptions};

pub async fn launch() -> Result<Browser> {
    Browser::launch(LaunchOptions {
        args: std::env::args_os().skip(1).collect(),
        ..Default::default()
    })
    .await
}

pub fn html_url(html: &str) -> String {
    format!(
        "data:text/html;charset=utf-8,{}",
        percent_encoding::utf8_percent_encode(html, percent_encoding::NON_ALPHANUMERIC)
    )
}

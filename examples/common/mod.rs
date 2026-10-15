use rustcdp::{Browser, Result};

pub async fn launch() -> Result<Browser> {
    Browser::builder()
        .args(std::env::args_os().skip(1))
        .launch()
        .await
}

pub fn html_url(html: &str) -> String {
    format!(
        "data:text/html;charset=utf-8,{}",
        percent_encoding::utf8_percent_encode(html, percent_encoding::NON_ALPHANUMERIC)
    )
}

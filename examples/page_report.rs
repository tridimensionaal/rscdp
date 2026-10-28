//! Export the same locally rendered report to PNG and PDF.
mod common;
use rustcdp::{Page, Result};
use std::{path::PathBuf, time::Duration};

async fn export_report(page: &Page, output: &std::path::Path) -> Result<()> {
    page.navigate(common::page_url("report.html")?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let png = page.screenshot().full_page(true).capture().await?;
    let pdf = page.pdf().print_background(true).generate().await?;
    tokio::fs::write(output.join("workshop.png"), png).await?;
    tokio::fs::write(output.join("workshop.pdf"), pdf).await?;
    println!("Report files: {}", output.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let output = std::env::var_os("RUSTCDP_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rustcdp-report"));
    tokio::fs::create_dir_all(&output).await?;
    let browser = common::launch().await?;
    export_report(browser.page(), &output).await?;
    Ok(())
}

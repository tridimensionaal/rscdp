//! Export the same locally rendered report to PNG and PDF in output/.
mod common;
use rscdp::{Page, Result};
use std::time::Duration;

async fn export_report(page: &Page) -> Result<()> {
    page.navigate(common::page_url("report.html")?)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let png = page.screenshot().full_page(true).capture().await?;
    let pdf = page.pdf().print_background(true).generate().await?;
    tokio::fs::write("output/workshop.png", png).await?;
    tokio::fs::write("output/workshop.pdf", pdf).await?;
    println!("Saved output/workshop.png and output/workshop.pdf");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::fs::create_dir_all("output").await?;
    let browser = common::launch().await?;
    export_report(browser.page()).await?;
    Ok(())
}

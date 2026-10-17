//! Export the same locally rendered report to PNG and PDF.
mod common;
use rustcdp::{Page, Result};
use std::{path::PathBuf, time::Duration};

const REPORT: &str = r#"<!doctype html><meta charset="utf-8"><title>Workshop report</title>
<style>body{font:18px system-ui;background:#f4f0e8;color:#263b38;padding:35px}
article{max-width:620px;margin:auto}h1{font-size:42px;margin:0}small{letter-spacing:2px}
table{width:100%;border-collapse:collapse;margin:24px 0}td,th{text-align:left;padding:12px;border-bottom:1px solid #aab8ae}
aside{background:#d5e5d8;padding:20px;border-radius:12px}footer{margin-top:30px;font-size:13px}
@media print{body{background:white;padding:0}}</style>
<article><small>MAKERS GUILD · WEEK 42</small><h1>A good week of making.</h1>
<p>Workshop attendance and materials, ready to share.</p>
<table><tr><th>Workshop</th><th>People</th><th>Projects finished</th></tr>
<tr><td>Bookbinding</td><td>18</td><td>16</td></tr><tr><td>Woodworking</td><td>12</td><td>9</td></tr>
<tr><td>Ceramics</td><td>24</td><td>21</td></tr></table>
<aside><strong>46 projects finished</strong><br>Next week: open studio on Thursday.</aside>
<footer>Sample data · rendered locally by rustcdp</footer></article>"#;

#[tokio::main]
async fn main() -> Result<()> {
    let output = std::env::var_os("RUSTCDP_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("rustcdp-report"));
    tokio::fs::create_dir_all(&output).await?;
    let mut browser = common::launch().await?;
    let result = export_report(browser.page(), &output).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn export_report(page: &Page, output: &std::path::Path) -> Result<()> {
    page.navigate(common::html_url(REPORT))
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

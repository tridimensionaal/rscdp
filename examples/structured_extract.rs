//! Pass Rust data into browser functions and collect a typed catalogue summary.
mod common;
use rustcdp::{Error, Page, Result};
use serde_json::json;

async fn extract_seeds(page: &Page) -> Result<()> {
    page.navigate(common::page_url("seeds.html")?)
        .send()
        .await?;
    // Arguments travel separately from JavaScript source, including quotes.
    let seeds: Vec<(String, u64)> = page
        .call(
            r#"filters => Array.from(document.querySelectorAll('tbody tr'))
            .filter(row => row.dataset.category === filters.category &&
                Number(row.dataset.stock) >= filters.minimum)
            .map(row => [row.cells[0].textContent, Number(row.dataset.stock)])"#,
        )
        .arg(json!({"category": "Herbs & 'greens'", "minimum": 5}))
        .json()
        .await?;
    if seeds != [("Basil".into(), 12), ("Coriander".into(), 8)] {
        return Err(Error::InvalidArgument(format!(
            "unexpected seed selection: {seeds:?}"
        )));
    }
    let basil = page.require_element("#basil").await?;
    // A normal function binds `this` to this element; an arrow function would not.
    let label: String = basil
        .call("function(prefix) { return prefix + ': ' + this.cells[0].textContent; }")
        .arg(json!("Huerto de Lucía 🌱"))
        .json()
        .await?;
    basil.release().await?;
    if label != "Huerto de Lucía 🌱: Basil" {
        return Err(Error::InvalidArgument(format!(
            "unexpected seed label: {label}"
        )));
    }
    println!(
        "{label}; selected {seeds:?}, {} packets total",
        seeds.iter().map(|(_, count)| count).sum::<u64>()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = common::launch().await?;
    extract_seeds(browser.page()).await
}

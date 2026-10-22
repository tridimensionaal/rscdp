use rustcdp::{Browser, Result};

pub async fn launch() -> Result<Browser> {
    Browser::builder()
        .args(std::env::args_os().skip(1))
        .launch()
        .await
}

/// Resolve a page on disk independently of the process working directory.
#[allow(dead_code)] // HTTP examples use the localhost server instead.
pub fn page_url(name: &str) -> Result<url::Url> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("pages")
        .join(name)
        .canonicalize()?;
    url::Url::from_file_path(&path).map_err(|()| {
        rustcdp::Error::InvalidArgument(format!("cannot create a file URL for {}", path.display()))
    })
}

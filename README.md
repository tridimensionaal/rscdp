# rustcdp

An asynchronous Rust library for Chrome DevTools Protocol (CDP): typed commands
and events, browser contexts, and page automation. Uses Tokio.

## Quickstart

Requires Rust 1.99.1 or newer and Chrome/Chromium. Set `CHROME_PATH` if Chrome
is not discoverable on your system.

Add these dependencies, adjusting the path to the library in your checkout:

```toml
[dependencies]
rustcdp = { path = "../rustcdp/rustcdp" }
tokio = { version = "1.54.1", features = ["macros", "rt-multi-thread"] }
```

```rust
use rustcdp::{Browser, Page, Result};
use std::time::Duration;

async fn read_page(page: &Page) -> Result<()> {
    page.navigate("https://example.com")
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    println!("{}", page.evaluate("document.title").json::<String>().await?);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let browser = Browser::builder().launch().await?;
    read_page(browser.page()).await?;
    Ok(())
}
```

This program opens Chrome, visits example.com, and prints the page title.
Add `.headless(true)` before `.launch()` to run without a window.

Use the [browser builder](rustcdp/src/browser/builder.rs) for launch settings,
including profiles, proxies, and Chrome arguments. Use `Browser::connect(endpoint)`
to attach to an existing browser.

## Examples

See [examples](examples/README.md) for runnable examples and instructions.

## API documentation

Generate and open the API reference for this checkout:

```sh
cargo doc -p rustcdp --no-deps --open
```

## Development

See [development instructions](scripts/README.md) for checks and running all examples.

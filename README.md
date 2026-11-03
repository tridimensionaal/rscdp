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

This program opens Chrome, visits example.com, and prints the page title.
Add `.headless(true)` before `.launch()` to run without a window.

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

Use the [browser builder](rustcdp/src/browser/builder.rs) for launch settings,
including profiles, proxies, and Chrome arguments. Use `Browser::connect(endpoint)`
to attach to an existing browser.

## Examples

See [examples](examples/README.md) for runnable examples and instructions.

## API documentation

- [Browser](rustcdp/src/browser.rs), [Page](rustcdp/src/page.rs), and
  [Element](rustcdp/src/page/element.rs) provide browser automation helpers.
- [Connection](rustcdp/src/connection.rs), [Session](rustcdp/src/session.rs), and
  [typed CDP bindings](rustcdp/src/lib.rs) expose commands and events directly.
- Enable the optional `tracing` feature for diagnostics; install your own subscriber.

Build the API reference locally:

```sh
cargo doc -p rustcdp --no-deps --open
```

## Development

The workspace contains the library in `rustcdp/` and the examples in `examples/`.
CDP bindings are generated at build time from the pinned schemas in
[rustcdp/protocol](rustcdp/protocol), without network access.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p rustcdp
```

Run browser checks explicitly (Linux, Python 3.11+; `CHROME_PATH` if needed):

```sh
cargo test -p rustcdp -- --ignored --test-threads=1
cargo build -p rustcdp-examples --examples
python3 scripts/browser_smoke.py
```

# rscdp

Async Chrome and Chromium automation in Rust, with typed access to the
Chrome DevTools Protocol (CDP).

## Quickstart

You need Rust 1.99.1 or newer and Chrome or Chromium. Set `CHROME_PATH` if the
browser is not discoverable on your system.

Add these dependencies to your application’s `Cargo.toml`:

```toml
[dependencies]
rustcdp = "0.1"
tokio = { version = "1.54.1", features = ["macros", "rt-multi-thread"] }
```

Here is a complete example:

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

The [browser builder](rustcdp/src/browser/builder.rs) also supports profiles,
proxies, and Chrome arguments. To attach to an existing browser, use
`Browser::connect(endpoint)`.

## Documentation and examples

Explore the [examples](examples/README.md) for complete workflows and instructions
for running them. To browse the API reference for this checkout, run:

```sh
cargo doc -p rustcdp --no-deps --open
```

## CDP bindings

[Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)
defines the messages exchanged with the browser. The bindings represent these
commands, responses, and events as Rust types, so you can work with typed values
instead of constructing protocol messages by hand. Cargo generates the bindings
automatically during builds from the pinned schemas. See the
[generation instructions](rustcdp/protocol/README.md#generating-bindings) for details
and schema updates.

## Contributing

If you’re changing the library or its examples, the [contributor guide](CONTRIBUTING.md)
explains which formatting, test, and browser checks to run.

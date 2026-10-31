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

The launch examples use local [HTML pages](examples/pages), with a localhost server where
cookies or HTTP requests are needed. No external site is required. They open a
Chrome window by default; append `-- --headless=new` for headless mode.

```sh
cargo run -p rustcdp-examples --example checkout
```

Replace `checkout` with a launch example below:

| Example | Demonstrates |
| --- | --- |
| [navigation](examples/navigation.rs) | Observe a click-triggered navigation and wait for application readiness |
| [connect](examples/connect.rs) | Inspect an existing browser and dispose a temporary tab without stopping Chrome |
| [native_controls](examples/native_controls.rs) | Save account preferences with native select and checkbox helpers |
| [structured_extract](examples/structured_extract.rs) | Pass JSON arguments and extract a typed seed catalogue |
| [live_search](examples/live_search.rs) | Reuse locators across changing search results and wait for DOM states |
| [parallel_pages](examples/parallel_pages.rs) | Collect independent warehouse summaries concurrently |
| [checkout](examples/checkout.rs) | Fill and submit a form with Unicode text and keyboard input |
| [page_report](examples/page_report.rs) | Export a report to PNG and PDF |
| [frame_walk](examples/frame_walk.rs) | Traverse nested frames and update inventory |
| [event_watch](examples/event_watch.rs) | Subscribe to console events before navigation |
| [browser_contexts](examples/browser_contexts.rs) | Isolate cookies and share them between tabs in one context |
| [response_body](examples/response_body.rs) | Correlate network events and read a completed response body |
| [request_interception](examples/request_interception.rs) | Continue requests, replace responses, and block analytics |
| [file_upload](examples/file_upload.rs) | Select a CSV and read its local preview; no server upload |

`connect` takes an existing browser endpoint and optional target ID instead of
Chrome launch flags:

```sh
cargo run -p rustcdp-examples --example connect -- http://127.0.0.1:9222
```

Report files go to `rustcdp-report/` in the system temporary directory;
set `RUSTCDP_OUTPUT` to choose another location. Example dependencies are separate
from the library's dependencies.

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

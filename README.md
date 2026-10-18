# rustcdp

An asynchronous Rust library for Chrome DevTools Protocol (CDP): typed commands
and events, browser and context management, and page automation.

## Quickstart

Requires Rust 1.99.1 or newer; this repository pins 1.99.1 through rustup.
Install Chrome/Chromium or set `CHROME_PATH` to its executable.

Add these dependencies to your application's `Cargo.toml`, adjusting the path to the library directory inside this checkout:

```toml
[dependencies]
rustcdp = { path = "../rustcdp/rustcdp" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

This program opens a Chrome window and reads a local page. It needs no website
or account. To run headless, add `.headless(true)` before `.launch()`.

```rust
use rustcdp::{Browser, Page, Result};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = Browser::builder().launch().await?;

    let result = read_page(browser.page()).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn read_page(page: &Page) -> Result<()> {
    page.navigate(
        "data:text/html,<title>Workshop</title><h1>Hello%20from%20Rust</h1>",
    )
    .timeout(Duration::from_secs(15))
    .send()
    .await?;
    println!("{}", page.evaluate("document.title").json::<String>().await?);
    println!("{}", page.html().await?);
    Ok(())
}
```

The named operation lets `?` return an error while `main` still awaits cleanup.
If both fail, this example returns the operation error. Your application owns the
Tokio runtime; keep it running until awaited cleanup finishes.

The [browser builder](rustcdp/src/browser/builder.rs) lists supported settings and
their defaults, including window size, DevTools, executable, profile, and startup
timeout. Use `.arg()` or `.args()` for additional Chrome flags. Setting the same
option through both a typed setter and a raw flag returns an error; use one form.
`Browser::launch(options)` remains available for existing `LaunchOptions` values.

Use `Browser::connect` for an existing browser, or `Connection::connect` for direct
CDP access. Both accept HTTP(S) discovery endpoints and WS(S) socket URLs.
See [Browser](rustcdp/src/browser.rs) and [LaunchOptions](rustcdp/src/launcher.rs) for connection,
profile, and process-cleanup contracts.

## Try the examples

The examples open a Chrome window and use local sample pages. Chrome flags
can follow `--` in each run command. Headless mode is optional:

```sh
cargo run -p rustcdp-examples --example checkout -- --headless=new
```

| Example | Demonstrates | Run |
| --- | --- | --- |
| [checkout](examples/checkout.rs) | Fill a checkout form with Unicode input, Tab, and Enter | `cargo run -p rustcdp-examples --example checkout` |
| [page_report](examples/page_report.rs) | Export a workshop report as a full-page PNG and PDF | `cargo run -p rustcdp-examples --example page_report` |
| [frame_walk](examples/frame_walk.rs) | Update inventory inside a nested dispatch-board iframe | `cargo run -p rustcdp-examples --example frame_walk` |
| [event_watch](examples/event_watch.rs) | Subscribe before navigation and collect console events | `cargo run -p rustcdp-examples --example event_watch` |
| [browser_contexts](examples/browser_contexts.rs) | Isolate and clear two shoppers' cookies; close contexts independently | `cargo run -p rustcdp-examples --example browser_contexts` |

The [examples package](examples/Cargo.toml) is separate, unpublished, and excluded
from the library package. Its dependencies are not needed by library consumers.
Each example awaits browser cleanup on success or failure. Report files go to
`rustcdp-report/` in the system temporary directory; set `RUSTCDP_OUTPUT` to override it.

## Choose an API

- [Browser](rustcdp/src/browser.rs) and [BrowserContext](rustcdp/src/browser/context.rs):
  launch/connect, isolated contexts, context-wide cookie reads/updates/clearing,
  and explicit cleanup. `new_context()` bypasses the parent browser's proxy;
  [proxy methods](rustcdp/src/browser/proxy.rs) configure a process or context proxy.
- [Page](rustcdp/src/page.rs): navigation, JavaScript evaluation, HTML, and cookies.
  [Selectors](rustcdp/src/page/selectors.rs) provides optional lookup, required lookup,
  and element-returning presence waits;
  [DOM](rustcdp/src/page/dom.rs) handles HTML reads and search transactions.
  [Element](rustcdp/src/page/element.rs) owns remote nodes;
  [input](rustcdp/src/page/input.rs) provides keyboard and Unicode input.
- [Frame](rustcdp/src/page/frame.rs): same-process frame traversal and scoped evaluation.
  `Page::frame(index)` attaches to an out-of-process frame belonging to that page.
- [Capture](rustcdp/src/page/capture.rs): `page.screenshot().full_page(true).capture().await`
  returns image bytes; `screenshot_with_options` exposes all CDP screenshot options.
  [PDF export](rustcdp/src/page/pdf.rs) uses
  `page.pdf().print_background(true).generate().await` to return PDF bytes;
  `print_to_pdf` accepts all CDP PDF options.
- [Connection](rustcdp/src/connection.rs) and [Session](rustcdp/src/session.rs): typed `execute`,
  JSON `execute_raw`, and event subscriptions for lower-level protocol access.

For ordinary interactions, wait for a matching element and use direct actions:

```rust
let input = page.wait_for_element("input[name=email]", std::time::Duration::from_secs(5)).await?;
input.insert_text("reader@example.com").await?;
page.require_element("button[type=submit]").await?.click().await?;
```

`wait_for_element` checks presence, not visibility or readiness. `require_element`
looks up once and reports `Error::ElementNotFound` if absent; `element_by_selector`
returns `Option<Element>` for optional matches. `click()` scrolls and sends CDP
mouse input; `click_with_options` accepts named settings through `page::ClickOptions`.
Actions are not automatically retried. `wait_for_selector_count_stable` and
`wait_for_xpath_count_stable` use `CountWaitOptions` to wait for a sampled nonzero
match count to stay unchanged. They return a timeout error if it never stabilizes
and add no delay after success. This does not check node identity or readiness.

The [generated `cdp` API](rustcdp/src/lib.rs) groups types, commands, and events by domain.
Commands implement `protocol::Command` with an associated response type. Commands
and named objects with schema-defined fields have `new(required_fields)` and
optional `with_*` setters, such as `CookieParam::new("name", "value").with_secure(true)`.
Optional fields use `Option`; all-optional types also implement `Default`.

Register `session.listen::<Event>()` before the action that emits the event, then
await `recv()` or handle events in a Tokio task. Typed subscriptions filter by
session and method before buffering; `connection.subscribe()` receives raw events
across sessions. Overflow reports `Error::Lagged`. Keep the appropriate
[domain guard](rustcdp/src/session/domain.rs) while listening for domain events.

## Ownership and cancellation

Call `close().await` or `detach().await` to observe cleanup results. Drop schedules
best-effort cleanup. Ordinary commands default to a 30-second deadline, including
queue admission; `with_command_timeout(None)` disables that caller deadline.
Resource-owning helpers retain their independent bounds and can continue after
caller cancellation. A timeout cannot undo a command Chrome already received.
Detailed contracts live on the owning methods linked above.

Closing an externally connected Browser disconnects without killing its process.
When no page exists, Browser creates a visible fallback in a fresh isolated context;
for externally connected browsers, dependent handles can retain it after Browser
Drop until explicit close or last Drop. Dropping a launched Browser terminates its
owned process. An ordinary BrowserContext disposes its context even if Page clones remain.
Keep a context owner alongside standalone Sessions or listeners.

Evaluation defaults to the page's main world, where its JavaScript globals are visible.
Choose `.world(EvaluationWorld::Isolated)` to share the DOM without sharing those globals.
Use `page.evaluate(source).json::<T>().await` for a Rust value, `.run().await` for
side effects, or `.remote().await` for an owned JavaScript reference. A remote value
schedules best-effort release on drop; `.release().await` observes cleanup errors.
Evaluation uses unique context identities and never retries user expressions.
See [evaluation](rustcdp/src/page/evaluation.rs) for cancellation and lifetime details.

Recorded transport failures share their typed cause through
`Error::ConnectionFailed(Arc<Error>)`; inspect the Arc or the standard error source
chain. See [Connection](rustcdp/src/connection.rs) and [Error](rustcdp/src/error.rs) for delivery rules.
Proxy readiness is not proof of proxy traversal or credential use; see
[ProxyAuthMode](rustcdp/src/proxy.rs) and [authentication](rustcdp/src/proxy/auth.rs).

## Diagnostics

Enable the optional `tracing` feature and install a subscriber in your application.
Debug events report method names, elapsed time, outcomes, shutdown, and process cleanup.
`CDP request finished` reports the exchange **before typed response decoding**:
`ok` does not imply the requested Rust response type decoded successfully.
Each exchange produces one completion event. Payloads, endpoint URLs, and remote
error text are not logged. The library does not install a subscriber.

## Development

The root is a Cargo workspace. The library lives in `rustcdp/`, and `examples/`
is an unpublished workspace member. Both share the root lockfile and `target/`
directory. Plain `cargo build` selects the library; `--workspace` selects both.

`rustcdp/build.rs` calls the generator in `rustcdp/codegen/` to produce bindings
from the pinned schemas in `rustcdp/protocol/`, without network access. Build the
local API reference with `cargo doc -p rustcdp`; the source links above identify
the owners of the detailed contracts.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p rustcdp --doc --all-features
RUSTDOCFLAGS="-D warnings" cargo doc -p rustcdp --no-deps --all-features
cargo package -p rustcdp
```

# rustcdp

An asynchronous Rust library for Chrome DevTools Protocol (CDP), with typed
commands and events, browser and context management, and page automation.

## Run

Requires Rust 1.99.1 or newer. The repository pins Rust 1.99.1 through rustup.

Install Chrome/Chromium or set `CHROME_PATH` to its executable, then:

```sh
cargo run --example checkout
cargo run --example page_report
```

The examples use headless Chrome and accept additional Chrome flags after `--`.
They render local sample pages and do not require an external website or account.
The library itself does not force headless mode. `LaunchOptions` controls arguments,
the executable, temporary-profile location/prefix, fixed profiles, and startup timeout.
Launching uses a port selected by Chrome and retries startup failures up to three
attempts. Connect to an existing fixed endpoint
with `Browser::connect` or `Connection::connect`.

## Browser and page

```rust,no_run
use rustcdp::{Browser, Result, launcher::LaunchOptions};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = Browser::launch(LaunchOptions {
        args: vec!["--headless=new".into()],
        ..Default::default()
    }).await?;

    let result = async {
        let page = browser.page();
        page.navigate_and_wait("https://example.org", Duration::from_secs(15)).await?;
        println!("{}", page.evaluate("document.title").await?);
        println!("{}", page.html().await?);
        Ok(())
    }.await;
    browser.close().await?;
    result
}
```

`evaluate` uses an isolated world and returns JSON. JavaScript exceptions become
errors. Use `evaluate_remote` for object IDs, `undefined`, BigInt, and non-finite
numbers. `navigate` starts navigation; `navigate_and_wait` matches its loader's
load event. `current_url` preserves fragments.

`browser.new_context().await?` creates an isolated `BrowserContext`. Its `page()`
exposes the same API. `cookies` and `set_cookies` operate in that context. A context
created without a proxy explicitly bypasses a proxy configured on the parent browser.

Page interaction includes `element_by_selector`, `wait_for_selector`,
`element_by_xpath`, `nodes_by_xpath`, `resolve_node`,
`wait_for_xpath`, `click_xpath`, `click_position`, `press_escape`, `go_back`, and
`flash_point`. `Element` supports scrolling, position, focus, text insertion, and CDP or
JavaScript clicks. Use `Page::insert_text` for Unicode text and `press_key` or
`press_key_with_modifiers` for keyboard events.
`WaitOptions` controls timeout, polling, count stability, and post-success delay.
`frame(index)` attaches to an out-of-process iframe owned by the current target;
it does not select another tab's iframe. For same-process frames, use `main_frame`,
`frames`, and `Frame::children`. Each `Frame` scopes evaluation and CSS/XPath lookup
to its own document and resolves the current execution context for each operation.
Reacquire frame handles after removal or a process swap.

`screenshot(&CaptureScreenshot)` and `print_to_pdf(&PrintToPdf)` return decoded
bytes. Both accept the generated CDP options, including their fluent setters.
PDF stream transfer remains available through `Session::execute`.

## Protocol access

```rust,no_run
use rustcdp::{Connection, Result, cdp::browser::commands::GetVersion};

async fn version() -> Result<()> {
    let connection = Connection::connect("http://127.0.0.1:9222").await?;
    let version = connection.execute(&GetVersion::default()).await;
    connection.close().await;
    println!("{}", version?.product);
    Ok(())
}
```

Each `cdp::<domain>` contains types and `commands`/`events` modules. Commands
implement `protocol::Command` with an associated response type. Required fields
are explicit struct fields; optional fields use `Option`. Types with only optional
fields implement `Default`. Identifier types are transparent newtypes.
Commands also provide `new(required_parameters)` and chainable `with_*` setters
for optional parameters, for example `Navigate::new(url).with_referrer(referrer)`.
Generated Rust documentation includes protocol descriptions and experimental or
deprecated status from the pinned schema.

`connection.attach(target_id)` returns a flattened `Session`. Both expose typed
`execute` and JSON `execute_raw`. HTTP, HTTPS, WS, and WSS endpoints are accepted.
The connection runs its reader automatically, with 256 simultaneous command slots
and a 100 MiB incoming-message limit. Commands default to a 30-second timeout.
Use `with_command_timeout(Some(duration))` on a connection, session, or page to
change its handle's deadline, or `None` to disable it.

Register typed events synchronously before the action that triggers them:

```rust,ignore
let mut loaded = page.session().listen::<rustcdp::cdp::page::events::LoadEventFired>();
page.navigate("https://example.org").await?;
loaded.recv().await?;
```

Typed subscriptions filter by session and method before buffering. Each holds 256
events and reports `Error::Lagged` on overflow. `connection.subscribe()` exposes
raw events across sessions. Use ordinary Tokio tasks to run event handlers.
Session detachment wakes pending operations and listeners, including descendants.

`Session::enable_dom()` and `enable_page()` return shared, reference-counted
`DomainGuard` values. The first guard enables the domain; closing the last disables
it. A `Page` owns its Page-domain guard. Call `guard.close().await` to await release.
Domain transitions run on the owning connection runtime so cancellation or a short
caller deadline cannot leave a half-completed enable operation.

## Proxies

Parse a `Proxy` from a URL; `Display` and `Debug` omit credentials. HTTP, HTTPS,
SOCKS4, and SOCKS5 endpoints are supported. Chrome Fetch authentication supports
HTTP proxy credentials, not SOCKS credentials.

```rust,ignore
use rustcdp::proxy::{Proxy, ProxyAuthMode};

let proxy: Proxy = proxy_url.parse()?;
let mut context = browser
    .new_context_with_proxy(proxy, ProxyAuthMode::OnDemand)
    .await?;
let result = async {
    context.page().navigate_and_wait(url, timeout).await?;
    context.wait_for_proxy_auth(timeout).await
}.await;
context.close().await?;
result?;
```

Use `Browser::launch_with_proxy` for a process-wide proxy. `Preflight` (the enum's
default) authenticates via `https://www.example.com/` before setup returns.
`OnDemand` authenticates on the first useful request. Both restrict credentials
to proxy challenges and propagate preflight failures. The worker serializes the
cold request burst, latches rejection, and disables Fetch after success. If cached
credentials later expire, retire the context rather than expecting automatic rearming.

Authentication events use a separate lossless, unbounded queue. Held requests and
held challenges are each bounded at 256;
excess work is aborted or canceled. Failed recovery can close the connection; see
`proxy_auth` module documentation.

`ProxyAuth` is also available as a low-level owned guard. Only one worker can own
a session. Normal browser/context shutdown awaits authentication cleanup; call
`detach_proxy_auth()` immediately before closing an unresponsive context to skip it.

## Ownership and development

Call `close().await` to confirm cleanup. Closing a context does not close its parent
browser. Closing an externally connected browser disconnects and closes any fallback
tab created by this library; it does not kill the external process. Temporary profiles
are removed after child reaping; supplied fixed profiles are retained. On Unix,
shutdown allows three seconds after TERM before forcing termination. Drop provides
best-effort fallbacks. A timeout cannot undo a command Chrome already received.

The pinned CDP schemas in `protocol/` generate Rust bindings through `build.rs`.
Generation runs locally without network access.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --examples
cargo doc --no-deps
```

## Examples

| Example | Demonstrates | Run |
| --- | --- | --- |
| [checkout](examples/checkout.rs) | Fill a local checkout form using CSS selectors, Unicode input, Tab and Enter | `cargo run --example checkout` |
| [page_report](examples/page_report.rs) | Export a workshop report as a full-page PNG and a PDF | `cargo run --example page_report` |
| [frame_walk](examples/frame_walk.rs) | Explore a dispatch board and update inventory inside a nested iframe | `cargo run --example frame_walk` |
| [event_watch](examples/event_watch.rs) | Subscribe before navigation and collect structured console events | `cargo run --example event_watch` |

The report files go to the system temporary directory under `rustcdp-report/`.
Set `RUSTCDP_OUTPUT` to choose another directory. Each example closes its browser
on success or failure.

## Diagnostics

Enable the optional `tracing` Cargo feature and configure a tracing subscriber in
your application to receive `rustcdp` events at debug level. Events report command
names, elapsed time, outcome categories, connection shutdown and process cleanup.
Command payloads, endpoint URLs and remote error messages are not logged by rustcdp.
The library does not install a subscriber or change application logging settings.

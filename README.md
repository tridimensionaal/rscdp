# rustcdp

An asynchronous Rust library for Chrome DevTools Protocol (CDP), with typed
commands and events, browser and context management, and page automation.

## Run

Requires Rust 1.99.1 or newer. The repository pins Rust 1.99.1 through rustup.

Install Chrome/Chromium or set `CHROME_PATH` to its executable, then:

```sh
cargo run --manifest-path examples/Cargo.toml --bin checkout
cargo run --manifest-path examples/Cargo.toml --bin page_report
```

The examples are a separate, unpublished Cargo package in `examples/`. Its
manifest lists the dependencies needed by these applications, including their
Tokio runtime. Examples are excluded from the library package.

The examples use headless Chrome and accept additional Chrome flags after `--`.
They render local sample pages and do not require an external website or account.
The library itself does not force headless mode. `LaunchOptions` controls arguments,
the executable, temporary-profile location/prefix, fixed profiles, and startup timeout.
Launching uses a port selected by Chrome and retries startup failures up to three
attempts. Connect to an existing fixed endpoint
with `Browser::connect` or `Connection::connect`.

## Browser and page

```rust
use rustcdp::{Browser, Page, Result, launcher::LaunchOptions};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    let mut browser = Browser::launch(LaunchOptions {
        args: vec!["--headless=new".into()],
        ..Default::default()
    }).await?;

    let result = read_page(browser.page()).await;
    let cleanup = browser.close().await;
    result?;
    cleanup
}

async fn read_page(page: &Page) -> Result<()> {
    page.navigate_and_wait("https://example.org", Duration::from_secs(15)).await?;
    println!("{}", page.evaluate("document.title").await?);
    println!("{}", page.html().await?);
    Ok(())
}
```

The operation runs in a named function so `?` can return an error while `main`
still awaits browser cleanup. If both fail, the operation error is returned.

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

```rust
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

```rust
use rustcdp::{Error, Page, Result, cdp::page::events::LoadEventFired};
use std::time::Duration;

async fn observe_load(page: &Page, url: &str) -> Result<()> {
    let mut loaded = page.session().listen::<LoadEventFired>();
    page.navigate(url).await?;
    tokio::time::timeout(Duration::from_secs(15), loaded.recv())
        .await
        .map_err(|_| Error::Timeout("waiting for a load event".into()))??;
    Ok(())
}
```

This example expects a full-document navigation. A fragment-only navigation may
not emit a load event and will time out. Use `navigate_and_wait` when you need to
wait for the specific navigation's completion rather than observe an event.

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

```rust
use rustcdp::{Browser, BrowserContext, Result, proxy::{Proxy, ProxyAuthMode}};
use std::time::Duration;

async fn browse_with_proxy(browser: &Browser, proxy: Proxy, url: &str) -> Result<()> {
    let mut context = browser.new_context_with_proxy(proxy, ProxyAuthMode::OnDemand).await?;
    let result = load_through_proxy(&context, url).await;
    let cleanup = context.close().await;
    result?;
    cleanup
}

async fn load_through_proxy(context: &BrowserContext, url: &str) -> Result<()> {
    let timeout = Duration::from_secs(15);
    context.page().navigate_and_wait(url, timeout).await?;
    context.wait_for_proxy_ready(timeout).await
}
```

Use `Browser::launch_with_proxy` for a process-wide proxy. `Preflight` (the enum's
default) checks readiness via `https://www.example.com/` before setup returns.
`OnDemand` handles challenges as requests occur. `wait_for_proxy_ready` succeeds
when a request completes without an observed authentication failure. Inspect
`proxy_auth_status()` to distinguish `Authenticated` from `Unchallenged`: cached
credentials, bypassed traffic, and proxies without authentication can all produce
an unchallenged response. Readiness does not prove that traffic traversed the proxy
or that the supplied credentials were used.

The worker restricts credentials to challenges matching the configured proxy's
scheme, host, and port. Mismatches are canceled without sending credentials.
It serializes the cold request burst and latches credential rejection.
Unchallenged traffic leaves Fetch enabled
for later challenges; verified authentication disables it. Network errors do not
count as credential rejection. If cached
credentials later expire, retire the context rather than expecting automatic rearming.

Authentication events use a separate queue of at most 4,096 compact events and
4 MiB of queued request-ID and origin text. Headers and bodies are not retained.
Overflow fails authentication and triggers recovery instead of silently losing
paused requests. Held requests and held challenges are each bounded at 256;
excess work is aborted or canceled. Failed recovery can close the connection; see
`proxy_auth` module documentation.

`ProxyAuth` is also available as a low-level owned guard. Only one worker can own
a session. Normal browser/context shutdown awaits authentication cleanup; call
`detach_proxy_auth()` immediately before closing an unresponsive context to skip it.
Authentication setup has a 30-second upper bound and observes cancellation.
Cleanup gives Fetch disable one second before recovery. These bounds still apply
when ordinary command timeouts are disabled.

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
cargo fmt --manifest-path examples/Cargo.toml --check
cargo clippy --manifest-path examples/Cargo.toml --all-targets -- -D warnings
cargo doc --no-deps
```

## Examples

| Example | Demonstrates | Run |
| --- | --- | --- |
| [checkout](examples/src/bin/checkout.rs) | Fill a local checkout form using CSS selectors, Unicode input, Tab and Enter | `cargo run --manifest-path examples/Cargo.toml --bin checkout` |
| [page_report](examples/src/bin/page_report.rs) | Export a workshop report as a full-page PNG and a PDF | `cargo run --manifest-path examples/Cargo.toml --bin page_report` |
| [frame_walk](examples/src/bin/frame_walk.rs) | Explore a dispatch board and update inventory inside a nested iframe | `cargo run --manifest-path examples/Cargo.toml --bin frame_walk` |
| [event_watch](examples/src/bin/event_watch.rs) | Subscribe before navigation and collect structured console events | `cargo run --manifest-path examples/Cargo.toml --bin event_watch` |

The report files go to the system temporary directory under `rustcdp-report/`.
Set `RUSTCDP_OUTPUT` to choose another directory. Each example closes its browser
on success or failure.

## Diagnostics

Enable the optional `tracing` Cargo feature and configure a tracing subscriber in
your application to receive `rustcdp` events at debug level. Events report command
names, elapsed time, outcome categories, connection shutdown and process cleanup.
Command payloads, endpoint URLs and remote error messages are not logged by rustcdp.
The library does not install a subscriber or change application logging settings.

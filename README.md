# rustcdp

An asynchronous Rust library for Chrome DevTools Protocol (CDP), with typed
commands and events, browser and context management, and page automation.

## Run

Install Chrome/Chromium or set `CHROME_PATH` to its executable, then:

```sh
cargo run --example basic
cargo run --example interact
```

Examples that launch a browser use headless Chrome and accept additional Chrome
flags after `--`. `version` and `session` connect to an existing debugging endpoint.
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

Page interaction includes `element_by_xpath`, `nodes_by_xpath`, `resolve_node`,
`wait_for_xpath`, `click_xpath`, `click_position`, `press_escape`, `go_back`, and
`flash_point`. `Element` supports scrolling, position, and CDP or JavaScript clicks.
`WaitOptions` controls timeout, polling, count stability, and post-success delay.
`frame(index)` attaches to an out-of-process iframe owned by the current target;
it does not select another tab's iframe or implement same-process frame switching.

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

```sh
# Set CDP_PROXY and optionally CDP_URL in your environment first.
cargo run --example context_proxy
```

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
| [basic](examples/basic.rs) | Navigation, JavaScript evaluation, HTML and isolated contexts | `cargo run --example basic` |
| [interact](examples/interact.rs) | XPath waits, DOM node resolution, clicks and keyboard input | `cargo run --example interact` |
| [launch](examples/launch.rs) | Owned Chrome process, typed version request and cleanup | `cargo run --example launch` |
| [version](examples/version.rs) | Connect to an existing browser and execute a typed command | `cargo run --example version -- http://127.0.0.1:9222` |
| [session](examples/session.rs) | Create a target, attach a session, receive events and detach | `cargo run --example session -- http://127.0.0.1:9222` |
| [context_proxy](examples/context_proxy.rs) | Per-context proxy with on-demand authentication | `cargo run --example context_proxy` |
| [proxy](examples/proxy.rs) | Low-level proxy authentication guard | `cargo run --example proxy` |

The proxy examples read `CDP_PROXY` and optionally `CDP_URL` from the environment.
The low-level `proxy` example requires a proxy URL containing credentials.

## Diagnostics

Enable the optional `tracing` Cargo feature and configure a tracing subscriber in
your application to receive `rustcdp` events at debug level. Events report command
names, elapsed time, outcome categories, connection shutdown and process cleanup.
Command payloads, endpoint URLs and remote error messages are not logged by rustcdp.
The library does not install a subscriber or change application logging settings.

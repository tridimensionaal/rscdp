# Examples

The examples use local [HTML pages](pages), with a localhost server where
cookies or HTTP requests are needed. No external site is required. They open a
Chrome window by default; append `-- --headless=new` for headless mode.

Run from the repository root or this directory:

```sh
cargo run -p rustcdp-examples --example checkout
```

Replace `checkout` with any example below:

| Example | Demonstrates |
| --- | --- |
| [navigation](navigation.rs) | Observe a click-triggered navigation and wait for application readiness |
| [connect](connect.rs) | Attach and disconnect a second connection without stopping the launched browser |
| [native_controls](native_controls.rs) | Save account preferences with native select and checkbox helpers |
| [structured_extract](structured_extract.rs) | Pass JSON arguments and extract a typed seed catalogue |
| [live_search](live_search.rs) | Reuse locators across changing search results and wait for DOM states |
| [parallel_pages](parallel_pages.rs) | Collect independent warehouse summaries concurrently |
| [local_storage](local_storage.rs) | Restore a saved draft across navigation and remove its storage key |
| [dialogs](dialogs.rs) | Answer a prompt and dismiss a blocking confirmation dialog |
| [popup_handoff](popup_handoff.rs) | Discover a popup and return its seat selection to the opener |
| [downloads](downloads.rs) | Correlate browser download events and read an exported CSV |
| [responsive_preview](responsive_preview.rs) | Inspect responsive layout and light/dark appearance |
| [geolocation](geolocation.rs) | Emulate coordinates and exercise granted and denied permissions |
| [offline_recovery](offline_recovery.rs) | Fail an uncached request offline, then restore connectivity and retry |
| [init_script](init_script.rs) | Set configuration before page scripts and remove the registration |
| [accessibility_tree](accessibility_tree.rs) | Inspect accessible roles, names and state, then repair an unnamed control |
| [checkout](checkout.rs) | Fill and submit a form with Unicode text and keyboard input |
| [page_report](page_report.rs) | Export a report to PNG and PDF |
| [frame_walk](frame_walk.rs) | Traverse nested frames and update inventory |
| [event_watch](event_watch.rs) | Subscribe to console events before navigation |
| [browser_contexts](browser_contexts.rs) | Isolate cookies and share them between tabs in one context |
| [response_body](response_body.rs) | Correlate network events and read a completed response body |
| [request_interception](request_interception.rs) | Continue requests, replace responses, and block analytics |
| [file_upload](file_upload.rs) | Select a CSV and read its local preview; no server upload |

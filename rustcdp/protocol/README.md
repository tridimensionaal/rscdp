# Pinned CDP schemas

Both JSON files match the upstream Chrome DevTools Protocol snapshot at
[`ebca9061ecc4c77b7d6b5b8f7b8afe19dc3ab9ed`](https://github.com/ChromeDevTools/devtools-protocol/commit/ebca9061ecc4c77b7d6b5b8f7b8afe19dc3ab9ed)
(2026-10-27, “Roll protocol to r1727723”) byte-for-byte.
The generator in `codegen/`, invoked by `build.rs`, produces Rust bindings without
network access. Update the snapshots deliberately when updating protocol support,
not on every build.

Immutable source files and SHA-256 hashes:

- [browser_protocol.json](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/ebca9061ecc4c77b7d6b5b8f7b8afe19dc3ab9ed/json/browser_protocol.json):
  `332d242771f522def6cab034fe90e4d65d87c31c20f3d8a642f54f04466ef642`
- [js_protocol.json](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/ebca9061ecc4c77b7d6b5b8f7b8afe19dc3ab9ed/json/js_protocol.json):
  `3716f86e2228f9414ffff43551249bc6a5783388c3fbdba5226f14e6941774f1`

Each domain exposes named types, `commands` and `events`. Commands implement
`protocol::Command`, linking a serializable request to its response type. Events
implement `protocol::Event`. Optional properties are omitted when `None`.
Identifiers use transparent newtypes. Cyclic inline object references are boxed;
arrays use `Vec` as their indirection. Unstructured objects and `any` retain JSON.

Generated types reflect this snapshot. Protocol enums are non-exhaustive and
preserve unknown strings in `Unrecognized(String)`, including when serialized
again. `as_str()` returns the wire value for both known and unknown variants.
Other incompatible response shapes can still fail typed decoding;
`Error::Decode` retains the received payload. Use `Connection::execute_raw` or
`Session::execute_raw` to work with untyped JSON responses.

Upstream: https://github.com/ChromeDevTools/devtools-protocol (BSD-3-Clause).

## Browser compatibility

The schema pin describes the generated Rust API, not a minimum Chrome version
or a guarantee that every browser implements every command. The high-level
helpers and local examples are exercised with Chrome for Testing 151.0.7922.34
on Linux. Newer and experimental commands depend on the running browser and its
feature configuration. Inspect `Browser.getVersion` and the browser's
`/json/protocol` endpoint when diagnosing compatibility; the protocol version
string alone does not identify a schema snapshot.

One generator exception preserves older event decoding:
`WebAudio.BaseAudioContext.renderQuantumSize` is optional in Rust because Chrome
151 omits it. The checked-in JSON remains an exact upstream copy. Other missing
required fields remain errors instead of silently receiving invented defaults.

This refresh removes deprecated generated APIs, including Network interception
commands (use Fetch instead), and changes some field types and constructors.
For example, `WebAuthn.Credential.signCount` is now `f64`, and
`DOM.Node.isAdRelated` was replaced by `adProvenance`. Also:

- `CSS.CSSContainerQuery.conditionText` is newly required; older payloads without
  it fail typed decoding.
- `SeekAnimations::new` now takes only the animation IDs; choose
  `with_current_time` or `with_current_times`. `EmulateNetworkConditionsByRule::new`
  no longer requires `offline`; use `with_offline` when needed.
- Removed APIs include Network accepted-encoding overrides and several Storage
  privacy APIs. `WebMCP.toolsRemoved` now carries `RemovedTool` values.
- Added optional fields can break exhaustive struct literals; removed enum
  variants can break named references despite unknown-string decoding support.

Review downstream imports, constructors and struct literals when upgrading.

## Updating the snapshot

1. Choose an exact upstream commit and replace both JSON files from that commit.
   Record its date and SHA-256 hashes above; do not fetch schemas from `build.rs`.
2. Review removed commands, changed required fields, enums and numeric types.
   Keep compatibility exceptions narrow, documented and covered by a regression.
3. Run formatting, Clippy, unit tests and doctests, build the API docs, then run
   the browser smoke checks and examples. Record the actual Chrome version;
   compilation alone does not establish runtime compatibility.
4. Commit the schemas, generator adjustments and migration notes together. A
   generated API change can break callers even if the high-level API is unchanged.

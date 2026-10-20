# Pinned CDP schemas

Both JSON files match the upstream Chrome DevTools Protocol snapshot at
[`07f7b8278cde19db07bed2abe72a0aa7c1656fb3`](https://github.com/ChromeDevTools/devtools-protocol/commit/07f7b8278cde19db07bed2abe72a0aa7c1656fb3)
(2026-03-10, “Roll protocol to r1596832”) byte-for-byte.
The generator in `codegen/`, invoked by `build.rs`, produces Rust bindings without
network access. Update the snapshots deliberately when updating protocol support,
not on every build.

Immutable source files and SHA-256 hashes:

- [browser_protocol.json](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/07f7b8278cde19db07bed2abe72a0aa7c1656fb3/json/browser_protocol.json):
  `641093559684b6cbdd5ac1887cb09b0af3ef51839c9b6f3277ed729051b20650`
- [js_protocol.json](https://raw.githubusercontent.com/ChromeDevTools/devtools-protocol/07f7b8278cde19db07bed2abe72a0aa7c1656fb3/json/js_protocol.json):
  `5a54a335617a0ff088c22f8d7a39ee7616ebdba3eb982ebf4e6b1869239e60f5`

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

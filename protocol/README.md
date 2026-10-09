# Pinned CDP schemas

These JSON files are pinned Chrome DevTools Protocol schema snapshots.
`build.rs` generates Rust bindings from them without network access. Update the
snapshots deliberately when updating protocol support, not on every build.

Each domain exposes named types, `commands` and `events`. Commands implement
`protocol::Command`, linking a serializable request to its response type. Events
implement `protocol::Event`. Optional properties are omitted when `None`.
Identifiers use transparent newtypes. Cyclic inline object references are boxed;
arrays use `Vec` as their indirection. Unstructured objects and `any` retain JSON.

Upstream: https://github.com/ChromeDevTools/devtools-protocol (BSD-3-Clause).

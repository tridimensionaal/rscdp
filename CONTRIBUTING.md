# Contributing

These instructions are for contributors changing the library or its examples.
Run commands from the repository root and choose checks relevant to your change.
For documentation-only changes, check the text, links, and any modified code snippets.

## Rust changes

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p rscdp
```

CI also compiles every example to catch API changes that break their code:

```sh
cargo build --locked -p rscdp-examples --examples
```

## Browser behavior

For changes to browser behavior, run the relevant integration tests in
`rscdp/tests/`. To run all browser-dependent tests:

```sh
cargo test -p rscdp -- --ignored --test-threads=1
```

These require Chrome/Chromium; set `CHROME_PATH` if it is not discoverable.

When changing an example or an API it uses, run the affected example and inspect
its behavior and output. See the [example instructions](examples/README.md).
CI compiles examples but does not execute them; compilation alone cannot check
browser behavior.

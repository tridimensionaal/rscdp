# Contributing

These instructions are for contributors changing the library or its examples.

The workspace contains the library in `rustcdp/` and examples in `examples/`.
Run these commands from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test -p rustcdp
```

## Browser checks

These checks require Chrome/Chromium; set `CHROME_PATH` if it is not discoverable.
Run the browser-dependent tests explicitly:

```sh
cargo test -p rustcdp -- --ignored --test-threads=1
```

To check every example, build them and run the smoke script (Linux, Python 3.11+):

```sh
cargo build -p rustcdp-examples --examples
python3 scripts/browser_smoke.py
```

[browser_smoke.py](scripts/browser_smoke.py) runs each compiled example headlessly in a
separate temporary working directory, checking that it succeeds without relying
on the repository's working directory. Each run has a 60-second timeout; on a
timeout, the script kills that run's process group, including Chrome.

Python's standard library handles process management, temporary directories, and
reading the example manifest; no Python packages are needed. This is development
tooling, not a requirement for using the library or running an individual example.

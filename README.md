# Bugsee SDK for Rust

Crash, native-fatal (signal / SEH), panic and handled-error reporting for Rust
applications, in the style of the Bugsee mobile SDKs. Reports are standard Bugsee
report bundles, so they ingest into the existing Bugsee backend and dashboard
unchanged.

> **Status: pre-release (0.1.0).** The API may still change and the crates are not
> yet published to crates.io. See [`PROGRESS.md`](./PROGRESS.md) for what is done
> and what is open.

## Quick start

```rust
use bugsee::{Bugsee, LaunchOptions};
use std::time::Duration;

fn main() {
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("<your_app_token>").app_version(bugsee::app_version!()),
    )
    .expect("bugsee launch");

    Bugsee::event("app_started");
    Bugsee::set_attribute("tier", "premium");

    // ... your application ...

    Bugsee::flush(Duration::from_secs(5));
}
```

Dropping the guard shuts the SDK down. Fatal crashes are recovered and sent on the
next launch, so give the process a data directory that survives restarts
(`LaunchOptions::data_dir` or `BUGSEE_DATA_DIR`).

## Crates

| Crate | Role |
|---|---|
| `bugsee` | Public facade, default HTTP transport, APM. Start here. |
| `bugsee-core` | Capture, persistence and export engine. |
| `bugsee-panic` | Panic observer and `catch_unwind` boundary guards. |
| `bugsee-native`, `bugsee-native-api` | Native fatal-crash handler and its backend contract. |
| `bugsee-reqwest` | `reqwest` network-capture middleware. |
| `bugsee-tracing`, `bugsee-log` | `tracing` layer and `log` adapter. |
| `bugsee-ffi` | C ABI for embedding in iOS, Android and other hosts. |

## Documentation

- [`DESIGN.md`](./DESIGN.md): architecture and decision log.
- [`PROGRESS.md`](./PROGRESS.md): current status.
- Wire format: [`bugsee/specs`](https://github.com/bugsee/specs) (`sdk/reporting/`).

## Development

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

MSRV is 1.88 and `Cargo.lock` is committed (CI builds with `--locked`).

## License

MIT. See [LICENSE](./LICENSE).

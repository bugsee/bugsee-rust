# Rust SDK ↔ worker integration harness

An end-to-end check that the **producer** (this SDK) and the **consumer** (the
`worker` repo's `crash/rust.py`) agree — the class of contract drift that unit
tests on either side structurally cannot catch.

What `rust_worker_e2e.py` does:

1. **Generates** a throwaway Cargo project that depends on this SDK by path.
2. **Builds it in release**, **twice** — once `panic = "abort"`, once
   `panic = "unwind"` — with the settings a real user needs for symbolication
   (`[profile.release] debug = 1` and, on Linux, `-C link-arg=-Wl,--build-id`).
   The two panic strategies take completely different paths through the SDK, so
   both must be covered.
3. **Crashes it for real**, three times:
   - an **aborting panic** → SIGABRT → recovery *correlates* signal + snapshot;
   - an **unwinding panic** (the Rust **default**) → no signal at all; the
     marker is retained because `Recorder::drop` sees `thread::panicking()`;
   - a genuine **SIGSEGV** (null-pointer write) → the native variant.
4. **Relaunches** the app so the SDK's next-launch recovery turns each marker
   into a report bundle, queued on disk (delivery points at an unreachable
   endpoint, so the bundle stays put).
5. **Extracts `crash.json`** from each queued `*.bundle.zip`.
6. **Feeds both to the real worker code** (`crash.rust.process_crash_report`) and
   asserts the outcomes.

The load-bearing assertion is step 7: the **`code_id` the SDK reports at runtime
must equal the identity `symbolic` reads from the built binary** — the same
value the worker uses to look symbols up. If those two ever diverge, symbol
upload silently resolves nothing, and *only* a test like this notices.

## Running

```sh
python3 tests/integration/rust_worker_e2e.py \
    --worker /Users/alexeykarimov/Projects/Bugsee/worker
```

Options:
- `--worker PATH` — the worker checkout (default: `../worker` next to this repo)
- `--keep` — keep the generated project + data dir for inspection
- `--skip-worker` — only produce the bundles (useful when the worker's Python
  environment isn't available)

## Requirements

- a Rust toolchain (`cargo`)
- Python 3 for the harness itself
- **for the worker half:** the worker's deps. You do not need to activate
  anything — if the interpreter you launch with cannot import `symbolic` /
  `typing_extensions`, the harness **re-executes itself** under the worker's
  `.venv` (announcing which interpreter it switched to) and shims the two things
  a non-3.14 environment lacks (`zipfile.ZIP_ZSTANDARD`, `bugsee_demangle`).
  When no venv exists it carries on and reports precisely what is missing; the
  `symbolic` cross-check then degrades to a clear skip rather than a failure —
  worth noticing, since that check is the load-bearing one.

Exit code is non-zero on any assertion failure, so it can gate CI.

---

# `deployment_e2e.py` — against a real deployment

`rust_worker_e2e.py` deliberately points delivery at `http://127.0.0.1:1/v2`, a
dead address, and calls the worker **in-process**. That is the right design for
a CI gate — no network, no credentials, fully deterministic — but it means the
harness has never exercised **transport, auth, the symbol-upload API, the
symbol-store lookup, or grouping**. Those only exist on a real server.

`deployment_e2e.py` covers that half. It drives the path an actual user walks:

```
build (debug info + build id)
  -> bugsee-cli debug-files upload --type rust
  -> crash against the deployment
  -> relaunch; next-launch recovery delivers the bundle
  -> repeat, so grouping has two events to merge
```

It asserts everything it *can* locally — the binary died on a real signal, and
`Bugsee::flush` reported the outbound queue **actually drained** (so a bundle
left sitting on disk is a failure, not a pass). It deliberately does **not**
assert on frames or grouping: those facts live in the deployment's database.
It ends by printing what to check and the `code_id` to match against, which you
read back with the Bugsee MCP tools or the dashboard.

```sh
export BUGSEE_APP_TOKEN=...                      # never pass as an argument
export BUGSEE_ENDPOINT=https://apidev.bugsee.com # BASE url, no /v2
python3 tests/integration/deployment_e2e.py
```

Options: `--runs N` (default 2; more than 1 is what makes grouping meaningful),
`--skip-upload` (to see what an unsymbolicated issue looks like), `--keep`.

Two things that will bite otherwise:

- **The app must be of type `rust`.** The appserver gained that type in
  `473fa430`; pointing this at an ios/android app ingests through the wrong
  branch and the result misleads rather than obviously breaking. At the time of
  writing **no Rust app exists on staging** — one has to be created first.
- **`BUGSEE_ENDPOINT` is the base url, not the `/v2` one.** `bugsee-cli`
  appends `/v2/apps/<token>/…` itself while the SDK wants the `/v2` url, and
  both read this same variable. The script takes the base, passes it through to
  the CLI, and appends `/v2` for the SDK; it rejects a `/v2` value rather than
  silently producing `/v2/v2/…`.

Requirements: a Rust toolchain, `bugsee-cli` (on `PATH`, at `BUGSEE_CLI`, or
built in a sibling checkout), and `symbolic` for the `code_id` readout — that
last one degrades to a warning rather than a failure.

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

# Bugsee Rust SDK — Progress

_Snapshot: 2026-08-07 · `main` @ `7309414` (pushed, nothing unpushed) · 137 tests, 0 failed · clippy `-D warnings` + fmt clean · MSRV 1.86 (`Cargo.lock` committed, CI `--locked`; no new **direct** deps — the tree gained one transitive crate, `mach2`, with the `crash-handler` 0.8 bump). Working tree clean._

> **Ingestion is verified end to end against a live deployment.** A real crash
> from a real binary routes, symbolicates against uploaded symbols, and groups
> across runs on `apidev.bugsee.com`. Getting there surfaced six defects across
> four repos — see [Verified against a live deployment](#recent-work--verified-against-a-live-deployment-and-the-six-defects-it-found).

A standalone, cross-platform crash + native-fatal + panic + handled-error reporter,
built in **Bugsee mobile-SDK style**, producing **backend-compatible report bundles**
that ingest into the existing appserver/viewer unchanged. Design + decision log live
in [`DESIGN.md`](./DESIGN.md); the wire contract is `bugsee/report-bundle-structure`.

## Crates (8)

| Crate | Role |
|---|---|
| `bugsee-core` | Pure-`std` capture/persistence/export engine; recovery; queue; signatures. One contained `unsafe` (`kill(pid,0)` liveness probe, unix). |
| `bugsee-panic` | Chained global panic observer + `catch_unwind` boundary guards. |
| `bugsee-native` | Native fatal-crash handler (signal/Mach) → async-signal-safe marker + frame capture + module map for next-launch recovery. |
| `bugsee` | Public facade + default `ureq` HTTP transport + APM. |
| `bugsee-reqwest` | `reqwest` network-capture middleware (sanitized). |
| `bugsee-tracing` | `tracing` `Layer`. |
| `bugsee-log` | `log` adapter. |
| `bugsee-ffi` | Panic-safe C ABI (staticlib/cdylib) for iOS/Android/host embedding. |

## Implementation progress

| Phase | Status | Notes |
|---|---|---|
| 0 — Harness | ✅ | Workspace, mock 3-step API server, bundle validator, subprocess crash runner, CI (build/test/clippy/fmt + pinned-MSRV job). |
| 1 — Ingestible bundle (core) | ✅ | Sliding-window capture (time/byte/event caps), length-prefixed part streams, envelope/manifest, Zstd method-93 ZIP, signatures. |
| 2 — Handled errors + caught panics | ✅ | `catch_unwind` guards, panic snapshot, error/cause chains. Follow-ups: `tracing` Layer + `log` adapter done. Subsystem quarantine still open. |
| 3 — Fatal + next-launch + correlation | ✅ (core) | Native handler, session generations + liveness markers, next-launch recovery, panic↔SIGABRT correlation, client-side native dedup signature from frame offsets. **New:** code-id-bearing module maps (natives are now symbolicatable) and uncaught-unwinding-panic reporting. Open: best-effort in-flight flush, full out-of-process minidump. |
| 4 — Integrations & APM | ✅ (core) | APM transactions/spans, sysinfo telemetry, reqwest network capture. **New:** the environment is actually populated — OS version, kernel, memory, disk, locale, and app identity (`package_id`/`version`/`build`) via `LaunchOptions::app_version`. Open: sessions / release-health. |
| 5 — Hardening | ✅ (core) | Durable retry queue (backoff, retry cap, blacklist), `before_send`/`before_breadcrumb`, event sampling. **New:** `on_report_dropped` — delivery failure is observable at last; `flush()` alone never could distinguish delivered from discarded. Open: general PII scrubber, rate limits. |
| 6 — Mobile/FFI | ✅ (core) | `bugsee-ffi` C ABI (`include/bugsee.h`). Open: actual iOS/Android target builds + Swift/Kotlin wrappers. |

## Recent work — the symbol pipeline, end to end (`f224188`, `3e2f9bf`, `51f4dc7`)

Closing the last gap between "a Rust crash arrives" and "a Rust crash is
readable". Landed as three steps across four repos, each validated by a
cross-repo harness that builds a real binary and feeds its real crash documents
to the real worker code.

- **Native crashes are symbolicatable (was a hard blocker).** The backend symbol
  store is keyed on a module's **code id** — Mach-O `LC_UUID` / GNU build-id, the
  same identity `symbolic` extracts from an uploaded symbol file — but the SDK
  reported only base/size/name, so **no upload could ever match, whatever the user
  did**. `bugsee-native` now captures the code id inside the walks it already
  performed (`LC_UUID` from the Mach-O load commands; the GNU build-id from
  `PT_NOTE` during the `dl_iterate_phdr` pass, with bounds-checked note
  iteration), and recovery emits `modules[]`
  (`base_addr`/`end_addr`/`code_id`/`filename`) + `frames[]`
  (`addr`/`module`/`reladdr`) in the worker's existing shape. Both older
  module-map formats still parse, so a marker written by a previous build still
  recovers after an upgrade.
- **Uncaught panics are reported.** The global hook only *captured*; reporting
  happened at a `catch_unwind` boundary — so an uncaught **unwinding** panic (the
  Rust default) unwound out of `main`, dropped the guard, shut the SDK down
  "cleanly", removed the liveness marker, and **was never reported**.
  `Recorder::drop` now keeps the marker when it observes `thread::panicking()`
  (the process is dying *from* this panic) so next-launch recovery reports it via
  `build_fatal_panic`. A panic on a **non-main** thread is reported inline from
  the hook instead — it kills only that thread, so there is no process death for
  recovery to observe, and no crash-time isolation concern either. It is recorded
  as a **non-fatal error** (counting it as a crash would corrupt crash-free-session
  rates), annotated with the thread name. Inline reporting is gated on a
  thread-local guard depth so a panic one of our boundaries will catch is never
  reported twice. Opt-in `report_panics_from_hook` extends inline reporting to
  main-thread panics for short-lived processes that may never restart.
- **Frame attribution fix.** The reported crash site was a panic-runtime frame,
  not user code — every panic unwinds through the same shims, so distinct bugs
  collapsed into one group. Two causes: the `__rustc[<hash>]::` shim namespace
  matched nothing, and `starts_with("bugsee")` hid the **user's** frames whenever
  their crate name began with `bugsee`. Both near-duplicate predicates replaced by
  one `is_internal_frame` in `bugsee-core` matching on the full first path segment.

  ```text
  before: panic at <alloc::boxed::Box<F,A> as ...>::call (boxed.rs:2220)
  after:  panic at bugsee_e2e_app::main (src/main.rs:25)
  ```
- **Self-describing crash documents — the `source_*` triple.** Every `crash.json`
  now carries `source_sdk` (`"rust"`), `source_platform` and `source_arch`.
  Routing previously depended solely on `request.json`'s `environment.sdk.type`,
  but the two documents **do not travel together**: on the worker's
  resymbolication path `crash.json` is fetched from S3 while `environment` comes
  from a separate `api.get_recording` call, so a missing environment left the
  crash unroutable and it fell through to the platform-guessing branches.
  `environment.sdk.type` remains the fallback. All three values come from one
  source of truth shared with the `environment` builder, so a document cannot
  contradict its own environment. `source_arch` deliberately reports the
  **symbol-pipeline** spelling (`aarch64` → `arm64`): the worker's
  `normalize_arch` only strips ISA suffixes, it does not translate between
  vocabularies, so the Rust spelling would never match its own symbols.
- **Supporting work in the other repos:** `worker` gained Windows **PDB** support
  (`symbolfiles/pdb.py`, MSF magic sniffing, `symbolic`-based debug-id
  identification) and Rust native frame symbolication against the symbol store;
  `bugsee-cli` gained PDB discovery + upload; the wire contract in
  `report-bundle-structure` documents the triple (`bundle/crash.md`,
  `platforms/rust.md`) including the load-bearing arch-normalization rule.
- **Cross-repo integration harness** (`tests/integration/rust_worker_e2e.py`):
  generates a Cargo project, builds it **twice** (`panic = "abort"` and
  `"unwind"`) with `debug = 1` + `-Wl,--build-id`, crashes it three ways, recovers,
  and feeds the real `crash.json` to the real `crash/rust.py`. Load-bearing
  assertion: the runtime `code_id` matches the one read back off the built binary,
  proving three-way identity agreement (SDK runtime ↔ CLI upload ↔ worker
  normalization).

## Recent work — backend ingestion wired end-to-end (`appserver` + `worker`, merged)

Cross-repo change landing the Rust ingestion contract (DESIGN §15, now closed).
**All six changes are merged to `origin/master` in both repos** — the ingestion
path is live, not just code-complete. Routing keys off **`crash.json`'s
`source_sdk`**, with `environment.sdk.type == "rust"` as the fallback for
documents that predate the field — required because Rust reports an OS-typed
`platform.type`, so a `macos` report would otherwise land in the worker's
Apple/Mach-O branch.

- **`rust` (this repo):** `Environment.sdk` now carries `type: "rust"`
  (`model/environment.rs::SDK_TYPE`) — it previously had **no** `type` field at
  all, so the backend had nothing to route on.
- **`worker`:** new `crash/rust.py` handling the managed variant (panic /
  handled error, `cause` chain) and the thin native variant (`signal{…}`,
  minidump when present); routed from `jobs/bundle.py`; `utils/platform.py`
  documents that the bare OS names must not be folded into `web`; per-OS entries
  added to `static/sdk_versions.json` (their absence logged an *error* on every
  Rust bundle). **24 tests** in `test/test_crash_rust.py` — including 7 that
  exercise the real `jobs/bundle.py` dispatch (mutation-checked: breaking the
  discriminator fails 5 of them).
- **`appserver`:** `rust` umbrella application type (per-session OS rides
  `platform.type`, modelled on `javascript`), `cfg.core.sdk.rust` flat version
  floor, `isSupportedSdkVersion` rust branch, MCP `application.list` enum, and
  `rust` added to the **symbol-storage routing** (`symbols/` + `symbols.*` jobs,
  and the reprocess switch) so Rust minidumps can actually be symbolicated.
- **Signature contract settled:** client signatures unprefixed, server appends
  `"s."`-prefixed; grouping `$in` over the flat array. Found + fixed a real
  integration defect while testing: the shared managed signature builder takes
  its location from the *deepest* `cause`, but Rust cause links carry no
  backtrace, so every handled error with a source chain produced no server
  signature and an "Unknown location" summary.

## Recent work — 4th adversarial pass (Opus-verified), fully resolved (`68823c8`, `82a1fed`)

A fourth review: **Fable** finders across 8 risk dimensions → **Opus** skeptics
adversarially verifying each finding (2 lenses, default-to-refute). 34 unique
findings → 25 confirmed, 5 contested, 4 refuted. **Outcome: all 25 confirmed +
all 5 contested + the deferred F1 are fixed and pushed (CI green); the 4 refuted
are non-bugs, left alone.** Fixes shipped in two commits — the confirmed 5 high +
9 medium (`68823c8`), then the 5 contested + F1 (`82a1fed`) — all validated
(fmt · clippy `-D warnings` · 104 tests · MSRV `--locked`; the Linux-only native
paths, cfg'd out on the macOS dev host, are exercised by the ubuntu CI runner and
were pre-verified by inspection against `crash-handler` 0.6.3 + `libc`).

- **Durable queue (high):** a forced `flush()` no longer burns the durable
  60-retry cap while offline — forced attempts are decoupled from the counter, so
  a still-deliverable crash bundle can't be deleted in ~1.3 s. Retry now **resumes
  at the presigned PUT** via a persisted `.endpoint` sidecar instead of re-POSTing
  `create_issue` (no duplicate issues / no `12003`-drop of an un-uploaded bundle).
- **PII (high):** failed-request network entries scrub the URL out of reqwest's
  error `Display` (was leaking full credential-bearing URLs); header redaction is
  unified onto the normalized param matcher (`signature`/`csrf`/`api_key` no
  longer leak).
- **Native (high/med):** the guaranteed crash marker is written **before** the
  non-async-signal-safe Linux unwinder (a re-fault loses only frames, not the
  report); the module map now records module **size** so recovery rejects
  out-of-module PCs (ASLR-stable native signatures); Linux fault address read
  through the real `siginfo_t` layout (was always `0x0`); arm64e return addresses
  PAC-stripped (`xpaci`).
- **Signatures (high):** `normalize_frame` strips the rustc `::h<16hex>` hash
  inside the real `"{sym} (file:line)"` frame → build-stable dedup/blacklist
  (previously the hash survived, changing the signature every recompile).
- **Robustness (med):** `process_report` reclaims its hard-linked report dir on
  every error path (no leak on disk-full); relaunch with `native_crash_capture=
  false` uninstalls the previously-installed handler; the HTTP transport uses one
  `ureq::Agent` with connect/read/write timeouts (a stalled connection no longer
  wedges the uploader thread); rotation/eviction/telemetry run on a wall-clock
  deadline (steady capture traffic can't starve the sliding window); manifest
  `files[]` `type` uses the shared `events`/`traces` base (the user/system split
  stays in the filename).
- **Refuted (not bugs):** FFI second-panic (already guarded), app_token in error
  strings (unreachable), recovered-env attribution (env is correct), cross-thread
  panic-snapshot overwrite (not a real race).

Two tests were passing *because of* bugs and were corrected: the FFI lifecycle
flush (the old offline flush "succeeded" by deleting the queued report) and the
`events.user`/`traces.user` manifest-type assertions.

**Follow-up — the 5 contested findings + F1 (all adjudicated real, now fixed):**
- **F5** — cap the `capture_error` `source()` walk at 32 links: a cyclic chain no
  longer hangs the caller and a pathologically deep chain no longer overflows the
  recursive `crash.json` serialization.
- **F18** — bound the report backlog (separate in-flight counter, drop-newest
  NON-crash reports over the cap, never crashes), restoring the module's stated
  RSS invariant under a report storm (`report_at` bypassed the capture counter).
- **F23** — the form/fragment redactor also splits pairs on `;` (some stacks use
  it as a separator), so `mode=full;session_token=…` redacts the token.
- **F24** — add `passphrase` and `pwd` to the sensitive stem list.
- **F28** — an empty liveness marker (pid write failed on a full disk / died in
  the O_EXCL-create→pid-write window) is recovered once it is older than a 30 s
  grace window, instead of being skipped and its generation leaked forever.
- **F1** — the FFI `bugsee_flush` returns a distinct `Timeout` (`BUGSEE_TIMEOUT`
  in `bugsee.h`) when the SDK is launched but the queue didn't drain, vs
  `NotLaunched` only when it was never launched (added `Bugsee::is_launched()`).

## Recent work — three adversarial-review passes + native dedup (`524198f`)

Multi-agent reviews across concurrency, recovery/durability, wire/PII, native/FFI,
and build/MSRV. Fixed (all validated):

- **Concurrency:** drop the old `Recorder` outside the global lock (`launch_with`/`stop`);
  `Recorder::drop` ends the session only on a genuine clean join (detached ⇒ keep marker);
  `transport::deliver` no longer holds the session lock across the network call.
- **Recovery/durability:** pid-liveness + empty-marker skip (don't destroy a live shared-dir
  peer / emit a phantom crash); per-pending `catch_unwind` + attempt-cap; `gc_orphans` for
  marker-less parts and orphaned queue sidecars; `build_native` always emits `signal`;
  aborting-panic `domain` is `null`; blacklist writes append-only.
- **PII/wire:** `is_sensitive_param` normalized-substring (was exact-match — leaked
  `accessToken`/`authorization`/`session`/`cookie`/…); recursive JSON body key-redaction +
  drop unknown content types; URL-fragment credential redaction; `error_entry` body/reason pairing.
- **Native safety:** Apple `on_crash` decodes `EXC_SOFT_SIGNAL`/`EXC_BAD_ACCESS`;
  `mach_to_signal` unknown → UNKNOWN; `StackBuf::dec` `i64::MIN`-safe; `report_caught`
  blocking-lock snapshot cleanup; `0x00`-delimited signature hashing.
- **Native crash dedup signature (new capability):** crash handler captures the crashing
  thread's frame PCs async-signal-safely (Apple: `thread_get_state` + fault-safe
  `mach_vm_read_overwrite` FP-walk; Linux/Android: `backtrace::trace_unsynchronized`);
  module map snapshotted at install (`crash.modules`); recovery computes
  `native_signature` from ASLR-invariant `pc − base` offsets → local blacklist can now
  suppress a native crash-on-launch loop. Proven by a real macOS SIGSEGV subprocess test.
- **Build:** MSRV → 1.86 (icu/idna floor), `Cargo.lock` committed, CI msrv `--locked`.

## Recent work — verified against a live deployment, and the six defects it found

The first end-to-end run against `apidev.bugsee.com` (build → upload symbols →
crash → recover → deliver → read the issue back). It passed, but only after six
defects came out — every one of them invisible to the existing tests, because
each sat in a seam between two repos.

**What the run proves.** Issue `6a747da3…`: routed as a Rust native crash
(SIGSEGV), frames resolved, and both runs grouped onto one issue
(`events_count: 2`). The resolution is not circumstantial — the SDK emits
addresses only (`{addr, module, reladdr}`, no name/file/line key exists in the
payload), yet the summary reads `core::ptr::write_volatile (mod.rs:2171)`, so
the symbol and line can only have come from server-side symbolication against
the dSYM we uploaded. The canonicalization chain held on real artifacts:
`bugsee-cli` declared `ab98e80b-042c-…` (lower-dashed), the worker stores dSYM
ids upper-dashed, the SDK reports dashless — three spellings reconciled on a
byte-exact Mongo index.

The defects, in the order they blocked things:

1. **Every Rust report was silently discarded** (`appserver`, merged). The issue
   service summed `(env.platform.jailbreak && 1)`; in JS `undefined && 1` is
   `undefined` and `0 + undefined` is `NaN`, which fails document validation and
   throws the report away. iOS/Android never tripped it because they always send
   the key. **Not Rust-specific** — the browser SDK omits it too. Fixed on both
   sides: the appserver tolerates absence, and the SDK now emits
   `platform.jailbreak: false` (`984c14e`).
2. **The SDK could not see the rejection** (`66f0135`). The API answers
   application errors with `ok:false` and **HTTP 200**, but the transport only
   inspected the envelope for 4xx/5xx. The error degraded to "no endpoint in
   response", the report was dropped, and the server's explanation was thrown
   away. The known codes `12003`/`12004` were reachable only on a 4xx too — so
   `12004` skipped the signature blacklist entirely.
3. **Nothing could observe a dropped report** (`1b2a456`). `flush()` returns
   "the queue is empty", and the queue drains on success *and* on permanent
   failure alike — so it returned `true` while the server accepted nothing.
   `LaunchOptions::on_report_dropped` now reports `DropReason`, and the e2e
   harness fails on a drop instead of reporting a clean run.
4. **A fully symbolicated stack rendered empty** (`worker@daf9e0b`,
   `viewer@16490`). The SDK sends the crashing thread as a bare top-level
   `frames[]` — a symbolication input, not a display shape — and no consumer
   reads that. The worker now reconstructs `threads: [{crashed: true, …}]`.
5. **Frame data was an opaque string** (`worker@daf9e0b`). Structured parsing was
   skipped for *demangled* symbols, which is every Rust frame. Real document:
   1 of 6 frames had structured data, now 5 of 6.
6. **The user's own code was missing from the stack** (`worker@daf9e0b`).
   `symcache.lookup` returns the whole inline chain and the symbolicator kept
   only `results[0]` — the innermost inlined callee — so
   `bugsee_deploy_e2e::main (main.rs:25)`, inlined at the crash address, appeared
   nowhere. **73%** of addresses in a Rust binary carry a chain (0.05% in a real
   iOS dSYM). The shared contract now returns chains; Rust expands them, Apple
   deliberately does not (see below).

**The environment was also near-empty** and is now populated: OS version, kernel,
memory, disk and locale (`f69a041`, `89e28fe`), plus app identity via
`package_id`/`version`/`build` and `LaunchOptions::app_version` (`205aa72`). Three
fields the SDK sent were being dropped on ingest — `app.name`/`app.path` were
Rust-only inventions (the other SDKs use `package_id`), while `hardware.arch` was
right and the schema was missing it.

## Open items

### Earlier — CLI ergonomics, cross-SDK adoption, canonicalization

- **CLI ergonomics** — `bugsee-cli` gained `--type rust` with per-host discovery
  (dSYM / ELF-with-build-id / PDB), preflight advice when the host project lacks
  `debug = 1` / `split-debuginfo = "packed"` / `-Wl,--build-id`, and a
  `nothing-found` diagnostic. Smoke-testing a real `cargo build --release` caught
  the case that mattered: a correctly-configured macOS build puts a **symlinked**
  `.dSYM` at the profile root, and the first implementation told the user to fix
  settings that were already correct.
- **Cross-SDK `source_*` adoption** — Android merged (`33a3d6d94`, in
  `origin/master`); iOS on `origin/nextgen` (`c0d4bfbd7`, not yet in
  `master`/`release`); the contract in `report-bundle-structure` documents the
  **mirror rule** — each `source_*` key is a *copy* of an environment value from
  the same source of truth, never an independent probe, and a producer omits any
  key it has no counterpart for. That is why only Rust emits `source_arch`.
  JavaScript is deferred (parallel work in flight).
- **Symbol UUID canonicalization (backend)** — the appserver now canonicalizes
  `symbols.images[].uuid` at the DAO boundary plus a migration for stored
  records, and it is live on staging. That field is a plain String with a plain
  index and no collation, so comparisons are byte-exact; three producers wrote
  three spellings of one identity and lookups silently found nothing.
  **The Rust SDK needs no change** — `bugsee-native` already emits `LC_UUID` as
  lowercase dashless hex (`hex_lower`), which *is* the canonical form. The
  worker's Python port landed in `worker@cec8390`, driven byte-for-byte from the
  appserver's shared vector table.

### Next up — actionable now

1. **JavaScript `source_*` adoption** — deferred, not dropped: there is parallel
   work in flight on that SDK. Findings are held for whoever owns it. Note the
   browser SDK is also the remaining producer exposed to the
   `platform.jailbreak` class of bug — the appserver fix protects it now, but its
   `EnvironmentPlatform` is still `{type, os, locale}`.
2. **Two environment fields remain unpopulated**, both needing a decision rather
   than effort. `utc_offset`: the `time` crate refuses a local offset in a
   multithreaded process (an SDK always is one), so it needs `localtime_r`/Win32
   and a small `unsafe` — which belongs in the host crate, not core.
   `hardware.model`: no `sysinfo` API, per-platform work, and on desktop it
   yields a mainboard string worth little until Phase 6 puts this SDK on mobile.
   Windows `locale` is likewise unset — `GetUserDefaultLocaleName` needs a
   binding or a crate, and this SDK still takes no new dependencies.

### Decided against

- **Expanding inline frames on the Apple path.** The chain plumbing is shared and
  in place (`worker@daf9e0b`), but Apple deliberately keeps selecting
  `chain[0]`; the reasoning is recorded at that call site. **The decision is
  about value, not risk** — the risk has a known fix, below.

  *Value:* 0.05% of addresses in a real iOS dSYM carry a chain, and on the one
  real iOS crash fixture **none** of the 3 app frames does (22 of its 25 frames
  are system libraries, whose `.symcache` files we have no sample of).

  *Risk, and its fix:* the signature takes its location from one frame chosen by
  two index-sensitive steps — a `frames[0:-2]` truncation and `skipFrames` from
  per-app merging rules. Both are fixed by counting **physical** call frames:
  an inline entry consumes no skip budget and is skipped with the parent it
  belongs to (expanded entries already carry `inlined: true`), with a sibling
  `countInlinedFrames` option for anyone who wants the opposite. Both sites
  should go through one shared `physical_frames()` accessor — otherwise the rule
  is an implicit invariant that nothing enforces and a future positional
  heuristic would silently break.

  *Why it is not a small change:* `mergingRules` is **client-supplied**, not a
  backend setting — the appserver has none, it rides the crash document from the
  SDK (`BugseeMergingRulesSkipFramesKey` on iOS, `ExceptionSignature.java` on
  Android). A new option is therefore public API in three SDKs, each needing a
  release, and inert until expansion ships. `skipFrames` is also applied
  **client-side** for Android's own signature, so skip semantics must stay
  consistent across both sides or the two signatures diverge for one crash.

  Revisit only if a real iOS case shows a hidden user frame.

### Deferred review findings — need a decision before building

1. ~~**Native `signatures` wire placement (confirm).**~~ **RESOLVED** — the backend keeps
   client signatures and *appends* its own `"s."`-prefixed ones; grouping is an `$in` over
   the flat array, so both are live keys. Sending `[<client sig>]` unprefixed is correct
   (same convention as the Android NDK path). See DESIGN §15.
2. **Cross-thread panic↔crash tid match** (DESIGN §195). Correlation is time-window only;
   no thread-id check. Practically mitigated (reliable snapshot cleanup + strict window),
   but a full fix needs a kernel tid on Linux — and Apple's mach-port vs pthread-id
   namespaces make a portable version fragile.
3. **Cross-instance queue/recovery exclusivity.** Multiple processes sharing a data dir can
   double-deliver a recovered/queued report. A lock-file fix risks a *permanent block* if
   the holder dies, so it needs real design (the server already dedups).
4. **`crash-handler` crate resolves a private Apple libproc symbol** (App-Store sensitivity).
   Third-party; track upstream / consider a fork or a different backend.
5. **Dependency-spec dedup** (LOW): centralize `zip`/`backtrace` in `[workspace.dependencies]`.

### Architecture to revisit — crash-handling backend

We are fully in-process: marker on disk, next-launch recovery. That matches the documented
in-process pattern (the reference implementations' in-process backends write to disk and
send on the next run), and the hard half — recovery, panic↔signal correlation, client-side
dedup — already works on two platforms.

But **in-process stack-overflow handling cannot be made reliable**. The guard page is already
consumed, so the handler runs in whatever few hundred bytes remain; our `StackBuf` is 160
bytes and heap-free, which suits it, but that is not the same as robust. Windows compounds it:
the top-level exception filter can be overwritten by other components, a second fault
re-enters the handler, and module enumeration must avoid the loader lock. The established
reporters all state that writing a minidump from inside the faulting process is unsafe, which
is why they offer an out-of-process option everywhere.

**Direction:** build-time backend selection, as the reference implementations do — in-process
(today's default, no extra binary) or out-of-process (a monitor process, the only way stack
overflow and heap corruption become genuinely reliable). It has to be a *build-time choice*
rather than a change of default, because out-of-process means shipping and launching a second
executable, and we are a library — we cannot impose that on a host's packaging.

**Do not block the Windows work on this.** Windows native capture is a total gap today (the
handler is a no-op stub), and every piece of it — marker format, the PE module map with a
debug-id `code_id`, next-launch recovery — is backend-independent. Revisit alongside the
Phase 3 out-of-process minidump, which is the same machinery. `minidumper` /
`minidumper-child` (same authors as `crash-handler`) already implement the monitor pattern in
Rust and are worth evaluating before building one.

### Remaining phase work

- Phase 2: subsystem quarantine (auto-disable a repeatedly-faulting integration).
- Phase 3: best-effort in-flight flush at crash; full out-of-process minidump (desktop) — see the backend note below, which is the same machinery.
- Phase 4: sessions / release-health.
- Phase 5: general PII scrubber, rate limits.
- Phase 6: iOS/Android target builds; Swift/Kotlin language wrappers.
- CI: cross-platform matrix (currently ubuntu-only); loom/miri/fuzz jobs.

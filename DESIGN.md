# Bugsee for Rust — Design

**Status:** Approved design (brainstorm-validated). Implementation in progress.
**Date:** 2026-07-25
**Scope:** A standalone, cross-platform Rust SDK — a **crash + native-fatal + panic + handled-error reporter** built in the architectural style of the Bugsee mobile SDKs, producing **backend-compatible report bundles** that ingest into the existing Bugsee appserver/viewer unchanged.

> **Layout amendment (2026-08-08):** the workspace is evolving into a **shared embeddable engine** (basement for most Bugsee SDKs) with pluggable platform capabilities, a C host vtable, and private console backends. See [`DESIGN_PLATFORM.md`](./DESIGN_PLATFORM.md) — it amends product-shape and I/O-boundary decisions below; wire format and ingestion are unchanged.
>
> **Capture amendment (2026-08-08):** Android-style capture actors, protobuf on-disk parts, C-reachable providers, and snapshot dedup (`DedupStore`: FS-as-DB default, optional `dedup-redb`, XXH3-128). See [`DESIGN_CAPTURE.md`](./DESIGN_CAPTURE.md). This amends decision 13 for **capture part** encoding (wire export remains JSON bundles).

---

## 1. Understanding Summary

- **What:** *Bugsee for Rust* — core deliverable is a crash/native-fatal/panic/handled-error reporter, built the Bugsee way, emitting the same report-bundle wire format the mobile SDKs upload.
- **Targets:** Linux, Windows, macOS, Android + iOS (via FFI). Tier-1 = desktop/server; mobile reuses the same core.
- **Capture channels:** logs / events / custom-data, network, system/process telemetry, breadcrumbs + contexts + scopes. **No UI/video.**
- **Error channel (full):** explicit `capture_error`/`capture_message`; `std::error::Error`/`anyhow`/`eyre`/`thiserror` source-chain + backtrace; `tracing`/`log` integration; `Result` ergonomics.
- **Surrounding features (v1):** offline persistence + retry + next-launch upload; before-send / PII-scrub / sampling / fingerprinting; sessions / release-health / perf spans (APM).
- **Export:** flat-ZIP of per-type JSON (`{"version":2,"events":[…]}`) + `manifest.json` / `request.json` / `.apptoken`; 3-step API (`sessions` → `issues` → presigned `PUT`).
- **Why:** first-class Bugsee reporting for Rust apps with the same product experience as mobile.

## 2. Assumptions

1. **Android is the primary architectural reference** (the wire contract is extracted from Android SDK 7.0.0; iOS inventories are still placeholders). Port Android's manager/module decomposition + public API naming, adapted to idiomatic Rust.
2. **Native crash capture leans on the proven Rust crate ecosystem** (`crash-handler` + `minidump-writer` / `minidumper`), wrapped Bugsee-style — not hand-rolled signal/Mach/SEH code. Coexistence/chaining with any host handlers is mandatory.
3. **Distribution:** internal crate first (private, pre-1.0, API may churn). crates.io publication is a later explicit decision.
4. **Runtime baseline:** `std` required; async/`tokio` **optional** (feature-gated). The core capture/persistence path is a plain `std` background thread, crash-survivable, no forced runtime.
5. **Rollout is phased**, tier-1 desktop/server, tier-2 mobile-via-FFI.
6. **Scope guard (YAGNI):** no UI/feedback, no video/view-hierarchy/screenshots — ever, for this crate.

## 3. Non-Functional Requirements

- **Performance:** near-zero happy-path cost — producers only enqueue (a move + at most one small alloc); serialization happens on the worker; one `catch_unwind` frame per FFI boundary; no stack capture on the happy path.
- **Scale/throughput:** non-blocking capture (depth-capped channel, drop-newest under pressure); one background worker owns drain + persistence; lock-light hot path; volume bounded by sampling + rate-limits + body caps.
- **Privacy/security:** default network sanitizer (URLs/headers/body), 20 KiB body cap, `before_send`/`before_breadcrumb` hooks, TLS to `api.bugsee.com`; no full-memory dumps by default (minidump opt-in).
- **Reliability:** durable offline queue, next-launch fatal upload, retry caps (export 15 / upload 60), signature dedup, crash-recursion guard, never crash the host.
- **Maintenance:** Bugsee-owned internal crate; Android-parity naming; feature-gated integrations to keep deps opt-in.

## 4. Decision Log

| # | Decision | Alternatives | Why |
|---|---|---|---|
| 1 | Fully standalone product | Embed in Bugsee / standalone core + Bugsee backend | Cleaner product story; Bugsee is default backend, not a coupling |
| 2 | All platforms; desktop/server tier-1 | Mobile-first | Standalone core simplest to build/test on desktop; mobile reuses it |
| 3 | Full error channel (explicit + ecosystem + tracing/log + `Result` ergonomics) | Explicit-API only | Differentiator vs. a plain crash reporter |
| 4 | Full v1 feature set + port mobile-SDK principles | Minimal capture core | Parity with mobile SDK APIs/mechanics |
| 5 | Capture logs/net/sys only; no UI/video | Include desktop-GUI capture | UI/video meaningless for headless Rust; YAGNI |
| 6 | Backend-compatible bundle | New Rust-native format / pluggable-later | Zero server changes; reports land in existing product |
| 7 | Android-primary reference | iOS-primary | Wire contract is Android 7.0.0; iOS still placeholder |
| 8 | Native crashes via proven crates | Build own / defer | Fastest, battle-tested, coexistence handled |
| 9 | Internal crate first, pre-1.0 | Public crates.io / vendored | Freedom to iterate before semver |
| 10 | Hybrid caps, disk-backed capture window | Faithful mobile time-window / in-memory ring | Crash-survivable context + adapts to server throughput |
| 11 | Rust signature = two feeds, mirror Bugsee | Frame-only / location-only | Best grouping fidelity; not byte-identical across platforms (never was) |
| 12 | Layered workspace, `std` core | Single crate / async-first | Most modular, no forced runtime, reusable core |
| 13 | JSON on disk (codec-flag escape hatch) | msgpack/binary on disk | Parse-free export; overhead off the caller's path; upgradeable if profiled |
| 14 | GPU under `hardware.gpu` | `platform.gpu` / both | Android-compatible, zero viewer change |
| 15 | Snapshot = pin bounds + hard-link current parts; filter at export | Seal/rotate on snapshot; copy; move | No churn, no data duplication, refcount-safe |
| 16 | In-memory queue + best-effort crash flush | mmap-ring ingress / hybrid | Simple, fast producers; mmap-ring noted as future max-durability upgrade |
| 17 | Red-green TDD; E2E test per flow | Test-after | Executable spec per flow; contract-conformance gated in CI |

---

## 5. Architecture — crate topology

A Cargo workspace. Integrations are **separate crates** re-exported by an umbrella `bugsee` crate behind features. Dependencies point **down only** (no cycles).

```
                    ┌────────────────┐
   user / other  ─▶ │     bugsee     │  umbrella facade + typed LaunchOptions
   Bugsee SDKs      │  (re-exports)  │  features: panic, tracing, log, reqwest, native, ffi, async, gpu
                    └───────┬────────┘
        ┌───────────┬───────┼─────────┬───────────┬──────────┐
        ▼           ▼       ▼         ▼           ▼          ▼
 bugsee-panic  bugsee-  bugsee-  bugsee-     bugsee-     bugsee-ffi
 (observer +   tracing   log     reqwest     native      (C ABI)
  catch_unwind)(Layer)  (adapter)(middleware)(crash-handler/
        │           │       │         │       minidump)      │
        └───────────┴───────┴────┬────┴────────────┴─────────┘
                                 ▼
                        ┌──────────────────┐
                        │   bugsee-core    │  pure std, no host-integration deps
                        └──────────────────┘
   event model · scopes/contexts/breadcrumbs · disk-backed capture window + part
   rotation · export→JSON · bundle ZIP (zstd-93) · signature · offline queue + retry
```

**Rules:** `bugsee-core` builds with zero default features, no async. `unsafe` concentrates in `bugsee-native` (signals/FFI) and `bugsee-ffi`; `bugsee-core` declares `#![forbid(unsafe_op_in_unsafe_fn)]` and holds exactly **one** small, contained `unsafe` — a `kill(pid, 0)` liveness probe (unix) used by recovery to avoid resurrecting a still-running shared-data-dir peer's session as a "crash". Its persistence uses only safe buffered-append `std::fs` I/O (the earlier "gated core mmap module" was never built). Each integration crate pulls exactly one ecosystem dep. **Edition 2021, MSRV 1.86 (CI-gated, verified on a pinned `1.86.0` toolchain against the committed `Cargo.lock`).** (The transitive dep graph forces this floor — `icu_*`/`idna_adapter` pulled in via `reqwest`→`url` require 1.86; a pinned-`1.85.0` build fails to compile them. The earlier "1.85"/"~1.75" figures were aspirational and untrue.)

## 6. Public API surface

Global singleton (mobile parity); `launch` returns a guard whose `Drop` flushes.

```rust
// Lifecycle
let _guard = Bugsee::launch("APP_TOKEN")?;
let _guard = Bugsee::launch_with(LaunchOptions::new("APP_TOKEN")
    .max_window(Duration::from_secs(60)).max_events(10_000).max_bytes(8 << 20)
    .sample_rate(1.0).native_crash_capture(true).system_telemetry(true)
    .before_send(|r: &mut ReportMeta| { r.severity = Severity::High; true })
    .before_breadcrumb(|c| Some(c)))?;
// Real builder set (see `crates/bugsee/src/options.rs`): data_dir, max_window,
// max_bytes, max_events, endpoint, with_transport, before_send,
// before_breadcrumb, sample_rate, native_crash_capture, system_telemetry,
// report_panics_from_hook (opt-in immediate panic reporting — see §9).
// (`capture_logs` / `network_default_sanitizer` / a dedicated `on_report` are
// not part of the current surface — capture toggles and a network sanitizer
// hook are future work; report mutation goes through `before_send`.)
Bugsee::stop(); Bugsee::pause(); Bugsee::resume(); Bugsee::is_active();

// Timeline
Bugsee::event("checkout_started");
Bugsee::event_with("promo", params!{ "code" => "SAVE10" });
Bugsee::trace("cart_total", 129.95);
Bugsee::log(LogLevel::Info, "screen shown");

// Error / crash reporting (core)
Bugsee::capture_error(&err);              // walks source() chain + backtrace
Bugsee::capture_message(Level::Error, "invariant broken");
result.capture()?;                        // ResultExt: report-and-propagate

// Identity & report-level attributes
Bugsee::set_email("user@example.com");    // → request.json.email
Bugsee::set_attribute("userTier", "premium"); // → manifest.attrs
Bugsee::clear_attribute("userTier"); Bugsee::clear_all_attributes();

// Manual reports
Bugsee::upload();                         // immediate, source.type = code_upload
Bugsee::upload_with(UploadOptions{ summary, description, severity, labels });
let mut r = Bugsee::create_report();      // deferred: snapshot now (pin bounds + hard-link)
r.set_summary("Checkout stuck"); r.set_severity(Severity::High);
r.add_attachment(bytes, "resp.txt", "text/plain"); r.upload();  // drop = discard
Bugsee::flush(Duration::from_secs(5));
```

**Manual entry points (mobile parity, minus UI):** `upload_with` (immediate), `create_report`+`report.upload()` (deferred), `capture_error`/`capture_message` (handled `logException`), — `showReportDialog` dropped. Report-level attributes are mutated through the `before_send(&mut ReportMeta) -> bool` hook (return `false` to drop) — the same metadata shape the imperative `create_report` path builds.

**Dropped from mobile:** blackout/secure-view, video/screenshot, view-tree, feedback/chat, gestures.

## 7. Data model & capture channels

```rust
enum CaptureEntry {
    Log(LogEntry),            // → log.json          {level:1-5, source, tag?, message}
    Event(EventEntry),        // → events.user.json  {name, params?}
    SystemEvent(SystemEvent), // → events.system.json{name, params?}  (process/thread lifecycle)
    Breadcrumb(Breadcrumb),   // → breadcrumbs.json  {type, category, level, message?, data?}
    Network(NetworkEntry),    // → network.json      (multi-stage, shared id)
    Trace(TraceEntry),        // → traces.{user|system}.json {name, value}
}
struct Transaction { trace_id, name, operation, status, start_ms, end_ms, duration_ns, spans } // → performance.json
```

Enums mirror the wire exactly: `LogLevel` Error=1…Verbose=5; `LogSource` StdOut=1/StdErr=2/PlatformLog=3/Bugsee=4/Custom=98/Internal=99; breadcrumb `level` debug/info/warning/error/fatal, `type` system/lifecycle/navigation/user/http/default. **System/process telemetry** (cpu_usage, ram, process_memory, thread_count, app_state…) rides `traces.system`.

**Scope & contexts** — push/pop stack, Bugsee semantics:
```rust
struct Scope { email: Option<String>,        // → request.json.email
               labels: Vec<String>,          // → request.json.labels
               attributes: Map<String,Value>,// → manifest.json.attrs
               contexts: Map<String,Value> }
```
Global base scope behind `RwLock`; `with_scope` pushes a thread-local overlay. **Breadcrumbs are a capture channel on the disk-backed ring** (not in-scope RAM) so they survive crashes.

**Environment** (`request.json.environment`, Rust-adapted, built once, `x-client-type: rust`):
`platform`{type:`linux`/`windows`/`macos`, os version, kernel, utc_offset, disk/mem totals+free, locale} · `app`{name, version, build, exe path} · `hardware`{cpu_count, arch, model?, boot_time, mem, **gpu**{vendor, renderer, api, driver_version, vram_mb} — best-effort, `gpu` feature default-off} · `sdk`{version, build, options, wrapper?}.

**Conventions:** timestamps = epoch ms (monotonic-guarded); custom-data flattened as top-level entry keys **except** breadcrumbs (nested under `data`); three `version` fields preserved (manifest=1, envelopes=2, video-aux N/A); `null` tolerated on read.

## 8. Capture window & disk persistence

**Storage layout** (one per-SDK data dir, single volume so hard links work):
```
<data>/gen                                   persisted monotonic generation counter
<data>/parts/<gen>/<part-n>/<channel>.part   live capture, buffered append streams
<data>/reports/<id>/                         per-report snapshot (hard links) + state
<data>/queue/                                finished *.bundle.zip awaiting upload
```

**Record format** (ours — JSON wire lets us skip msgpack): `[u64 LE timestamp][u32 LE len][entry-as-JSON-bytes]`. Payoffs: timestamp-filter without parsing; parse-free export (concat payloads into the envelope). A per-stream **codec flag** (mirrors mobile's descriptor bit) allows swapping in a compact codec later; **JSON is the default**. Parts stay uncompressed on disk (bounded by eviction); compression happens once in the bundle ZIP.

**Rotation + hybrid eviction (the sliding window):** ~1 s timer rolls the active part (also on a size cap); after each roll, evict oldest parts while **any** cap is exceeded — `max_window` (time), `max_bytes` (global), `max_events`/per-channel count caps (breadcrumbs 100). Eviction = `unlink` (refcount-safe vs. live snapshots). The worker recv's with a timeout bounded by the next rotation deadline and runs the rotation/eviction/telemetry tick whenever that deadline passes — via a message OR a timeout — so **sustained capture traffic can't starve the window** (an earlier "only on recv-timeout" placement let steady messages reset the timer forever, bypassing `max_bytes`/`max_window`).

**Write path (as built):** producers push into an MPSC channel fronted by an atomic depth counter (`Shared::queued` vs. `max_queued`); the send path atomically reserves a slot and, if that would exceed the cap, **drops the incoming (newest) entry** rather than block — so a stalled worker sheds new load instead of unbounded-buffering it. The single capture worker drains → serializes each entry to JSON → frames it (`record::frame`) → `write_all`s the framed bytes to a per-channel file opened `OpenOptions::create(true).append(true)` (`store::PartStore::append`). There is **no mmap, no msync, no `fsync`/`sync_all`** — `flush()` only pushes the `std::fs::File`'s (empty) userspace buffer, so once `write_all` returns the bytes are in the OS page cache. Because capture is **single-threaded** (one worker owns the store), there is no reader/writer concurrency on a live part, so the "committed length" scheme the earlier design added to bound a concurrent-write race is unnecessary and not implemented; instead the reader (`record::RecordIter`) is self-terminating — a zero-length or over-long length prefix (the zeroed/torn tail of a part) ends iteration at the last complete record.

**Durability envelope (be honest):** buffered append survives **process death** (panic, `abort`, signal, `kill -9`) because completed `write()`s live in the kernel page cache independent of the crashing process — this is what the `recovery_e2e` / `kill -9` tests exercise. It does **not** guarantee survival of **power loss / kernel panic**, since nothing is `fsync`'d; a hardening tier (periodic `fsync`, or the originally-envisioned `mmap(MAP_SHARED)` + `msync`) is **future work**, not current behavior.

**Generations:** each launch bumps `gen`; next-launch recovery reads the **previous** gen's parts before advancing.

**Snapshot (no seal, no copy):** pin `[start,end]` (`end`=snapshot instant, `start`=`end−window`) + **hard-link current parts including the active one** into the report dir (`PartStore::snapshot_into`, after a `flush`). At export, read linked parts and include only entries with `start ≤ timestamp ≤ end` (monotonic order → early-stop at the tail; the self-terminating `RecordIter` cleanly ignores any torn trailing write on the active part). Bounds become `manifest.time.start/end`. Cost: 2 timestamps + O(#parts) `link` syscalls. Fallback to copy on FAT/exFAT/cross-volume (capability-probed once). Reflink (APFS `clonefile` / Linux `FICLONE`) a noted future tier.

## 9. Failure pipeline

**Boundary guards (`bugsee-panic`):** every exported FFI fn / SDK thread root / task root runs inside `catch_unwind` + a TLS scope guard (`sdk_scope_depth`, `foreign_callback_depth`, `operation_id`). A panic never unwinds across `extern "C"`. Caught panic → quarantine (Class A–D) → non-fatal event (`type:"error"`, `handled:true`).

**Panic observer:** one chained global hook (`Once` + `take_hook`), installed once, never naively uninstalled; minimal pre-unwind work — write message/file/line/tid/panic_id/raw-PCs into a fixed-size preallocated `PanicSnapshot` (`EMPTY→WRITING→READY`), then call the previous hook. Attribution: SDK when `sdk_scope_depth>0 && foreign_callback_depth==0`.

**Uncaught-panic reporting (mark-and-recover, default).** The hook only *captures*; it never reports. Which path a panic takes is then decided by what actually happens to the process:
- **caught** at a `guard`/`catch_unwind` boundary → reported there as `handled`, and `report_caught` **deletes** the snapshot so nothing later misreads it as fatal;
- **aborting** (`panic = "abort"`) → SIGABRT → the native handler records it → next-launch recovery *correlates* signal + snapshot into one managed crash;
- **uncaught + unwinding** (the Rust **default**) → no signal at all. The panic unwinds out of `main`, and `Recorder::drop` observes `std::thread::panicking()` and therefore **keeps** the liveness marker; next-launch recovery finds the lone surviving snapshot and reports it as a fatal panic.

That last case previously produced **no report whatsoever** — the SDK shut down "cleanly", dropped the marker, and recovery had nothing to find. The two invariants that make a lone surviving snapshot unambiguous are the `report_caught` deletion above and the `thread::panicking()`-gated marker retention.

**Worker-thread panics (reported inline, default).** A panic on a non-main thread kills only that thread — the process survives — so there is *no process death for recovery to observe*, and it would otherwise be invisible. It is therefore reported **inline from the hook**. Crucially, the crash-time isolation objection does not apply here: the process is healthy and this is an ordinary report on an ordinary thread. It is recorded as a **non-fatal error, not a crash** (counting it as a crash would corrupt crash-free-session rates), annotated with the thread name to keep it distinguishable from a panic the app deliberately caught.

**Never double-reported.** The hook cannot know whether a panic will be caught, but it *can* know whether one of our `catch_unwind` boundaries is on the stack: `guard` maintains a thread-local depth, and inline reporting is gated on it being zero. Without that gate a caught panic on a worker thread is emitted twice — once from the hook as uncaught, once from `report_caught` as handled.

**Opt-in immediate reporting (`report_panics_from_hook`).** Extends inline reporting to **main-thread** panics too, so delivery does not wait for a next launch (a short-lived process may never restart). Still gated on the guard depth. The cost is that reporting runs allocation/disk/network on a thread that is about to die, which is why it is not the default.

| panic | default behaviour | reported as |
|---|---|---|
| caught at a `guard` boundary | reported there | non-fatal error, `handled` |
| uncaught on a **worker** thread | reported inline from the hook | non-fatal error + thread name |
| uncaught on **main**, unwinding | marker retained → next-launch recovery | crash |
| uncaught on **main**, aborting | SIGABRT → next-launch correlation | crash |

**Native fatal (`bugsee-native`):** thin wrapper over `crash-handler` + `minidump-writer` (in-process, next-launch upload — mobile parity; optional out-of-process `minidumper` monitor on desktop). Crash-time handler does the minimum: write minidump, read `PanicSnapshot`, set marker, re-raise/terminate. **No allocation/JSON/network in-handler.** Alt-signal-stack for stack overflow.

**Correlation:** native handler matches `PanicSnapshot` (same tid, READY, close timestamp) → collapses "Rust panic + SIGABRT" into one fatal event (`rust_panic_fatal`), dedup key `(session_id, panic_id)`.

**Signatures (two feeds):** panic/handled → SHA-1 over type/message-class + top-N `crate::module::fn(file:line)` + handled + domain; native → SHA-1 over `signal + module + relative-PC`. Lowercase hex, `0x00`-delimited between segments, symbol-normalized, skip SDK frames. **Not byte-identical to iOS/Android (never was across platforms).**

**Native frame capture (as built):** the crash handler records the crashing thread's frame PCs into the marker (`frame=0x…` lines). On Linux the **guaranteed marker (signal/code/address) is written first**, then frames are appended — the Linux frame walk (`backtrace::trace_unsynchronized`) is not strictly async-signal-safe, so a re-fault there loses only the frames, not the report. PCs are absolute (ASLR); the loaded-module map is snapshotted **at install** (dyld/`dl_iterate_phdr` are not signal-safe) into `crash.modules` as `base → size → name`, recording each module's loaded vm extent. Next-launch recovery joins them — `offset = pc − module_base` is ASLR-invariant — but only for a PC that falls **within** a module's `[base, base+size)` range; a PC outside every module (e.g. a frame from a library `dlopen`'d after the install-time snapshot) is skipped rather than misattributed to the nearest-below module with a bogus, ASLR-unstable offset. The native `signature` over the surviving `(module, offset)` pairs lets the **client-side blacklist suppress a native crash-on-launch loop**. Apple: `thread_get_state` on the crashed thread + fault-safe `mach_vm_read_overwrite` frame-pointer walk (handler runs on a separate thread), with arm64e return addresses **PAC-stripped** (`xpaci`) so offsets are stable. Linux/Android: `backtrace::trace_unsynchronized` on the crashing thread; the fault address is read back through the real `siginfo_t` layout (`crash-handler` hands us a reinterpreted `signalfd_siginfo` whose `ssi_addr` does not line up). No frames ⇒ no client signature (server still dedups).

**Next-launch recovery:** read previous gen's parts + crash marker/minidump → build `crash.json` (managed variant for panics with `exception.frames`; thin native variant referencing the minidump) → bundle → queue. **Crash-loop protection:** synchronous export before re-arming. OOM/OS-kill inferred from a session marker.

## 10. Export & transport

**Export** (on worker): read linked parts, timestamp-filter to `[start,end]`, parse-free concat into per-type JSON files (`{"version":2,"events":[…]}`). Build `crash.json`, `manifest.json` (files[] + time + attrs), `request.json`, `.apptoken`. Each manifest `files[]` entry's `type` is the **shared base** per the cross-SDK contract (`events.user`/`events.system` → `events`, `traces.user`/`traces.system` → `traces`); the user/system split stays encoded in the filename only, so type-keyed consumers (worker ingest, MCP get-resource) resolve the file.

**Bundle:** flat ZIP (`<random20>.bundle.zip`) — STORE `request.json` + media, **Zstd method-93** for JSON/text (backend-validated: worker runs Python 3.14 native zstd-zip; mixed method-8/93 round-trips). Streamed entry writes.

**3-step delivery** (`https://api.bugsee.com/v2`, overridable; `x-client-type: rust`):
1. `POST /v2/sessions` — `{app_token, environment}` → `access_token`.
2. `POST /v2/issues?app_token=…` — body = `app_token + access_token + request.json` → `result.endpoint` (presigned URL).
3. `PUT <presigned>` — raw ZIP bytes, `Content-Length`, no auth header. Success → delete bundle + cached URL.

**Server semantics:** `12003` SimilarCrashExists → drop silently; `12004` TooManySimilar → blacklist those `signatures` locally + drop; `401`/invalid → re-register session + retry; `429`/`503`/network → retry.

**Offline queue + cross-launch retry:** bundles in `<data>/queue/`; sync uploader thread (default; optional async `reqwest`) with backoff `min(30s·2^n, 300s)`, caps export 15 / upload 60. Pending-report list + retry counters persist → next launch re-drives. Crash reports flush synchronously first. A presigned endpoint obtained from `create_issue` is persisted (a `.endpoint` sidecar) when its PUT fails transiently, so a retry **resumes at the PUT** rather than re-creating the issue (which would mint a duplicate or trip server dedup `12003` and drop the bundle un-uploaded). The retry counter (and the abandon-at-cap deletion) advances **only** on backoff-paced attempts; a forced `flush()` re-attempts a queued report but never consumes its durable budget, so an offline flush can't delete a still-deliverable crash bundle.

**APM:** separate lighter path `POST /v2/performance/transactions` (one bare transaction, batched ~30 s or realtime).

**TLS:** optional cert-pinning to `*.bugsee.com` for API calls (not presigned PUT).

## 11. Concurrency & threading

| Role | Count | Owns | Does |
|---|---|---|---|
| Producers | app threads | — | `Bugsee::log/event/…` → enqueue only, never block |
| Capture worker | 1 | buffered-append part streams | drain → serialize → frame → `write_all` (append); rotation + eviction + telemetry sampling on the same timed loop |
| Report/upload worker | 1 | `reports/`, `queue/` | export → bundle (zstd-93) → 3-step upload → retry |
| Crash-time context | any thread | — | panic observer / native handler: touch only preallocated `PanicSnapshot` + minidump |

**Crash-time isolation rule:** the fault path never enqueues/allocates/locks — it writes only to fixed preallocated storage + a file-based crash marker (worker may be dead/frozen). **Back-pressure:** the depth-capped channel drops the newest entry when full (never blocks a producer); **reports** are bounded by a separate in-flight counter (heavier per message, so a report storm drops-newest **non-crash** reports over the cap while never dropping a crash); the worker holds no large in-RAM buffer (it frames one record at a time and `write_all`s it), so on-disk growth is bounded by `max_bytes`/`max_window` eviction rather than RSS. **Async-optional:** capture worker is always a `std` thread; only the uploader can become a task on the host runtime (`async` feature). **Shutdown:** guard `Drop` → `flush(timeout)`.

## 12. Crash-time safety, best-effort flush & edge cases

**Best-effort in-flight flush:** at crash, secure the guaranteed artifact first (`PanicSnapshot` / minidump), then attempt flush, then terminate. The crashing thread never touches the queue lock/allocator — it does an async-signal-safe wake (atomic flag + `sem_post`/eventfd/self-pipe) and waits ≤~50 ms for the worker to drain→append→`write_all`→ack; no ack → give up. (This best-effort in-flight flush is a **planned** hardening step; the current worker already durably appends each record as it drains, so completed records survive without it.) Panic(unwind): usually succeeds. panic=abort: hook wakes worker before abort. Native fault: defensive only.

**Recursion guard:** `NORMAL→PANICKING→HANDLING_FATAL→SECONDARY`; second fatal → tiny marker + terminate, no re-entry.
**Quarantine:** Class A (continue) / B (restart) / C (rebuild instance) / D (disable); restart budget (1 immediate, 3/10 min, then disable) — never infinite.
**Coexistence:** chain prior hook + signal handlers; never destructive-uninstall; `native_crash_capture=false` when host owns native.
**Edge cases:** double-panic in `Drop`; foreign C++/ObjC exceptions (shim, never `catch_unwind`); stack overflow (alt-stack); OOM/OS-kill (inferred); disk-full (drop + internal log); multi-process sharing the data dir (per-pid `gen` subdirs + dir lock).

## 13. Testing strategy — red-green TDD, E2E per flow

**Methodology:** red-green TDD; **every flow has an E2E test** written failing first, then implemented to green, then refactored.

**What is actually verified today (in CI):**
- **Unit tests** — e.g. `record.rs` framing/`RecordIter` termination, `signature.rs`, `util.rs`, `errors.rs`.
- **E2E / integration tests** — `bugsee-core/tests/{bundle,persistence,durable_queue,recovery_e2e,runtime_e2e,wire_format}.rs`, `bugsee/tests/{facade_e2e,error_channel,panic_channel,sampling,before_send,apm}.rs`, `bugsee-native/tests/crash_subprocess.rs`, `bugsee-ffi/tests/lifecycle.rs`, and the `bugsee-{log,tracing,reqwest}` capture tests. These cover launch→emit→export→bundle, rotation/eviction, hard-link snapshot, `kill -9`→recovery, and wire-format assertions against `report-bundle-structure` expectations.
- **A new `HttpTransport` test** exercising the default HTTP transport against a mock server (added alongside this doc pass).
- **A record-reader proptest** — property test over `RecordIter` on torn/truncated/garbage buffers (added alongside this doc pass).

**Wire-format conformance (contract-critical):** the `wire_format`/`bundle` E2E tests assert envelope versions, lowercase enums, null-tolerance, and custom-data flattening; signature golden vectors pin grouping. **Planned (not yet wired):** full golden-fixture schema validation against the published `report-bundle-structure` schemas for every type, and the strongest end-to-end gate — feeding a produced `*.bundle.zip` to the actual worker ingest in CI and asserting it parses.

**Matrices:** panic (API/thread/task/holding-Mutex/`&str`/`String`/custom/panicking-Drop/double-panic/panic-in-observer/hook-before+after/multi-init); FFI (panic from C/JNI, C++/ObjC exception from callback, reentrancy, nested); native-fatal subprocess (SIGSEGV/SIGBUS/SIGILL/abort/stack-overflow/panic=abort/alloc-abort → artifact + next-launch recovery); correlation (abort→one event, caught→no dup, stale-snapshot→not correlated, cross-thread→not correlated); persistence (rotation, hybrid eviction, hard-link refcount, timestamp-filter, torn-tail/truncated-record recovery, `kill -9`→recovery); coexistence (both init orders).

**CI (as built — `.github/workflows/ci.yml`):** on every push/PR, a MATRIX across `ubuntu-latest`/`macos-latest`/`windows-latest` running `cargo build`/`test`/`clippy -D warnings` per platform (`fmt --check` once on ubuntu — it is platform-independent; clippy is not, since lints only see the code the active `cfg` selects). `fail-fast: false`, so a break on one platform still reports the other two — which is what says whether it is platform-specific. Plus a separate job pinning the MSRV toolchain (1.88) that builds the workspace `--locked`.

**Rigor — planned, not yet in CI:** `loom` (queue interleavings — note the "committed-length protocol" no longer applies now that capture is single-threaded), `miri`/ASan/TSan (FFI/native `unsafe`), a **fuzz** target over the part reader (the record-reader **proptest** above is the currently-shipping approximation), and a **cross-platform CI matrix** (Linux/Win/macOS full; Android+iOS FFI smoke — the current workflow runs a single host).

## 14. Rollout plan (TDD, E2E-per-flow)

- **Phase 0 — Harness:** workspace skeleton; mock Bugsee server (3-step API); bundle-schema validator; subprocess crash-runner; temp-data-dir fixtures; CI (build/test/clippy/fmt + pinned-MSRV job — **done**). Cross-platform matrix + loom/miri/fuzz jobs are **planned follow-ups**.
- **Phase 1 — Ingestible bundle (core):** data model → capture worker → disk parts → hybrid window → hard-link snapshot → timestamp-filter export → JSON → ZIP(zstd-93) → 3-step transport → offline queue. E2E: `launch→emit→upload→valid bundle`; `create_report→mutate→upload`.
- **Phase 2 — Handled errors + caught panics:** `bugsee-panic` observer + boundary guards, `capture_error/message`, `bugsee-tracing`/`log`, panic signature feed, quarantine.
- **Phase 3 — Fatal + next-launch + correlation:** `bugsee-native`, recovery, correlation, native signature feed, best-effort flush, abnormal-exit inference.
- **Phase 4 — Integrations & APM:** `bugsee-reqwest`, telemetry sampler, APM `performance.json` + `/v2/performance`, sessions/release-health.
- **Phase 5 — Hardening:** retry/backoff maturity, `before_send`/`before_breadcrumb`, default sanitizer, sampling, rate limits, coexistence.
- **Phase 6 — Mobile/FFI:** `bugsee-ffi` C ABI, per-platform native backend, optional out-of-process minidump (desktop), reflink tier.

**Gate:** a phase ships only when its E2E flows are green **and** unit + property tests pass under the CI job (build/test/clippy/fmt + MSRV). Fuzz/loom/miri are aspirational gates, tracked as planned follow-ups rather than currently enforced.

## 15. Backend-coordination items — **resolved** (implemented in `appserver` + `worker`)

The ingestion contract is now implemented end-to-end. The keystone is that routing
keys off **`environment.sdk.type == "rust"`**, not `platform.type` — the same
discriminator the JS SDK uses. This is required, not cosmetic: Rust reports an
OS-typed platform, so a `macos` report would otherwise fall through to the
worker's Apple/Mach-O branch and a handled Rust error into the managed generic path.

1. **`x-client-type: rust` + OS-typed `platform.type`** — *done*. `appserver`
   gained a `rust` **umbrella application type** (`applicationTypes.APPLICATION_TYPE_RUST`),
   modelled on `javascript`: one app per project, with the per-session OS
   (`linux`/`windows`/`macos`) riding `environment.platform.type`.
   `utils.isValidForClient` accepts `x-client-type: rust` only for a `rust` app.
   `isSupportedSdkVersion` reads a **flat** floor from `cfg.core.sdk.rust` (one
   crate family across every OS — unlike the JS per-runtime floor).
2. **Rust `crash.json` shape** — *done*. Every `crash.json` this SDK writes carries
   three **provenance** fields, making the document **self-describing**:
   **`source_sdk`** (`"rust"`), **`source_platform`** (`linux`/`windows`/`macos`/
   `android`/`ios`) and **`source_arch`** (`arm64`, `x86_64`, …). All three mirror
   the corresponding `environment` values from a single source of truth
   (`model/environment.rs`), so the two documents cannot disagree — and
   `source_arch` deliberately uses the **symbol-pipeline spelling** (`arm64`, not
   Rust's `aarch64`), otherwise a crash's arch would never string-match the arch
   recorded on its own symbols (`normalize_arch` strips ISA suffixes; it does not
   translate vocabularies). The backend
   picks its processor from the crash document itself rather than from a second
   file. This matters because the two do not travel together — on the worker's
   resymbolication path `crash.json` is read from S3 while `environment` comes
   from a separate `api.get_recording` call, so a missing or partial environment
   previously left the crash unroutable (and, before the accompanying guard fix,
   raised before routing even ran). `environment.sdk.type = "rust"`
   (`model/environment.rs::SDK_TYPE`) remains as the **fallback**, so SDKs that do
   not emit `source_sdk` yet route exactly as before. `worker/jobs/bundle.py`
   resolves the two with `_crash_source_sdk()` and dispatches to the new
   **`worker/crash/rust.py`**, which handles both variants:
   the *managed* one (panic / handled error: `exception` + `frames` + `cause`
   chain) and the thin *native* one (`exception_type: "native"`, `signal{…}`,
   plus the generic minidump processor when a dump was harvested).
   `__detect_platform` is bypassed entirely for Rust, so its
   iOS-fields-else-Android fingerprinting can no longer misclassify us.
3. **Signature representation** — *resolved, and the client feed is kept.* The
   contract is: **client signatures ride `signatures[]` unprefixed; the server
   APPENDS its own `"s."`-prefixed ones**, and grouping is a Mongo `$in` over that
   flat array — so both are live grouping keys and neither clobbers the other.
   (Precedent: the Android NDK path already emits an unprefixed "imitation of the
   client's signature".) This closes the open question about the native
   `signatures[]` placement: sending `[<client sig>]` is correct.
   Rust-specific wrinkle handled in `crash/rust.py`: the shared managed signature
   builder takes its location from the *deepest* `cause`, but a Rust `source()`
   chain carries a backtrace only on the **outermost** error, so without a
   fallback every handled error with a cause produced *no* server signature and an
   "Unknown location" summary. The digest still covers the whole chain, so distinct
   root causes at one call site keep grouping apart.
4. Zstd **method-93** is already backend-validated (worker Python 3.14) — no change needed, noted for reference.

**Symbols.** A `rust` app rides the per-app **`symbols/`** store + `symbols.*`
worker jobs (alongside `ios`/`javascript`), not Android's `mappings/` + ProGuard
`mapping.*` path — the worker resolves a Rust minidump against that unified
store, so the two halves must agree or every Rust minidump stays unsymbolicated.

**Still open (not required for ingestion):** the `viewer` dashboard has no
Rust-specific rendering yet (reports display through the generic managed path),
no Rust entry exists in the app-create wizard, and there is still no cross-repo
E2E feeding a real `*.bundle.zip` to the worker's ingest (DESIGN §13).

## 16. References

- `DESIGN_PLATFORM.md` — approved platform/engine layout amendment (shared engine, capability traits, C vtable, console NDA siblings).
- `DESIGN_CAPTURE.md` — approved capture subsystem port (Android actors, protobuf parts, dedup store, C extensions).
- `report-bundle-structure` (GitHub `bugsee/report-bundle-structure`) — the authoritative cross-platform wire contract (extracted from Android SDK 7.0.0).
- `rust_crash_panic_capture_design.md` — original crash/panic capture research (this repo).
- Android SDK `com.bugsee.library` (`Bugsee.java`, `OptionsDescriptors.java`, `IssueReportingRequest.java`, `ReportFile.java`, `ExceptionSignature.java`).
- iOS nextgen `BugseeLib` (`Reporting/`, `Capture/`, `Detection/Crash/`) — export/persistence/crash mechanics.
- appserver `worker` (`utils/compression/zipfile.py`, `typedefs/crash.py`, `crash/processors/__init__.py`) — ingestion ground truth.

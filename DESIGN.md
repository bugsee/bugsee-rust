# Bugsee for Rust — Design

**Status:** Approved design (brainstorm-validated). Implementation in progress.
**Date:** 2026-07-25
**Scope:** A standalone, cross-platform Rust SDK — a **crash + native-fatal + panic + handled-error reporter** built in the architectural style of the Bugsee mobile SDKs, producing **backend-compatible report bundles** that ingest into the existing Bugsee appserver/viewer unchanged.

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

**Data directory resolution and failure modes (read-only / ephemeral hosts).** The facade picks the data directory in this order: (1) `LaunchOptions::data_dir`; (2) `BUGSEE_DATA_DIR`; (3) platform default `bugsee/<token>` under `$XDG_DATA_HOME`/`~/.local/share` (Linux), `~/Library/Application Support` (macOS), `%LOCALAPPDATA%` (Windows), skipped if the base is unset (container with no home); (4) `<temp_dir>/bugsee-<uid>/<token>` (`bugsee/<token>` on Windows). A candidate must pass a create + `create_new` write-probe (not mere existence). **A directory the host asked for (1 or 2) is final**: if unusable the SDK does not use another disk location, it runs in memory and reports why in `StorageStatus::explicit_rejected`; only the defaults (3, 4) fall through to each other. The temp fallback is created `0700` per component and refused if a component is a symlink or owned by another user (the unix temp dir is world-writable). Launch never fails for storage reasons. Outcomes:

| Failure | Outcome |
|---|---|
| Default candidate unusable, a later default works | falls through; one stderr line; `StorageStatus::skipped` lists why |
| Requested directory unusable, or no default usable, or `Session::begin`/`PartStore::new` fails (also in a fork child) | **memory mode** (`StorageMode::Memory`): no capture window (entries dropped), no liveness marker, native crash handler and panic snapshot not installed; handled errors and manual reports are assembled without capture context and delivered from an in-memory backlog capped at 16 (an entry counts until its delivery settles; overflow drops the newest with `DropReason::StorageUnavailable`); a `TooManySimilar` verdict is remembered in memory; a clean stop (guard drop) gives the backlog one last bounded (1.5 s) attempt, but a report still unsent when the process dies is lost; a fork child starts with an empty backlog (the parent's is the parent's to send) |
| Directory exists but cannot take NEW files (full, read-only remount) | memory mode for new reports, but the directory is still **drained and recovered from**: its queued bundles are delivered and its crashed sessions recovered (claims are best-effort there; a crash that cannot be re-queued on disk is delivered from memory). Bundles delivered but undeletable are remembered and not resent in this process; a later launch with a writable disk may resend them and the backend deduplicates. Every such rejected candidate is drained, including a full default that lost to a later writable one and the private temp tree (which must be a real directory of ours); a stale claim that cannot be unlinked counts as "no claim possible", not "held elsewhere"; the on-disk `.blacklist` seeds the in-memory one. Only a missing or non-directory path is skipped |
| Directory vanishes / becomes unwritable mid-run | capture appends are best-effort and dropped; the rotation tick rewrites a missing `sessions/<gen>.alive` (a wiped-and-recreated tree would otherwise make the next launch's orphan sweep drop this session's crash); a report that cannot snapshot or enqueue is rebuilt without context and held in memory, so it still goes out; `StorageStatus::degraded` turns true and `crash_recovery_available()` false |
| Disk full / permission denied at enqueue, snapshot, marker | same as above; the `reports/<id>` snapshot dir is reclaimed by `SnapshotHandle::drop` |
| Crash handler cannot write its marker | the raw `open`/`write` return values are ignored; the process still dies with the original signal (watchdog bounds the handler) |

The in-memory backlog retries like the disk queue: it caches the presigned endpoint (a failed PUT resumes at the PUT, no duplicate issue), backs off on the scheduled path, and a flush honours its deadline.

Crash recovery needs the data directory to **outlive the process**: on a container, mount a volume and point `data_dir`/`BUGSEE_DATA_DIR` at it. The temp fallback is commonly wiped on restart. `Bugsee::storage_status()` reports `mode`, `data_dir`, `source`, `skipped`, `explicit_rejected`, `degraded`, `crash_recovery_available()` and `may_be_ephemeral()` (the SDK cannot tell a volume from a scratch disk; the temp fallback is the only case it flags). No FFI accessor yet (follow-up).

**Release note, default location moved.** Earlier builds defaulted to `<temp>/bugsee/<token>`; the default is now the platform location (3) or the private temp fallback (4). When no directory was requested and the old location exists, is a real directory and is owned by the current user, the launch recovers its crashed sessions and delivers its queue once, best effort, including when no new directory could be created. Recovery runs on the worker before a `flush` is acknowledged and `flush` drains the legacy queue with the data directory's, so a flush covers it; the uploader stops polling it once its queue is empty. It is not claimed to be lossless: a legacy session whose process is still alive is skipped, and a legacy directory that fails the ownership check is left alone. Hosts that set `data_dir` were never using it.

**Unsafe.** The facade makes two `geteuid()` calls (unix) to name and vet the private temp directory. Core adds `fcntl(F_SETLK)` (unix) / `LockFileEx` (Windows) for claims, a `kill(pid, 0)` probe (plus `proc_pidinfo` on macOS, `OpenProcess`/`WaitForSingleObject` on Windows) for legacy markers only, and one `UnsafeCell` registry guarded by a pid-stamped spin lock (below).

**Several processes on one directory (#14).** Every pending item is owned by exactly one process at a time, arbitrated by an **OS byte-range lock** on a `*.lock` file: a queued report by `queue/<bundle>.lock`, a session being recovered by `sessions/<gen>.recover.lock`, and a *live session* by `sessions/<gen>.lock`, held for the session's whole life. The kernel releases the lock when the process dies however it dies, so nothing can go stale: there is no owner pid, start time, lease or takeover protocol to get wrong. Choices that follow from this:

- **Pid namespaces.** Containers sharing a volume each see their app as pid 1; a pid (or pid plus start time) written in a file means nothing to the other. A lock does not depend on it, so a peer's live session is never mistaken for a crash and a dead one is never kept alive by pid collision. The `.alive` marker's pid is consulted only for a marker with no `.lock` beside it, i.e. one written by an older SDK build.
- **Why `fcntl`, not `flock`/OFD locks.** `fcntl` locks are per process and are not inherited across `fork`, so a prefork child never holds its parent's claim; `flock`/OFD locks follow the open file description and would. (MSRV 1.88 predates `File::lock` anyway, hence libc/FFI.)
- **Costs of per-process locks, handled in `claim.rs`.** They do not exclude threads, so a registry of held paths does (keyed by pid and spin-locked with the holder's pid stamped in, so a `fork` child that inherits it mid-update breaks the lock instead of deadlocking). POSIX drops a process's locks on a file when *any* descriptor for it closes, so a lock file is never opened twice by one process: the registry is checked before each open, and a claim closes its descriptor before leaving the registry.
- **Ordering at launch.** `Session::begin` creates the marker (`O_EXCL`), takes `<gen>.lock`, then writes the pid. Until the pid is written the marker is empty and fresh, which peers skip even if the lock reads free. A lock that cannot be held with kernel enforcement (contended past a short retry, or the filesystem rejects locks) is *removed*: an existing but unheld lock file reads as "that session died", so peers fall back to the marker's pid instead. On unix a probe asks `F_GETLK` and never takes the lock, so it cannot steal one from a launching peer.
- **Unclean shutdown.** If the worker or uploader thread is detached at shutdown it may still be writing the session, so the session's lock is kept until the process exits (the kernel then drops it) rather than released on drop.
- **Filesystems without locks.** If the filesystem rejects byte-range locks (`ENOLCK` on NFS without lockd; `EOPNOTSUPP`/`ENOSYS`/`EINVAL` on FUSE, 9p) a claim degrades to thread-only exclusion with a one-time message on stderr, and a probe reports unknown so recovery uses the marker's pid. The queue keeps draining and sessions keep recovering, but processes sharing that directory are no longer kept from delivering a report twice. Locks that *succeed but do not exclude* (Docker Desktop bind mounts, WSL2 drvfs/9p, NFS mounted `nolock`) cannot be detected and behave the same way: run one SDK host per directory there.
- **Paths.** The in-process registry canonicalises the lock file's directory, so `/tmp` vs `/private/tmp` or relative vs absolute paths are one claim. A `fork` child clears the registry (`claim::reset_after_fork`, from the fork hook) before claiming anything.
- **Stale scans.** After winning a session claim the marker is re-checked, since a peer may have recovered and discarded it since the scan (this caused phantom duplicate reports). `queue::gc_orphans` only reaps files untouched for 60 s: a peer's in-flight `enqueue` looks like an orphan, and reaping it lost the report. Lock files of finished items and dead sessions are swept when free.
- **Compatibility.** The `.alive` marker is still a bare pid and the lock files are new names, so older builds sharing the directory keep working. They do not take part in the locking, so a mixed-version directory (a rolling upgrade) gives no exactly-once guarantee between old and new processes, and a new process treats an old build's session by its pid as before. Run one SDK version per directory for the guarantee.
- **Limits.** Advisory locks on network filesystems are best effort (see above). The only timestamps relied on are the 30 s grace for an empty marker and the 60 s orphan grace, which assume clocks within seconds. Delivery is at-least-once: a holder that dies after uploading but before removing the item (or after enqueueing a recovered report but before discarding the session) makes its successor do it again. Platforms with neither unix nor Windows locks exclude threads only.

Tests: `bugsee-core/tests/multiprocess.rs` (real child processes: racing launchers, a SIGKILLed holder, a live peer whose marker names a colliding pid) and unit tests in `claim.rs`, `process.rs`, `queue.rs`.

Tests: `bugsee-core/tests/storage_failures.rs`, `bugsee/tests/storage_fallback.rs`, a memory-mode case in `bugsee/tests/fork_safety.rs`, `options.rs` unit tests. A real ENOSPC (size-capped tmpfs) is not exercised, since mounting one needs root; failures are injected by blocking path components and removing the directory mid-run.

**Early-crash recovery on the launch path (#10).** A host that dies again within milliseconds of launch (a crash loop under systemd/Kubernetes) must not outrun the report of the crash before it. `Bugsee::launch_with` therefore ends with `Recorder::recover_on_launch_path()`: a bounded pass that turns the newest crashed sessions into queue entries (build, `before_send`/blacklist, assemble, enqueue, then discard the evidence) before launch returns. Upload stays asynchronous and resumes from the queue. A bundle is queued with plain file writes and a rename, no `fsync`, so it survives process death (the case here) but not power loss, as everywhere in §8.

- **Threading contract.** The pass runs on a helper thread (`bugsee-launch-recovery`) and `launch` waits at most `launch_recovery_budget` (500 ms default, `LaunchOptions::launch_recovery_budget`; zero disables the pass) by polling a flag (never parking: a parker used before a `fork` is inherited by the child, and on macOS the child then traps in libdispatch when it parks). That is a hard upper bound whatever a `before_send` callback does: on timeout `launch` returns and the pass finishes in the background, so a host that holds its own lock while calling `launch` cannot deadlock against a callback that takes it. The callback can run on an SDK thread before `launch` returns; this is documented on `LaunchOptions::before_send`.
- **Ordering with the crash handlers.** The pass is the last thing `launch_with` does, after the native handler and the panic hook are installed and outside every lock, so a fault inside it is recorded with a stack against THIS session (its marker and `crash.info` path are already this generation's) and reported by the next launch. The previous session's evidence is untouched until its report is queued, and `bump_attempts` runs before processing, so a session that crashes the assemble path is abandoned after `MAX_RECOVERY_ATTEMPTS` launches instead of looping. On a re-launch within one process the native handler is replaced with one for the new generation before the pass runs; a fault in the pass is therefore the new generation's.
- **Worker coordination.** With the pass armed, the worker holds its own recovery back until the pass is done (bounded by budget plus 5 s, so a wedged callback cannot stall capture for good), then skips generations the pass claimed (`launch_attempted`, recorded only after winning the #14 claim). Claims keep the two exclusive in any interleaving. A host using `Recorder::launch` directly with a non-zero budget must call `recover_on_launch_path` or the worker waits that grace out; the core default budget is zero, i.e. no pass.
- **Bounds.** At most 8 sessions (`launch_recovery_max_sessions`), no new session after the budget (checked between sessions, so one huge session can overrun it, and the newest is always attempted), a scan of at most 4096 session markers (and four times as many directory entries; `.lock` and `.attempts` files do not use the cap). **Newest first**: the latest crash is the one most likely to belong to the build now running, and a crash loop makes only recent evidence reachable. What the pass leaves is recovered by the worker's unbounded pass, so nothing is lost; recovered reports may therefore upload newest first, which is harmless because the server deduplicates by signature.
- **Crash-loop escalation.** A fast loop never lets the uploader finish, so the server's `12004` verdict and the local blacklist are never reached. In the pass, a recovered crash whose signature set equals one already queued is dropped (the first copy is kept; once it has been delivered a recurrence is queued again), and recovered crashes stop being queued when the queue holds 64 reports or 64 MiB (`queue::admit_recovered`; the newest is dropped). Not done: a repeat counter on the kept report. The worker pass does not dedup or cap.
- **Scope.** A recovered crash carries no scope (email, labels, attributes) from the new process, on either path: the scope describes the new run, and merging it made the two paths disagree. Persisting scope per session is possible, not done.
- **Not covered.** Sessions of the legacy data directory are recovered by the worker only. A fork child (prefork worker boot loop) recovers on its worker, not on a launch path, so a prefork worker that dies before its worker thread finishes recovery leaves the evidence for a later launch (nothing is lost, it is reported late); the master's pass covers only the master's own launch. In memory mode with a recovery directory (#9), a crash that cannot be queued on disk is delivered from memory and its evidence stays, so a later launch reports it again (the backend deduplicates).
- **Measured** (`launch_recovery.rs::measure_launch_latency`, Apple silicon, synthetic log-only windows at the default 8 MiB / 60 s caps, a pass that waits for completion): empty directory about 0.1 to 0.3 ms; one crashed session about 7 ms release, 21 ms debug; eight identical sessions about 17 ms release, 43 ms debug. The eight-session figure undercounts assembly, since seven of them are dropped as duplicates before being assembled; scrubbing (#11) will add per-session cost. `Recorder::launch` itself is about 0.5 to 0.8 ms of that.

Tests: `bugsee-core/tests/launch_recovery.rs` (a real abort-after-launch crash loop, time and count caps, a blocked callback, zero budget), `queue.rs` and `recovery.rs` unit tests.

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

**Best-effort in-flight flush (implemented, `bugsee_core::crash_flush`):** a producer only queues; the worker appends later, so entries still in the channel (or mid-append) when the process dies are lost. At crash the handler first secures the guaranteed artifact (marker + frames), then does one atomic ticket increment, one non-blocking `write(2)` (Windows: `SetEvent`) to a process-wide wake handle (one pipe/event per process, never closed, so no descriptor-reuse race), and polls an atomic ack for at most 50 ms (`crash_flush::DEFAULT_BUDGET`, shared with the panic hook). It first raises a `dying` flag so the uploader and recovery start no new claims in the window (a kill mid-claim would strand it for the claim grace period). One helper thread per process (created at launch, re-created in a fork child, never in the handler) is blocked on that handle; it posts a `Flush` message through the normal FIFO channel and, when the worker acks, publishes the ticket. FIFO order means an ack implies every entry queued before the crash is appended. The crashing thread never takes the queue lock, allocates or serializes, and a stuck/dead worker or poisoned allocator costs exactly the budget (the 5 s watchdog is not involved). The pid recorded at registration makes a fork child skip the flush until its revived recorder re-registers. A fault on the worker or helper thread skips the wait; the last registered recorder is the flush target. Panic: the hook runs the same bounded flush for a main-thread panic no guard will catch (covers `panic = "abort"` builds with no native handler; with one, the SIGABRT handler flushes too); an unwinding panic is also drained by the launch guard's `Drop`. Native fault: flushed from every fatal exit of the signal handler / the Windows exception filter, after the marker is complete. **Tradeoffs:** zero hot-path cost and no new on-disk format (no spill file: a torn append is the already-tolerated torn part tail, pinned by a recovery test), but the flush needs a live worker and allocator, so a fault that corrupts the heap or hits the worker thread forfeits it, and what is drained is bounded by throughput (about 3.5k ~450-byte events in 50 ms on a debug build; the remainder is lost). A producer-side pre-serialized ring spilled by the handler would survive a dead worker but taxes every event; not built.

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

- `report-bundle-structure` (GitHub `bugsee/report-bundle-structure`) — the authoritative cross-platform wire contract (extracted from Android SDK 7.0.0).
- `rust_crash_panic_capture_design.md` — original crash/panic capture research (this repo).
- Android SDK `com.bugsee.library` (`Bugsee.java`, `OptionsDescriptors.java`, `IssueReportingRequest.java`, `ReportFile.java`, `ExceptionSignature.java`).
- iOS nextgen `BugseeLib` (`Reporting/`, `Capture/`, `Detection/Crash/`) — export/persistence/crash mechanics.
- appserver `worker` (`utils/compression/zipfile.py`, `typedefs/crash.py`, `crash/processors/__init__.py`) — ingestion ground truth.

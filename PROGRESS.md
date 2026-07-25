# Bugsee Rust SDK — Progress

_Snapshot: 2026-07-25 · `main` @ `524198f` · 96 tests green · clippy `-D warnings` + fmt clean · MSRV 1.86 (`Cargo.lock` committed, CI `--locked`)._

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
| 3 — Fatal + next-launch + correlation | ✅ (core) | Native handler, session generations + liveness markers, next-launch recovery, panic↔SIGABRT correlation. **New:** client-side native dedup signature from frame offsets. Open: best-effort in-flight flush, full out-of-process minidump. |
| 4 — Integrations & APM | ✅ (core) | APM transactions/spans, sysinfo telemetry, reqwest network capture. Open: sessions / release-health. |
| 5 — Hardening | ✅ (core) | Durable retry queue (backoff, retry cap, blacklist), `before_send`/`before_breadcrumb`, event sampling. Open: general PII scrubber, rate limits. |
| 6 — Mobile/FFI | ✅ (core) | `bugsee-ffi` C ABI (`include/bugsee.h`). Open: actual iOS/Android target builds + Swift/Kotlin wrappers. |

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

## Open items

### Deferred review findings — need a decision before building

1. **Native `signatures` wire placement (confirm).** Native `crash.json`/`request.json`
   `signatures` now goes `[]` → `[<client sig>]`, matching what iOS/Android send. If the
   backend expects native signatures to stay server-computed only, switch to a local-only
   dedup key instead (frame-capture work is unchanged either way).
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

### Remaining phase work

- Phase 2: subsystem quarantine (auto-disable a repeatedly-faulting integration).
- Phase 3: best-effort in-flight flush at crash; full out-of-process minidump (desktop).
- Phase 4: sessions / release-health.
- Phase 5: general PII scrubber, rate limits.
- Phase 6: iOS/Android target builds; Swift/Kotlin language wrappers.
- CI: cross-platform matrix (currently ubuntu-only); loom/miri/fuzz jobs.

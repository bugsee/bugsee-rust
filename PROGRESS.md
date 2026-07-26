# Bugsee Rust SDK — Progress

_Snapshot: 2026-07-26 · `main` @ `68823c8` + uncommitted contested/F1 fixes · 104 tests green · clippy `-D warnings` + fmt clean · MSRV 1.86 (`Cargo.lock` committed, CI `--locked`; no new deps)._

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

## Recent work — 4th adversarial pass (Opus-verified) + 14 fixes (working tree)

A fourth review: **Fable** finders across 8 risk dimensions → **Opus** skeptics
adversarially verifying each finding (2 lenses, default-to-refute). 34 unique
findings → 25 confirmed, 5 contested, 4 refuted. The confirmed **5 high + 9
medium** are fixed and validated (fmt · clippy `-D warnings` · 101 tests · MSRV
`--locked`; the Linux-only native paths are cfg'd out on the macOS host and were
verified by inspection against `crash-handler` 0.6.3 + `libc`, not executed).

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

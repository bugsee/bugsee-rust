# Out-of-process crash monitor (desktop / server) — design

**Status:** Proposal for maintainer review. No code. Tracks issue #17; constrains #18 (Windows fast-fail), relates to #4 (mobile / Crashpad) and #14 (shared data dir).
**Base:** `main` @ `a1564ae`; the `CrashBackend` contract is read from PR #23 (`origin/refactor/crash-backend`, crate `bugsee-native-api`), not yet merged.

**Provenance legend.** Statements marked **[V]** were verified against this repo or against crate sources in the local cargo registry (`crash-handler 0.8.1`, `crash-context 0.8.0`). Statements marked **[U]** are from memory or from the issue text and have **not** been verified; confirm before relying on them. `minidumper`, `minidumper-child`, Crashpad, Breakpad and WER were not available locally, so everything said about them is **[U]**.

---

## 1. Summary and recommendation

In-process capture is already shipped on Linux, macOS, Windows and Android (**[V]** `crates/bugsee-native`, PROGRESS.md). It cannot be made reliable for a few crash classes because the handler runs inside the damaged process, or never runs at all. An out-of-process monitor fixes some of these (stack overflow, heap corruption, handler-clobbered Windows filters), observes more of the rest (exit codes, immediate delivery), and fixes none of the "killed from outside" cases beyond reporting that they happened.

**Recommendation (details in section 9):**

1. Build it as an **opt-in cargo feature** (`out-of-process`) on a new `bugsee-monitor` crate, implementing the `CrashBackend` trait from `bugsee-native-api`. In-process stays the default and the automatic fallback.
2. **Do not adopt Crashpad for desktop in v1.** Use the Rust-native `minidumper`/`crash-handler` family (same authors and licence family as the crate we already depend on), behind a thin internal wrapper so the engine can be swapped later.
3. **Launch model: re-exec the host's own executable** as the monitor (zero extra files to ship), with a separate-binary mode as an option. The host adds one line at the top of `main`.
4. **Reuse the existing recovery contract unchanged**: the monitor writes `crash.minidump`, `crash.info` (plus new optional keys) and `crash.modules` into the session directory; next-launch recovery needs no new code path. Immediate upload by the monitor is a later phase, gated on the claim mechanism (#14).
5. **Phase 0 is cheap and independent**: on Windows, a tiny monitor that only holds a process handle and reports the exit code (`0xC0000409` fast-fail, `0xC00000FD` stack overflow) closes most of #18 without any dump machinery.

---

## 2. Goals and non-goals

**Goals**
- Reliable fatal-crash reports for stack overflow, heap corruption, and (Windows) fast-fail/`abort()` where the in-process path cannot.
- A crash artifact (minidump) that the server already symbolicates **[V]** (DESIGN §15: worker `crash/rust.py` handles the thin native variant "plus the generic minidump processor when a dump was harvested"; PDB support exists).
- Fully opt-in; zero behaviour change and zero extra process for users who do not enable it.
- Graceful degradation: any failure to start or reach the monitor falls back to the in-process backend and never fails `launch`.
- No change to next-launch recovery semantics, `crash.info` parsing, or the wire format.

**Non-goals**
- Mobile (Apple mobile has no `fork`/`exec` **[U]** per #4; Android is #4's scope, though the engine decision here feeds it).
- Making SIGKILL / OOM-kill / power loss *capturable*. They are inferred, not captured (section 3).
- Full-memory dumps by default. DESIGN §3 says minidump is opt-in and no full-memory dump by default; the monitor keeps that (small dump, stacks plus module list, no heap).
- Replacing the in-process backend, or a daemon shared across applications.
- Remote/cloud crash-processing in the monitor. It writes files; upload stays the SDK's job.

---

## 3. What in-process capture cannot see

Current handler facts **[V]** from `crates/bugsee-native/src/lib.rs` and PROGRESS.md: Unix uses the SDK's own signal handler writing an allocation-free `crash.info` marker with frame PCs; Windows uses `crash-handler` (SEH/VEH plus unhandled-exception filter); module map snapshotted at install into `crash.modules`; no minidump is actually written in-process today (recovery *consumes* `crash.minidump` if present — **[V]** `recovery.rs:25,110`).

| Crash class | In-process today | Why in-process is limited | What a monitor adds |
|---|---|---|---|
| **Windows `abort()` / `__fastfail`** | Abnormal-exit report or panic snapshot only (#18) | Fast-fail terminates via `int 0x29` and skips SEH/VEH/UEF **[U]**; no in-process hook runs | Parent-held process handle gives the exit code (`0xC0000409` **[U]**) for free. A real dump needs a WER runtime exception helper module or a debugger-style attach (section 5). |
| **Stack overflow** | Frame 0 only, walk may yield nothing on Windows (**[V]** PROGRESS.md "Two limitations") | Guard page consumed; handler has a few hundred bytes; Rust's own overflow handler ends in `abort()` | Healthy process reads the faulting thread's stack and context from outside. This is the headline win. |
| **Heap corruption** | Handler may re-fault or hang | `malloc`/`free` metadata is untrusted in the handler's process; allocator locks may be held | Monitor allocates freely in its own heap. |
| **Handler clobbered / second fault** | Windows UEF can be overwritten by other components; re-entry on second fault (**[V]** PROGRESS.md architecture note) | Same address space as the faulting code | Monitor is unaffected by what the host did to its own filters (still needs the in-process stub to *signal* it, see section 5). |
| **Hang / deadlock in handler** | Watchdog exists (**[V]** commit fa3a63b "handler watchdog") | Handler cannot kill itself cleanly | Monitor can enforce a deadline and terminate the host if configured. |
| **OOM-killer (Linux `SIGKILL`)** | Next-launch abnormal-exit inference only | Uncatchable by any mechanism | Cannot capture. Can *detect immediately* and attach context: Linux cgroup `memory.events`/`oom_kill` counters **[U]**, last heartbeat RSS. |
| **`SIGKILL` / `kill -9` / `TerminateProcess`** | Abnormal-exit inference (**[V]** recovery `build_abnormal_exit`) | Uncatchable | Same: immediate detection and exit status where the monitor is the parent, otherwise only "the process vanished". |
| **Power loss / kernel panic** | Not captured; only page-cache durability (**[V]** DESIGN §8) | | Nothing. |
| **macOS jetsam / Windows kill by job object** | Abnormal-exit inference | | Detection only. |

Key framing for stakeholders: the monitor **captures** stack overflow, heap corruption and (with extra Windows work) fast-fail; it **detects sooner and explains better** SIGKILL/OOM; it **cannot** capture the latter.

---

## 4. Architecture options

Evaluated against: reliability gain, Rust ecosystem fit, licence, build cost, packaging cost, platform coverage.

### A. Own helper using `minidumper` + `crash-handler` (recommended for v1)
Crates from EmbarkStudios (`crash-handler` **[V]** MIT OR Apache-2.0, `crash-context` **[V]** and `minidump-writer` **[V]** MIT; `minidumper` and `minidumper-child` **[U]** same org, same licence family). Pattern **[U]**: a client in the host registers a crash event handler that sends the `CrashContext` to a server process over a local IPC channel; the server writes the minidump of the (blocked, still-alive) client using `minidump-writer`; `minidumper-child` provides a helper that re-launches the current executable as the server.
- Pros: pure Rust, we already depend on `crash-handler` (Windows path today), permissive licences, no C++ toolchain, MSRV-compatible check needed, same ecosystem as the symbolicating minidump tooling.
- Cons: smaller user base than Crashpad **[U]**; Windows dump writing may rely on `MiniDumpWriteDump`/dbghelp **[U]**; macOS and sandbox behaviour needs a spike (task-port transfer **[U]**); `minidumper` API stability must be checked (0.x).
- Mitigation: wrap behind our own `OopEngine` trait inside `bugsee-monitor`; do not expose its types publicly.

### B. Crashpad as the out-of-process engine
Apache-2.0 **[U]**; handler is a separate executable (`crashpad_handler`) **[U]**; supports Windows, macOS, Linux, Android **[U]**; Windows has a `crashpad_wer.dll` WER module for fast-fail/unusual crashes **[U]**. Per #4, Bugsee's Android SDK already vendors Crashpad, so there is in-house experience and a possible single engine across Android + desktop.
- Pros: most battle-tested; WER module on Windows; same engine as Android (one place for fixes).
- Cons: C++ build (GN/ninja or CMake wrapper) inside a Rust workspace; a mandatory separate executable to locate and ship (re-exec trick unavailable **[U]**); its own database and upload format (we would use it only to produce dumps, fighting its design); large third-party attack and licence surface; `-sys` + wrapper crate pair per #4's structure.
- Not rejected: it is the right choice **if** maintainers decide one engine for Android and desktop (decision D2).

### C. Breakpad (`google-breakpad`) out-of-process
Older sibling of Crashpad **[U]**; superseded for new work by Crashpad. Not recommended; listed for completeness.

### D. Windows Error Reporting (WER) only
Register a runtime exception helper module (`WerRegisterRuntimeExceptionModule`) and/or configure LocalDumps **[U]**.
- Pros: the only mechanism I know of that sees fast-fail inside the faulting process before termination **[U]**; no extra process.
- Cons: Windows-only; the module is a DLL loaded into the crashing process by WER (separate artifact, bitness-matched, must be trivially simple); WER may be disabled by policy; semantics of what the module can do (request a dump, set report parameters) must be verified in a spike **[U]**.
- Role: **complement** to A on Windows for #18 (Phase 2), not a replacement.

### E. ptrace / Mach exception server with no cooperating in-process code
Monitor attaches to the host (Linux `ptrace`, macOS exception ports via `task_for_pid`, Windows debug API) and catches everything including fast-fail **[U]**.
- Cons: conflicts with real debuggers (one tracer per process on Linux; one debugger on Windows), slows the host, `task_for_pid` is gated on macOS by entitlements/SIP/hardened runtime **[U]**, Yama `ptrace_scope` and container seccomp can deny it **[U]**.
- Rejected as the primary mechanism. A debug-attach monitor on Windows is an *option* for fast-fail only (section 5.1), explicitly off by default.

### F. Supervisor wrapper (monitor is the parent)
A launcher (`bugsee-run -- ./app`, or as PID-1 in a container) owns the host as a child and gets the exact `wait` status (signal number, core flag, OOM `SIGKILL`) **[U]**.
- Pros: the only way to get precise cause for SIGKILL/OOM on Unix; trivial packaging in containers.
- Cons: changes how the app is started; not an SDK-only integration.
- Role: optional later phase for server deployments (Phase 4).

### Comparison

| | A minidumper | B Crashpad | D WER | E ptrace/Mach | F supervisor |
|---|---|---|---|---|---|
| Stack overflow / heap corruption | yes | yes | n/a | yes | partial (exit only) |
| Windows fast-fail dump | no (exit code only) | yes via WER module | yes | yes (debugger) | exit code |
| Extra binary to ship | none (re-exec) or small | mandatory | a DLL | none/small | launcher |
| Build cost | Rust only | C++ toolchain | small C-ish DLL | Rust | Rust |
| Licence | MIT/Apache **[V/U]** | Apache **[U]** | n/a | n/a | n/a |
| Matches Android engine (#4) | no | **yes** | no | no | no |

---

## 5. Design of the recommended approach

### 5.1 Components

```
host process (SDK linked)                     monitor process (same exe re-exec'd, or bugsee-monitor)
 ├ bugsee-core  (capture, marker, recovery)    ├ IPC server
 ├ bugsee-native (in-process backend, fallback)├ crash-context receiver + dump writer (engine)
 └ bugsee-monitor client                       ├ context mirror (user, tags, release, last RSS)
     ├ spawn / connect / heartbeat             └ exit watcher (process handle / pidfd)
     └ crash event handler -> send + block
```

New crate **`bugsee-monitor`** (pulls the engine crates, keeps `unsafe` and third-party deps out of `bugsee-core`, matching DESIGN §5 rule that each integration crate pulls one ecosystem dep). It implements `CrashBackend` (**[V]** PR #23: `name()`, `install(&BackendConfig) -> io::Result<Box<dyn BackendGuard>>`, `BackendConfig` is `#[non_exhaustive]` with only `crash_info_path` today). `BackendConfig` needs new fields (monitor mode, helper path, socket dir) — allowed by `#[non_exhaustive]`.

Backend selection stays in `default_backend()` per #4. With the feature enabled and `LaunchOptions::native_crash_capture_mode(OutOfProcess)` set, selection is: try monitor, on any error fall back to in-process, log once internally. The in-process handler is **not** installed in addition by default (two handlers fight over signals); in the fallback case it takes over.

### 5.2 Launch model

| Mode | How | Pros | Cons |
|---|---|---|---|
| **Re-exec self (default)** | `current_exe()` re-run with a reserved env var and hidden argument. Host's `main` calls `bugsee::monitor::run_if_monitor()` first; in monitor role it never returns. | No extra file in packaging; works in containers and zip-deploys; always version-matched | Needs one line in host `main` before it parses args or spawns threads; host with a custom entrypoint (language runtime embedding the SDK, FFI hosts) cannot do this |
| **Separate binary** | `bugsee-monitor` executable located via explicit option, next to `current_exe()`, or `BUGSEE_MONITOR` env | Smallest monitor, no host cooperation, usable from FFI hosts | Must be packaged and signed (notarisation on macOS, Authenticode on Windows) |
| **Fork without exec** | not offered | | Unsafe in a multithreaded process **[V]** DESIGN/`fork.rs` itself documents fork hazards |

If the helper cannot be found or started, fall back as above. FFI hosts (PHP, Ruby, .NET, Dart, the stated consumers in #4) will almost always need the **separate binary** mode, since they do not own `main`. That is a significant packaging cost for the SDK-as-native-layer story, called out in decision D3.

Process attributes: monitor is started with stdio closed/null, in its own process group/session (Unix) so a terminal Ctrl-C does not kill it before the host (it must outlive the host by a short grace), low priority, no inherited sockets other than the IPC endpoint (close-on-exec everywhere).

### 5.3 IPC

- **Transport:** Unix domain socket (Linux: abstract or per-session directory socket, mode `0600`; macOS: socket in the session dir) and a named pipe with a restricted DACL on Windows. Do not use TCP.
- **Session-scoped endpoint:** name derives from the generation and pid, inside `<data>/parts/<gen>/`; the engine's own protocol carries the crash event.
- **Two logical channels:** (1) the engine's crash channel (opaque to us); (2) our **context channel**: small versioned length-prefixed messages.
- **Context sync** (answers #17's staleness question): the host sends *deltas* on change (user email, labels, attributes, release/build, `app_state`) and a periodic **heartbeat** with last-known RSS/threads. The monitor keeps the latest copy in memory. Breadcrumbs, logs and events are **not** streamed: they already live in the crash-survivable on-disk capture window **[V]** (DESIGN §8), and the monitor/recovery read them from `parts/<gen>`. Context messages are fire-and-forget, non-blocking, and dropped under back-pressure (never block a producer, same rule as DESIGN §11).
- **Crash path:** the host's crash handler (in-process stub; on Unix our existing signal handler, on Windows `crash-handler`) must do the minimum: send the engine's crash event and **block waiting for the monitor's ack with a bounded timeout** (e.g. 5 s, option). On timeout or send failure it falls back to writing the in-process `crash.info` marker itself (the existing async-signal-safe path), so a dead monitor degrades to today's behaviour instead of losing the crash.

### 5.4 Marker and report contract (reuse, do not fork it)

Recovery today **[V]** looks in `parts/<gen>/` for `crash.minidump` (`MINIDUMP_NAME`), `crash.info` (key=value; unknown keys ignored, `_ => {}` in `parse_crash_info`), `crash.modules`, `panic.info`; the thin native variant is built when either minidump or crash.info exists; the minidump is bundled as an `ExtraFile` of type `minidump`.

The monitor writes the **same files**, atomically (write temp, `rename`):

| File | Written by monitor | Notes |
|---|---|---|
| `crash.minidump` | yes (small dump: threads, stacks, modules, exception stream) | Existing recovery bundles it unchanged. Size cap (e.g. 2 MB, option) and PII note: stack memory can contain secrets; keep stack-only, document it, offer `before_send`-style opt-out of attaching it (decision D6). |
| `crash.info` | yes | Same keys (`signal`, `code`, `address`, `time`, `frame=`). New **optional** keys: `backend=out-of-process`, `exit_code=` (Windows NTSTATUS), `oom=1` (cgroup evidence), `monitor_version=`. Old recovery ignores them **[V]**. |
| `frame=` lines | monitor unwinds the faulting thread from the received context | Keeps the client-side native dedup signature and crash-on-launch blacklist working (**[V]** DESIGN §9 "Native frame capture"). Needs no change to `native_frame_signature`. |
| `crash.modules` | monitor rewrites at crash time from the live process | More accurate than the install-time snapshot (covers late `dlopen`/`LoadLibrary`). Same `base size name` format. |

Result: recovery changes are limited to (a) honouring `exit_code`/`oom` for a more specific reason string, and (b) nothing else. Minidump **upload path** is already the bundle `ExtraFile` route **[V]**; no new endpoint.

Two crash artifacts can coexist (in-process fallback marker plus monitor dump) only if the fallback fired after a monitor timeout; the monitor's write is atomic so recovery sees either a complete dump or none.

**Immediate delivery (later phase).** After the host dies the monitor could run recovery and upload itself, which serves servers that never restart and short-lived processes. It must take the per-generation **claim** (`recovery::claim_pending`, **[V]** `claim.rs`, `O_EXCL` file with owner pid and stale takeover) so the monitor and the next launch cannot both deliver. That ties to #14, whose acceptance test (multi-process race, killing the holder mid-recovery) becomes a prerequisite. Because monitor liveness differs from host liveness, the claim's owner pid is the monitor's, which already works with the stale-owner rule.

### 5.5 Windows specifics (#18)

Three tiers, shipping independently:
1. **Exit-code monitor** (Phase 0, no dump): the monitor opens the host process handle and waits; on exit it reports `GetExitCodeProcess`. For fast-fail the exit code identifies the class **[U]**. Report shape: today's abnormal-exit report enriched with exit code and last heartbeat context. This alone upgrades `abort()` from "AppExit::Unknown" to a classifiable report.
2. **Dump via engine** for everything that does reach SEH/VEH/UEF (stack overflow and heap corruption, the main benefit of A).
3. **WER runtime exception helper module** for fast-fail dumps **[U]**, spike-gated; the DLL would be an additional shipped artifact and in-process component, so it must stay trivial (request dump to a pre-agreed path, nothing else).

A debugger-style attach for fast-fail (option E) is explicitly **not** planned.

### 5.6 macOS and Linux specifics

- **Linux:** the monitor must be permitted to read the host's memory. With Yama `ptrace_scope=1` **[U]** a non-ancestor needs `prctl(PR_SET_PTRACER, monitor_pid)` set by the host (the monitor is the child, so it is *not* the host's ancestor). Containers: default seccomp/capabilities may block `ptrace`/`process_vm_readv` **[U]**; the spike must run in a stock Docker container and Kubernetes pod. If memory access is denied, the monitor degrades to exit-code-only plus the in-process marker.
- **macOS:** obtaining the host's task port from a separate process is restricted by hardened runtime/SIP unless the port is handed over explicitly **[U]**; the `crash-context` macOS `CrashContext` carries mach port handles **[V]** (0.8.0 defines it for macOS), which suggests the intended mechanism is port transfer, to be verified in the spike. Hardened-runtime/notarised host apps and the App Store sandbox (which restricts spawning helpers) are the risk; ship the separate-binary mode signed with the host's team ID **[U]**.
- Private Apple API risk noted in PROGRESS.md (`crash-handler` resolves a private libproc symbol on Apple — **[V]** open item 4) also applies to engine A; the spike must check what the monitor path pulls in.

---

## 6. Packaging and licensing

- **Cargo feature layout:** `bugsee` feature `out-of-process` -> `bugsee-monitor` -> engine crates. Default features unchanged; `bugsee-core` gains nothing.
- **Re-exec mode** adds no distributable. **Separate-binary mode** adds a `bugsee-monitor` bin target, but cargo does not install a dependency's binaries into a user's app package: docs must tell users to build/ship it (e.g. `cargo install`, a `cargo-dist`/CI artifact, or vendoring into their image). This is the main adoption cost and the reason re-exec is the default.
- **Licensing:** repo is `license = "Proprietary"` **[V]** (workspace Cargo.toml). Engine A: MIT/Apache-2.0 **[V for crash-handler, crash-context, minidump-writer; U for minidumper]**, compatible with redistribution in binaries given attribution. Crashpad: Apache-2.0 plus its bundled third-party code **[U]**, requiring a notices file and a vendoring provenance note per #4's rule (name only the public upstream). There is no `deny.toml`/licence gate in CI today **[V]**; add `cargo deny` (licences and advisories) as part of Phase 1.
- **Sandboxes/stores:** the Mac App Store and iOS forbid or restrict spawning helper processes; Windows Store/MSIX allows helpers inside the package; Linux Snap/Flatpak confine `ptrace` **[U]**. Policy: in any sandbox where spawn or memory access fails, fall back to in-process silently. Document the unsupported list rather than promising.
- **Serverless/containers:** AWS Lambda disallows or limits ptrace and extra processes **[U]**; treat as unsupported (fallback). Containers: usually fine with a re-exec'd monitor in the same container, but memory access depends on seccomp/caps (section 5.6). Do not run the monitor as a sidecar container in v1 (cross-container memory access is not available).
- **Binary size/startup:** re-exec means the monitor carries the whole host binary in its page cache but not in RSS if it never touches host code; measure, and keep the monitor role's startup path minimal (decide before any host `main` logic runs).

---

## 7. Failure modes

| Failure | Behaviour |
|---|---|
| Monitor fails to spawn / cannot connect | `install` returns `Err`; caller falls back to in-process (`CrashBackend` contract **[V]**: error means "no native capture this run" and the SDK continues). Log once to the SDK's internal log. |
| Monitor dies mid-session | Host detects broken IPC (send error / missed heartbeat reply), tries one respawn per budget (like the quarantine restart budget, DESIGN §12: bounded, never infinite), else swaps to in-process handler. A crash in the gap is covered by the in-process marker path. |
| Monitor hangs after a crash event | Host's bounded ack timeout expires, writes in-process marker, terminates. Monitor additionally has its own watchdog and gives up writing the dump after a deadline, leaving no partial file (temp then rename). |
| Host exits normally | Host sends a goodbye; monitor exits. If the host vanishes without goodbye and without a crash event, the monitor treats it as abnormal exit (exit status path), never as a crash with a dump. |
| Host `fork()`s | The child shares the IPC socket but is a *different* process; the monitor must not treat a crash of the child as the parent's. The existing fork hooks **[V]** (`fork.rs`, the `rebind` of the marker path) already retarget the in-process marker; the monitor client must do the same: close the inherited connection in the child, and either connect a new monitor for the child's session or fall back to in-process there. The monitor identifies the crashed pid from the engine's context and rejects a pid it does not own. |
| Several processes (#14): prefork master plus workers | One monitor per host *session* (generation), not one per machine. Prefork workers each get their own connection (or share the master's monitor with per-pid session records). Spike chooses; v1 default is one monitor per process that called `launch`. Recovery exclusivity still uses claims. |
| Monitor orphaned (host `SIGKILL`ed) | Monitor sees EOF/handle signalled, writes the abnormal-exit evidence (exit status where available), and exits within a bound. Monitor must never linger indefinitely. |
| PID reuse | Hold a process handle (Windows) or `pidfd` (Linux 5.3+ **[U]**) rather than a bare pid; macOS/older kernels fall back to pid plus start-time check. |
| Crash while monitor is writing for another crashed process | Not applicable in v1 (one monitor per session). |
| Disk full / data dir unwritable in monitor | Same as in-process: drop and internal log. |
| Two crash handlers (host's own Crashpad/Sentry) | Existing coexistence rules apply **[V]** (DESIGN §12, `crash_coexist` tests). The monitor-based handler must still chain to previous handlers and honour hosts that recover from faults (commit a1564ae). Recovering-runtime hosts (CLR/JVM/V8) are a hard case: the in-process stub must decide "fatal vs recovered" *before* involving the monitor, reusing the current logic, otherwise every recovered SIGSEGV costs a dump. |

---

## 8. Security: IPC from an untrusted process

The monitor runs with the host's privileges and parses bytes from the host, but anything else on the machine may try to connect.

- **Endpoint access:** Unix socket in a `0700` directory under the data dir, socket `0600`; verify peer credentials (`SO_PEERCRED` / `LOCAL_PEERCRED` / `getpeereid`, named-pipe client token) and require the peer to be the expected host pid and same uid. Windows pipe: restrictive DACL, `FILE_FLAG_FIRST_PIPE_INSTANCE`, reject remote clients.
- **Treat the host as potentially corrupted:** by definition the client is a damaged process. Parse messages defensively: bounded lengths, bounded string sizes, versioned schema, no pointers or addresses trusted except through the engine's validated crash context; never `eval`/exec anything from messages; all allocations capped.
- **No path injection:** the monitor derives all output paths itself from its own startup config (session dir). Messages carry no paths.
- **Dump contents:** minidumps contain stack memory; they inherit the data-dir permissions (`0700`) and are removed after delivery like other artifacts. Redaction is not possible post-hoc inside a minidump; hence the opt-out in D6.
- **Privilege:** run unprivileged, never setuid; on Linux consider dropping capabilities and installing a seccomp filter in the monitor after setup (Phase 3 hardening, optional).
- **Env var spoofing in re-exec mode:** the role marker is honoured only when paired with a per-session random token passed over an inherited fd or the pipe name, so a user setting an env var cannot turn a normal run into a monitor.
- Fuzz the context-channel decoder (matches the open "fuzz" item in PROGRESS.md).

---

## 9. Platform plan and phasing

| Phase | Scope | Exit criteria |
|---|---|---|
| **0 — Prereqs** | Merge PR #23 (`bugsee-native-api`). Land #14 claims where needed. | Backend trait on `main`. |
| **1 — Windows exit-code monitor** (closes most of #18) | Tiny monitor (re-exec or separate binary) holding the process handle; enriched abnormal-exit report; `exit_code` key. No dumps, no engine. | CI test: `std::process::abort()` on Windows yields a report carrying the fast-fail NTSTATUS; monitor failure falls back cleanly. |
| **2 — Spike: engine A on Linux + Windows** | `bugsee-monitor` with `minidumper`-style engine; re-exec launch; IPC + context deltas; marker contract of 5.4; stack overflow and heap corruption tests. Measure in Docker, Yama on/off. | Stack overflow produces a symbolicatable report on both OSes; recovery unchanged; fallback verified; go/no-go doc. |
| **3 — macOS + packaging + hardening** | macOS port transfer, signed separate-binary mode, `cargo deny`, fuzzing, sandbox matrix. | Notarised sample host runs; unsupported-sandbox matrix documented. |
| **4 — Optional** | Immediate upload by the monitor (claims); supervisor wrapper (option F) for exact OOM/SIGKILL cause; WER runtime exception module for fast-fail dumps. Revisit Crashpad if D2 decided "one engine". | Per-item decisions. |

Per-platform summary: **Linux** — engine A, needs ptrace access (Yama/container); **macOS** — engine A with port transfer, highest uncertainty; **Windows** — exit-code first, engine A for SEH-visible crashes, WER module for fast-fail dumps; **Android** — out of scope (#4/Crashpad); **iOS** — impossible.

---

## 10. Testing strategy

Extend the existing subprocess harness (**[V]** `crates/bugsee-native/tests/crash_matrix.rs`, `crash_windows.rs`, `crash_coexist*.rs`, and the whole-client path test `crates/bugsee/tests/native_recovery_e2e.rs`). Parametrise the matrix over backend (`in-process`, `out-of-process`) so every existing crash class runs against both.

| Crash class | Test | Assertion |
|---|---|---|
| Stack overflow | host recurses infinitely | monitor mode: minidump present, `frame=` lines non-empty, report symbolicates; in-process mode keeps today's weaker assertion (documents the delta) |
| Heap corruption | double free / use-after-free of corrupted allocator state | report produced, host did not hang |
| `abort()` / fast-fail | Windows `abort`, panic=abort | exit_code key present; report not "Unknown" |
| SIGKILL / OOM | `kill -9`; cgroup-limited allocation loop (Linux CI container) | abnormal-exit report arrives promptly, with `oom` evidence only where measurable; no crash dump claimed |
| Monitor death | kill monitor, then crash host | in-process fallback marker produced |
| Monitor hang | monitor stopped (`SIGSTOP`) then crash host | host terminates within ack timeout with fallback marker |
| Spawn failure | missing helper path, sandbox-denied spawn | `launch` succeeds, backend name reports `in-process` |
| fork | fork child crashes while parent alive | child's crash not attributed to parent session; parent unaffected |
| Multi-process | N launchers on one dir, kill monitor mid-write | each pending report delivered exactly once (#14's test) |
| Hostile IPC | fuzzed / oversized / wrong-uid peer | rejected, monitor alive |
| Recovering host | runtime that recovers SIGSEGV hundreds/sec | no dump per recovered fault (a1564ae regression guard) |
| Contract | recovery fed a monitor-produced directory | thin native variant identical in shape to in-process |

Plus: a Windows real-hardware run for PDB round trip like PROGRESS.md did for in-process **[V]**; Docker + Yama matrix for Linux; a size/startup benchmark of re-exec mode; the full bundle fed to the worker ingest once that cross-repo E2E exists (DESIGN §13 planned).

---

## 11. Decisions needed from the maintainers

| # | Decision | Options | Recommendation |
|---|---|---|---|
| **D1** | Proceed with an opt-in out-of-process backend at all | yes / no / only Windows exit-code tier | Yes, phased; Phase 1 is low-risk even if the rest is cancelled |
| **D2** | Engine | A: `minidumper`-style (Rust) / B: Crashpad / decide later | A for v1 behind an internal trait. Choose B instead only if you want a single engine shared with Android (#4) and accept a mandatory C++ helper |
| **D3** | Launch model | re-exec self / separate binary / both | Both; re-exec default for Rust hosts, separate binary for FFI hosts. Confirm whether FFI hosts (PHP/Ruby/.NET/Dart) are a day-one target, since that makes the separate binary primary and packaging central |
| **D4** | Opt-in surface | cargo feature only / plus runtime `LaunchOptions` switch / env var | Cargo feature to compile it in, plus an explicit runtime option; never auto-enabled |
| **D5** | Who uploads | next launch only (v1) / monitor uploads immediately | Next launch only in v1; immediate upload in Phase 4 after #14 |
| **D6** | Minidump privacy | always attach / attach only if enabled / strip stacks | Attach a small stack-only dump when the feature is on, documented, with an option to drop it and keep the marker only. Needs security/privacy sign-off since DESIGN §3 says minidump is opt-in |
| **D7** | Windows fast-fail dumps via WER module | do / defer | Defer to Phase 4; Phase 1 exit code is enough for #18 |
| **D8** | Supervisor wrapper for exact OOM/SIGKILL cause | do / defer | Defer; only if server customers ask |
| **D9** | Support matrix commitment | which sandboxes/containers/stores are "supported" vs "falls back" | Promise only: Linux/Windows/macOS non-sandboxed, standard containers; everything else falls back silently |
| **D10** | Licence posture | accept MIT/Apache deps in a proprietary crate; add `cargo deny` | Yes; add the gate in Phase 1 |
| **D11** | Monitor per process vs shared per data dir (#14) | per process / per data dir | Per process in v1 |
| **D12** | `BackendConfig` growth | add monitor fields now vs behind a generic options map | Add typed fields (it is `#[non_exhaustive]`), review when PR #23 merges |

### Open risks to resolve in the spike
1. `minidumper` current API, maintenance status, and exact Windows/macOS dump mechanism **[U]** — verify against the crate source before committing to D2.
2. macOS task-port handoff under hardened runtime **[U]**.
3. Whether a stock Docker/Kubernetes seccomp profile and Yama allow the monitor's memory reads **[U]**.
4. WER runtime exception module capabilities and constraints **[U]**.
5. Re-exec cost and hazards for hosts with unusual entrypoints (service managers, language runtimes).

---

## 12. Sources

Verified locally: `DESIGN.md`, `PROGRESS.md`, `crates/bugsee-native/src/lib.rs`, `crates/bugsee-core/src/{recovery,claim,fork}.rs`, `crates/bugsee-native-api/src/lib.rs` (from `origin/refactor/crash-backend`, commit 6189ed6), issues #4, #14, #17, #18, cargo registry `crash-handler-0.8.1` (README, Cargo.toml: MIT OR Apache-2.0) and `crash-context-0.8.0` (Cargo.toml: MIT).
Unverified (from memory, to check before acting): everything about `minidumper`, `minidumper-child`, Crashpad (`crashpad_handler`, `crashpad_wer.dll`), Breakpad, WER APIs, `__fastfail` semantics, Linux Yama/seccomp/pidfd behaviour, macOS task-port restrictions, store/sandbox rules, Lambda limits.

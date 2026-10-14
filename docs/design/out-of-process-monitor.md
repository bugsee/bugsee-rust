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
4. **Reuse the existing recovery contract for dump-bearing crashes** (`crash.minidump`, `crash.info`, `crash.modules`; section 5.4), and add **one small recovery change** for deaths with no crash (a distinct `crash.exit` marker, so `crash.info` keeps meaning "native crash"). A monitor-to-recovery **handshake** (monitor lock, dump-complete marker written last) is required before any monitor write is trusted (section 5.4). Immediate upload by the monitor is a later phase, gated on #14's reworked claims and liveness.
5. **Phase 1 is the cheap first step, not an independent one**: on Windows, a tiny observer that only holds a process handle and reports the exit code (`0xC0000409` fast-fail, `0xC00000FD` stack overflow) closes most of #18 without any dump machinery. It composes with the in-process handler and still depends on the `crash.exit` recovery change, the monitor/recovery handshake, and #14's Windows-capable liveness (section 9). Phase 0 is only the prerequisites.

---

## 2. Goals and non-goals

**Goals**
- Reliable fatal-crash reports for stack overflow, heap corruption, and (Windows) fast-fail/`abort()` where the in-process path cannot.
- A crash artifact (minidump) that the server already symbolicates **[V]** (DESIGN §15: worker `crash/rust.py` handles the thin native variant "plus the generic minidump processor when a dump was harvested"; PDB support exists).
- Fully opt-in; zero behaviour change and zero extra process for users who do not enable it.
- Graceful degradation: any failure to start or reach the monitor falls back to the in-process backend and never fails `launch`.
- **No change to the wire format, to the meaning of `crash.info`/`crash.minidump` (native crash), or to the report-precedence rules** (native > lone panic > abnormal exit). Next-launch recovery **does** change, in three bounded ways (section 5.4): a new `crash.exit` marker that only enriches the abnormal-exit report, a launch-time bounded wait on a monitor lock plus a `crash.done` completeness check, and (later) preference for a persisted crash-time context. Old SDK versions ignore the new files.

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

Backend selection stays in `default_backend()` per #4. With the feature enabled and `LaunchOptions::native_crash_capture_mode(OutOfProcess)` set, selection is: try monitor, on any error fall back to in-process, log once internally. **Two kinds of monitor backend, with different composition rules (verified against the code: an exit-code waiter is not a crash handler):**
- **Observer (Phase 1, exit-code/exit-class only).** It does not catch faults. It **composes with** the `InProcessBackend`: `install` keeps today's in-process handler (signal handler on Unix, `crash-handler` SEH/VEH/UEF on Windows) and additionally spawns the waiter. Installing the observer *instead of* the in-process backend would regress every SEH-visible Windows crash (access violation, stack overflow, etc.) from a native report with frames to `AppExit`, because nothing would be catching them. The contract needs a composite (a backend that returns a guard owning both), or the observer is a second, non-exclusive installable alongside the default; this is a small addition to the `CrashBackend` shape in PR #23, which assumes one backend per target (D12).
- **Dump stub (Phase 2+).** It *replaces* the crash-catching part of the in-process handler (the stub still keeps the fatal-vs-recovered logic, section 5.3), so it is exclusive: two fault handlers would fight over signals. Only then does the in-process handler stand down, and in the fallback case (monitor unreachable) it takes over again.

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
- **Context sync (future; does NOT improve v1 reports, so #17's staleness question stays open for v1):** the host sends *deltas* on change (user email, labels, attributes, release/build, `app_state`) and a periodic **heartbeat** with last-known RSS/threads. The monitor keeps the latest copy in memory. Breadcrumbs, logs and events are **not** streamed: they already live in the crash-survivable on-disk capture window **[V]** (DESIGN §8), and the monitor/recovery read them from `parts/<gen>`. Context messages are fire-and-forget, non-blocking, and dropped under back-pressure (never block a producer, same rule as DESIGN §11). **Why it does not reach v1 reports (verified):** in v1 the monitor only writes files and the *next launch* builds and sends the report in `recover_one`, which calls `merge_scope(shared, ...)` and assembles with `shared.env`, i.e. the **new process's** email/labels/attributes and environment, not the crashed process's. That staleness exists for in-process capture today too, and a monitor holding the context in memory does not change it unless the context is **persisted into the session directory** (a `context.json` snapshot written at crash time by the monitor, or continuously by the host) **and** recovery is changed to prefer it over the live scope/env for that session. That recovery change is a separate, unscheduled work item (D13). Until it exists the context channel is worth building only for the heartbeat (last RSS, app state) that feeds `crash.exit`.
- **Crash path:** the host's crash handler (in-process stub; on Unix our existing signal handler, on Windows `crash-handler`) must do the minimum: send the engine's crash event and **block waiting for the monitor's ack with a bounded timeout** and fall back to the in-process marker on timeout or send failure. **Budget (verified):** the existing handler already runs under a hard watchdog, `HANDLER_BUDGET_SECS = 5` (`SIGALRM` on Unix, a `WaitForSingleObject` timeout on Windows, **[V]** `bugsee-native/src/lib.rs`), which kills the host when it expires. The ack wait therefore cannot be 5 s: it would consume the whole budget and the watchdog would fire before the fallback marker is written. Rule: `ack_timeout + fallback_write_reserve <= HANDLER_BUDGET_SECS`. Initial values: ack timeout 2 s, at least 1 s reserved for the marker write, the rest margin; the ack timeout is an option clamped to `HANDLER_BUDGET_SECS - reserve`. **Ordering (corrected): the monitor is involved only after the existing fatal-vs-recovered decision.** The in-process handler already writes a cheap marker header first and then decides whether the process is really dying (`finish_fatal` vs `drop_marker`, **[V]** `bugsee-native/src/lib.rs`, from a1564ae) so that runtimes that fault on purpose and recover (JVM, CLR, V8) pay almost nothing. Sending to the monitor straight after the header, before that decision, would trigger a dump for every recovered SIGSEGV (a dump storm at hundreds per second) and could leave a `crash.minidump`/`crash.done` for a process that did not die. So: header first (as today), run the unchanged decision, and **only on a fatal branch** send the crash record and wait for the ack. On Windows that is the unhandled-exception filter (which runs only after every other handler declined); on Unix there are **two** send points, `finish_fatal` and the nested-abort `finish_original_report`, plus a purely local register save before `call_host` for the overflow case, detailed below. Nothing is sent from the recovered path. A host that recovers never *dumps* via the monitor. Windows has this property structurally (the unhandled-exception filter runs only after every other handler declined). **On Unix, `finish_fatal` alone is not enough** (verified in `posix::handler`, `bugsee-native/src/lib.rs`), because the headline case, stack overflow, usually never reaches it:

| Unix path (verified) | Reaches | Monitor action |
|---|---|---|
| No host handler (`SIG_DFL`/ignored non-sent) or host declined after `call_host` | `finish_fatal` then `die_of` | confirm-fatal |
| **Stack overflow on a small alt stack** (`looks_like_stack_overflow`: fault within 16 KiB below / 4 KiB above `sp`, handler with under 32 KiB of alt stack left) | `complete_report` **before** `call_host`; the host (Rust's own overflow handler) then prints and `abort()`s | **local save only** of the faulting registers before `call_host` (below); no monitor traffic yet |
| **Nested `abort()` from inside the host handler** (`aborting_inside_host`: SIGABRT within 1 MiB below the chained frame) | `finish_original_report`, **never** `finish_fatal`; the original SIGSEGV `ucontext` is gone by now | confirm-fatal, sending the **locally saved** faulting registers from the chain slot |
| Host recovered (`drop_marker`) | marker unlinked | **nothing**: the monitor was never told, so there is nothing to cancel |

The alt stack is 128 KiB when we provide one (`ALT_STACK_BYTES`), and an existing one of at least 64 KiB is kept (`ALT_STACK_MIN_KEEP`); Rust's own per-thread alt stack is small, so the small-stack overflow path is the common one for Rust hosts and the nested-abort path is the common finish.

**How the overflow context reaches the monitor: save locally, send only when fatal.** By the time of the nested abort the original `ucontext` is gone, so the faulting registers must be preserved **before `call_host`**. But the monitor must not hear about a fault that may yet be recovered: the geometry test that identifies an overflow (fault near `sp`) also matches **recovered** guard-page faults (a JVM yellow page, Go stack growth), which can occur in storms, and a rate-limited or full socket could then drop the one record that matters. So there is **no pre-host IPC and no rate-limiting machinery**. Instead (verified in `posix::handler`/`call_host`, `bugsee-native/src/lib.rs`):
1. `call_host` already takes `snapshot(uc)` of the integer registers (a local `Snapshot` of up to 34 words) to detect whether the host changed anything, and already writes the fault into a per-thread **`ChainSlot`** (`sig`, `code`, `addr`, `pc`, `fp`, `generation`; `CHAIN_SLOTS = 16`). The design is to **also store that snapshot's words in the chain slot** (a fixed array of atomics or a plain array published by the existing release stores), i.e. a local, allocation-free memory copy, before invoking the host. Note the slot's existing `sp` field is the **handler-frame** stack pointer used by `aborting_inside_host`, not the faulting `sp`; the faulting `sp`/`pc`/`fp` come from the saved registers and must not be confused with it.
2. **Nothing is sent** on this path. A recovered fault (host handler fixes the page and returns; `drop_marker`) therefore costs the monitor nothing, and `drop_marker` needs no monitor action. It only needs to clear the chain slot as it does today.
3. On a **confirmed-fatal** branch only, `finish_fatal` (which still has the `ucontext` in hand) or `finish_original_report` (which reads the saved registers back from the chain slot, replacing the `sp: 0` it reconstructs today), the stub sends one **crash record** to the monitor on the pre-opened socket: tid, signal, `si_code`, fault address, the saved general-purpose registers (not the FP/xsave area), then waits for the ack (below). The monitor writes the dump using those registers and **reads the target's memory from outside**; the faulting thread's own stack memory is intact (the handlers and the nested abort ran on the alt stack), so the unwound stack is the overflowed one.
4. **Request identity.** The record is keyed by **tid plus a per-process fault sequence number**, not by the marker `generation`: `generation` is `0` for every fault that was not the first in flight (`record_header` returns it only when `IN_FLIGHT` was zero), so keying on it would collide or lose exactly the nested/concurrent cases. The chain slot already identifies the fault by tid.
Dumping all threads from outside before `call_host`, or notifying the monitor early, were both considered and rejected for the reasons above (a dump or a message per recovered fault).

**Ack wait must not run engine or heavy code on the small alt stack.** The stub contains **no** engine logic; the dump is written entirely by the monitor. The only thing the stub does after confirm is wait for the ack, and that is **one blocking syscall with a tiny frame** (`ppoll`/`poll` on the pre-opened socket with the clamped timeout from the budget rule above; or `read` with `SO_RCVTIMEO`), no formatting, no allocation, no locks beyond what the handler already holds. That is within what a handler on under 32 KiB of alt stack already does (the existing `StackBuf` is 160 bytes). The design requires a test that runs the confirm-and-wait path on a deliberately tiny alt stack (for example 8 KiB) and asserts it completes. If that proves unreliable on some platform, the fallback is to not wait at all on the small-stack path: the stub sends confirm, writes its own marker, and the **monitor freezes the host** itself (SIGSTOP of the process from outside, or the debugger-free equivalent) before dumping, accepting that the process may already be gone, in which case the monitor dumps from the saved registers plus whatever memory is still readable and otherwise reports the fallback evidence only. **Recovered faults never reach the monitor:** because it is contacted only from the confirmed-fatal branches, `drop_marker` is unchanged for the monitor (it still unlinks `crash.info` only if the marker is still ours), no cancel protocol exists, and no `crash.minidump`/`crash.done` can be produced for a process that recovered.

**Fallback must not clobber the monitor's work.** The in-process `write_marker` opens with `O_WRONLY|O_CREAT|O_TRUNC` on Unix and `CREATE_ALWAYS` on Windows (**[V]**), i.e. it **truncates** an existing `crash.info`. A late fallback after the ack timeout, if the monitor's rename of a better `crash.info`/`crash.done` already happened, would wipe it. Rule: once the stub has sent a crash event, the fallback path may only **append** to `crash.info` (`O_APPEND`) or create a **differently named** file (`crash.fallback`), and must use `O_EXCL`/`CREATE_NEW` semantics for any file the monitor may also write; it never truncates an existing `crash.info`, and never touches `crash.done`/`crash.minidump`. Recovery prefers monitor artifacts when `crash.done` exists, else uses the fallback evidence. (The initial header written before the decision is the existing truncating write; it happens strictly before any request is sent, so it cannot race the monitor.)

### 5.4 Marker and report contract (reuse, do not fork it)

Recovery today **[V]** looks in `parts/<gen>/` for `crash.minidump` (`MINIDUMP_NAME`), `crash.info` (key=value; unknown keys ignored, `_ => {}` in `parse_crash_info`), `crash.modules`, `panic.info`. **`has_native = minidump.is_some() || crash_info.is_some()`** (`build_report`, **[V]**) selects the thin *native* crash variant; a native marker with SIGABRT plus a fresh panic snapshot correlates into one managed event; a lone panic snapshot is a fatal unwinding panic; a session with none of these is an abnormal exit. So **the presence of `crash.info` or `crash.minidump` means "a native crash happened"**, and neither may be used as a side channel for "the process died some other way".

**Consequence for deaths without a crash (Phase 1 Windows exit code, OOM, SIGKILL).** Writing `crash.info` for an exit-code-only death would flip the session from "abnormal exit" (or from a lone fatal-panic report, which is what an aborting panic on Windows yields today when a snapshot exists) to a thin native report with no signal, no frames and no dump: a regression. So this design adds a **separate marker, `crash.exit`**, and a **recovery change** (small, in `bugsee-core`, shipped before or with Phase 1):
- `PendingSession` gains `exit_info: Option<PathBuf>` (found in `find_pending` like the others); `has_native` is unchanged and ignores it.
- `crash.exit` is `key=value` like `crash.info`: `exit_code=` (Windows NTSTATUS or Unix wait status), `signal=` where known, `oom=1` evidence, `time=`, `monitor_version=`.
- Precedence in `build_report` stays: native crash (with or without panic correlation) > lone panic snapshot > abnormal exit. `crash.exit` never changes which branch is chosen; it only **enriches**: the abnormal-exit report gets a specific reason/domain (for example `AppExit::FastFail`, `AppExit::OomKilled`), and the other branches may attach `exit_code` as context. Abnormal-exit signatures may then include the exit class so distinct causes group apart (today they all collapse into `AppExit::Unknown`); that grouping change needs backend-owner agreement (D14).
- Absent `crash.exit` behaves exactly as today; older SDK versions ignore the file.

The monitor writes the **same files**, atomically (write temp, `rename`):

| File | Written by monitor | Notes |
|---|---|---|
| `crash.minidump` | yes (small dump: threads, stacks, modules, exception stream) | Existing recovery bundles it unchanged. Size cap (e.g. 2 MB, option) and PII note: stack memory can contain secrets; keep stack-only, document it, offer `before_send`-style opt-out of attaching it (decision D6). |
| `crash.info` | yes, **only for a real native crash** (fault context or dump received) | Same keys (`signal`, `code`, `address`, `time`, `frame=`). New optional keys that old recovery ignores **[V]**: `backend=out-of-process`, `monitor_version=`. Exit codes and OOM evidence do **not** go here; they go to `crash.exit`. |
| `frame=` lines | monitor unwinds the faulting thread from the received context | Keeps the client-side native dedup signature and crash-on-launch blacklist working (**[V]** DESIGN §9 "Native frame capture"). Needs no change to `native_frame_signature`. |
| `crash.modules` | monitor rewrites at crash time from the live process | More accurate than the install-time snapshot (covers late `dlopen`/`LoadLibrary`). **Current 4-column format, one line per module: `<base_hex>\t<size_hex>\t<code_id>\t<name>`** (**[V]** writer `write_modules_file` in `bugsee-native`, reader `read_modules` in `recovery.rs`). `code_id` (Mach-O `LC_UUID`, GNU build-id, or PE CodeView GUID+age) is what the symbol store matches on, the symbolication blocker already fixed on main; it is empty only when a module has none. The monitor must compute it with the same logic as the in-process writer, ideally by sharing that code rather than reimplementing it, and `code_id_matches_symbolic.rs` should also be run against the monitor's output. The reader still tolerates legacy 3-column (`base size name`) and 2-column lines, but the monitor must **not** emit them: they lose `code_id` and the report becomes unsymbolicatable. |

Result: the recovery changes are (a) honouring `crash.exit` (`exit_code`/`oom`) for a more specific reason string and signature, (b) the handshake below (skip or bounded-wait on a live `monitor.lock`, use monitor artifacts only when `crash.done` exists, fall back to the stub's evidence otherwise), and (c) the optional crash-time context preference (D13). Nothing else changes: the report shapes, precedence, `crash.modules` format and the bundle path are untouched. Minidump **upload path** is already the bundle `ExtraFile` route **[V]**; no new endpoint.

Two crash artifacts can coexist (in-process fallback marker plus monitor dump) only if the fallback fired after a monitor timeout; the monitor's write is atomic so recovery sees either a complete dump or none.

**Handshake between monitor writes and next-launch recovery (required, v1).** Nothing coordinates them today, and the race is real: `find_pending` runs at the next launch (a supervisor can restart the host within milliseconds, while the monitor is still writing a dump), reads whatever is in `parts/<gen>/`, builds a report from a partial or absent dump, and then `discard` deletes the session, destroying the dump mid-write. Rules:
1. **Ownership while writing.** The monitor creates `parts/<gen>/monitor.lock` (a claim in the sense of `claim.rs`: `O_EXCL`, records the monitor's pid and a start-time token) *before* touching any artifact and removes it after the last rename. `find_pending` treats a session holding a live monitor lock as not yet pending, exactly as a live-owner `.alive` marker is skipped today. **But a plain skip is not acceptable here:** recovery runs only at launch (`run_recovery` is called once with the current generation, **[V]** `runtime.rs`), so a session skipped because the monitor is still writing is not seen again for the whole lifetime of the successor process (a supervisor restarting the host in milliseconds is the common case for servers). Therefore the launch-time scan does a **bounded wait**: for sessions with a live monitor lock it polls (for example every 50 ms, up to a budget of a few seconds, configurable, never blocking `launch` on the host thread: the wait lives inside the recovery pass on the worker) until `crash.done` appears or the lock owner is gone, then proceeds; past the budget it defers the session and schedules **one re-scan** later in the same process (a timer on the worker, not only at the next launch). Related in-flight work: the coordinator notes branch `fix/issue-10` (PR #10) adds a bounded launch-path recovery pass; this wait must fit inside that pass rather than add a second mechanism (contents of that branch **[U]**, not seen).
2. **Completeness marker, written last.** All artifacts are written to temp names and renamed into place; the **final** step is an atomic rename of `crash.done` (it may list the files it covers). Recovery uses monitor artifacts only if `crash.done` exists. Without it (monitor died mid-write) a `crash.minidump` is ignored and the header-only marker from the in-process stub (section 5.3) is the evidence, never a half-written out-of-process crash.
3. **Stale-owner detection must work on Windows.** **[V]** `process_is_alive` returns `false` unconditionally off Unix (`recovery.rs`), so on Windows a live monitor lock would look stale and recovery would run concurrently with the writer; and a bare pid is unsafe under pid reuse. The lock therefore needs an owner identity checkable on Windows (pid plus process creation time via `OpenProcess`/`GetProcessTimes`) and on Unix (pid plus start time from `/proc` or `sysctl`). **Dependency:** the coordinator reports that #14's branch (`fix/issue-14`) reworks claims and liveness including Windows start-time identity; this design depends on that work and does not specify a second implementation. I could not see that branch, so its contents are **[U]**; adapt rules 1 and 3 to whatever #14 lands.
4. **One owner at a time.** The monitor holds `monitor.lock` while writing; the next launch then holds the per-generation claim (`claim_pending`) for recover-and-discard, as today. The monitor does not deliver in v1.

**Immediate delivery (later phase).** After the host dies the monitor could run recovery and upload itself, which serves servers that never restart and short-lived processes. It must take the same per-generation claim (**[V]** `claim.rs`) so the monitor and the next launch cannot both deliver. The earlier statement that the claim's owner pid being the monitor's "already works with the stale-owner rule" was **wrong on Windows** (liveness is always false there, see rule 3), so immediate upload is **blocked on #14's liveness/start-time rework** and its multi-process race test.

### 5.5 Windows specifics (#18)

Three tiers, shipping independently:
1. **Exit-code monitor** (Phase 1, no dump; an *observer* that **composes with** the in-process handler, section 5.1, and never replaces it): the monitor opens the host process handle and waits; on exit it reads `GetExitCodeProcess`. For fast-fail the exit code identifies the class **[U]**. It writes **`crash.exit`** (never `crash.info`; section 5.4) under the 5.4 handshake (`monitor.lock`, then `crash.done` last). Report shape: today's abnormal-exit report enriched with exit code and last heartbeat context; a lone panic snapshot still wins, so an aborting panic with a snapshot is not regressed. This alone upgrades `abort()` from "AppExit::Unknown" to a classifiable report.
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
| **0 — Prereqs** | Merge PR #23 (`bugsee-native-api`). Land #14's claim/liveness rework (incl. Windows start-time identity). Land the `crash.exit` recovery change. | Backend trait on `main`. |
| **1 — Windows exit-code monitor** (closes most of #18) | Tiny observer (re-exec or separate binary) holding the process handle, **installed alongside the in-process handler, not instead of it**; enriched abnormal-exit report via the new `crash.exit` marker; **requires the small recovery change and the handshake of 5.4** (with #14's Windows-capable liveness). No dumps, no engine. | CI tests: `std::process::abort()` on Windows yields a report carrying the fast-fail NTSTATUS; an aborting panic with a snapshot still reports as the panic; fast-restart race (new launch starts while the monitor is mid-write); an SEH-visible crash (access violation) on Windows with the observer enabled still yields a native report with frames (no regression to `AppExit`); monitor failure falls back cleanly. |
| **2 — Spike: engine A on Linux + Windows** | `bugsee-monitor` with `minidumper`-style engine; re-exec launch; IPC + context deltas; marker contract of 5.4; stack overflow and heap corruption tests. Measure in Docker, Yama on/off. | Stack overflow produces a symbolicatable report on both OSes; recovery unchanged; fallback verified; go/no-go doc. |
| **3 — macOS + packaging + hardening** | macOS port transfer, signed separate-binary mode, `cargo deny`, fuzzing, sandbox matrix. | Notarised sample host runs; unsupported-sandbox matrix documented. |
| **4 — Optional** | Immediate upload by the monitor (blocked on #14 liveness); supervisor wrapper (option F) for exact OOM/SIGKILL cause; WER runtime exception module for fast-fail dumps. Revisit Crashpad if D2 decided "one engine". | Per-item decisions. |

Per-platform summary: **Linux** — engine A, needs ptrace access (Yama/container); **macOS** — engine A with port transfer, highest uncertainty; **Windows** — exit-code first, engine A for SEH-visible crashes, WER module for fast-fail dumps; **Android** — out of scope (#4/Crashpad); **iOS** — impossible.

---

## 10. Testing strategy

Extend the existing subprocess harness (**[V]** `crates/bugsee-native/tests/crash_matrix.rs`, `crash_windows.rs`, `crash_coexist*.rs`, and the whole-client path test `crates/bugsee/tests/native_recovery_e2e.rs`). Parametrise the matrix over backend (`in-process`, `out-of-process`) so every existing crash class runs against both.

| Crash class | Test | Assertion |
|---|---|---|
| Stack overflow, both Unix paths | recursion on a thread with a small (about 8 KiB) alt stack and again with a large (128 KiB) one, with Rust's own overflow handler as host (nested `abort()` path), and with no host handler (`finish_fatal`); plus a recovered guard-page fault | dump taken with the overflowing thread's registers and stack in every variant; confirm-and-wait completes on the tiny alt stack; the recovered fault sends nothing to the monitor and produces no dump or artifact (and a storm of them leaves the monitor's counters untouched) |
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
| **D13** | Crash-time context for v1 | accept that recovered reports carry the next launch's scope/env / persist a crash-time context snapshot and teach recovery to prefer it | Persist and prefer it, as its own work item independent of the monitor; #17's context-sync question stays open until then |
| **D14** | New recovery marker `crash.exit` and exit-class signatures | approve / other mechanism | Approve; needs backend owners' agreement on grouping for exit classes |
| **D12** | `BackendConfig` growth and composition | add monitor fields now vs generic map; single backend per target (PR #23) vs a composite/observer slot alongside the default | Typed fields (`#[non_exhaustive]`), plus an observer slot so Phase 1 composes with the in-process backend; review when PR #23 merges |

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

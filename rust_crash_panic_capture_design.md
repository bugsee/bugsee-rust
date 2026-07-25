# Crash and Panic Capture Architecture for an Embeddable Rust SDK

**Status:** Proposed design  
**Research date:** 2026-07-24  
**Target:** Linkable/embeddable SDK implemented partly or primarily in Rust  
**Primary use case:** A production SDK such as Bugsee embedded into applications that may already use Sentry, Firebase Crashlytics, Bugsnag, Datadog, or another crash reporter.

---

## 1. Executive summary

A production-grade Rust SDK should not treat “panic handling” and “crash handling” as one mechanism. They are separate failure channels with different safety properties:

1. **Rust panics that unwind** should be contained at SDK isolation boundaries using `std::panic::catch_unwind`.
2. A **process-global Rust panic hook** should be used only as an observer, primarily to capture the original panic context before unwinding destroys the useful stack.
3. **Native fatal failures** (`SIGSEGV`, `SIGBUS`, `SIGILL`, `SIGABRT`, access violations, Mach exceptions, stack corruption, aborting panics, etc.) require a native crash recorder.
4. The SDK should **not attempt to recover from native memory-corruption crashes**.
5. For an embeddable SDK, **handler coexistence is a first-class requirement**. Global panic hooks, POSIX signal handlers, Mach exception ports, JVM uncaught-exception handlers, and Windows exception handlers may already be owned by the host application or another monitoring SDK.
6. A Rust panic should be correlated with a subsequent native abort/crash so the backend emits **one logical failure**, enriched with both Rust panic metadata and native crash context.

The recommended architecture is:

```text
Host application
      │
      ▼
┌─────────────────────────────────────────────────────────────┐
│ FFI/API Boundary Guard                                     │
│ catch_unwind + SDK scope attribution + state quarantine    │
└──────────────────────────────┬──────────────────────────────┘
                               │
                  ┌────────────┴────────────┐
                  │                         │
              normal path               Rust panic
                                            │
                                            ▼
                              ┌─────────────────────────┐
                              │ Global panic observer   │
                              │ minimal TLS snapshot    │
                              │ before stack unwinds    │
                              └────────────┬────────────┘
                                           │
                                      stack unwinds
                                           │
                                           ▼
                              ┌─────────────────────────┐
                              │ catch_unwind catches    │
                              │ classify + quarantine   │
                              │ return error to host    │
                              └─────────────────────────┘

Any fatal native failure
(SIGSEGV/SIGBUS/SIGABRT/Mach/SEH/panic=abort/...)
      │
      ▼
┌─────────────────────────────────────────────────────────────┐
│ Existing/native crash backend                              │
│ in-process minimal recorder OR out-of-process snapshotter   │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ Merge shared panic context + native crash + breadcrumbs     │
│ persist safely; upload now or next launch                   │
└─────────────────────────────────────────────────────────────┘
```

For Bugsee specifically, the preferred implementation is to **reuse the existing Bugsee native crash layer on iOS/Android rather than install a second independent native crash handler from Rust**. Rust adds a language-aware panic layer and shared metadata to the already-existing crash pipeline.

---

## 2. What competitors actually do

### 2.1 Sentry Rust: a global panic observer, not a panic containment system

Sentry has a dedicated `sentry-panic` integration. Its current implementation is instructive because it is small and explicit:

- The integration is enabled by default in the main Sentry Rust crate.
- It installs a process-global Rust panic hook.
- Installation is guarded by `std::sync::Once`.
- It obtains the previous hook with `panic::take_hook()`.
- Its replacement hook reports the panic and then invokes the previous hook.
- The handler creates a Sentry fatal event, captures a current stack trace, and flushes the client.

Conceptually, the implementation is:

```rust
INIT.call_once(|| {
    let next = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        panic_handler(info);
        next(info);
    }));
});
```

Important conclusions:

- **Sentry observes panics; it does not automatically recover from them.** There is no general `catch_unwind` containment layer in `sentry-panic`.
- Sentry explicitly **chains the previously installed panic hook**, which is essential in an embedded environment.
- The panic hook captures the stack **before unwinding**, which preserves the useful failure site.
- Sentry performs relatively heavy work from the panic hook (`capture_event`, stack capture, `flush`). This is reasonable for a general application SDK, but a low-level embedded SDK should be more conservative because the panic may have occurred while an internal lock, allocator path, logger, or transport component was already compromised.

**Design lesson:** adopt Sentry’s hook chaining and pre-unwind capture, but make the hook much smaller. Treat it as a telemetry snapshotter, not as the reporting transport.

Sources:
- Sentry Rust `sentry-panic` source and README: https://github.com/getsentry/sentry-rust/tree/master/sentry-panic
- Rust panic integration source: https://raw.githubusercontent.com/getsentry/sentry-rust/master/sentry-panic/src/lib.rs

---

### 2.2 Sentry Native: separate native crash backends

Sentry’s native SDK keeps native crash capture separate from language-level integrations. Current Sentry Native documentation exposes several crash backends:

- `crashpad` — out-of-process; default on Windows, macOS, and Linux.
- `native` — newer experimental out-of-process Sentry-native handler.
- `breakpad` — in-process.
- `inproc` — small in-process backend; current default on Android.
- `none` — no crash handler.

This is architecturally important: the crash mechanism is abstracted behind a backend instead of being entangled with the higher-level event API.

Sentry’s documentation explicitly favors out-of-process handling where possible because the snapshot and upload logic is not executing inside the process whose heap/locks/state may be corrupted. The newer `native` backend can emit a native event, a minidump, or both. Sentry also documents an in-process model in which the signal handler performs minimal work and delegates processing to another thread.

On Android, current Sentry Native uses an in-process backend and its own unwinding integration rather than requiring a standalone Crashpad handler.

**Design lessons:**

- Use a **backend abstraction** for fatal crash capture.
- Prefer **out-of-process** on desktop/server environments where deployment allows it.
- On mobile, use a carefully designed in-process recorder with persistence for next-launch upload.
- Keep the crash-time path minimal and defer event construction, symbolication, networking, and compression.

Sources:
- https://docs.sentry.io/platforms/native/configuration/backends/
- https://docs.sentry.io/platforms/native/configuration/backends/native/
- https://docs.sentry.io/platforms/native/advanced-usage/backend-tradeoffs/
- https://docs.sentry.io/platforms/native/advanced-usage/signal-handling/

---

### 2.3 Firebase Crashlytics: chain the host handler and use Crashpad for Android NDK

Firebase Crashlytics does not expose a Rust-specific panic integration. It treats failures according to the platform runtime.

#### Android managed layer

Crashlytics installs a `Thread.UncaughtExceptionHandler`. Its current open-source implementation:

- records/handles the uncaught exception;
- keeps an atomic “currently handling” state;
- avoids recording a managed exception when a native crash already exists for the session;
- **always invokes the previous/default uncaught exception handler in `finally`**.

That last point is a strong precedent for embedded SDK behavior: the monitoring SDK records the event but does not assume ownership of final process semantics.

#### Android NDK

The Firebase Android repository includes Crashpad as a direct submodule under `firebase-crashlytics-ndk`, together with `mini_chromium` and Linux syscall support. Its NDK changelog explicitly records the migration of the underlying native crash reporting implementation to Crashpad.

Current Crashlytics NDK behavior includes:

- native crash capture;
- native build ID requirements;
- native symbol upload for server-side readable reports;
- local persistence followed by reporting after application restart;
- GWP-ASan metadata integration for memory-corruption diagnostics.

Crashlytics itself has no special understanding of a Rust `panic!`. Therefore:

- a Rust panic caught inside Rust is invisible unless the application explicitly reports it;
- a Rust panic that terminates through abort/native fatal behavior is seen as a native crash;
- panic message/source metadata may be lost unless the Rust layer records it before abort.

**Design lessons:**

- Always chain or forward to the pre-existing host failure handler unless explicitly configured otherwise.
- Deduplicate language-runtime and native crash paths.
- Persist fatal crash state locally and finish processing on a healthy next launch.
- Preserve native build IDs and symbols as a first-class release artifact.

Sources:
- Crashlytics uncaught handler: https://github.com/firebase/firebase-android-sdk/blob/main/firebase-crashlytics/src/main/java/com/google/firebase/crashlytics/internal/common/CrashlyticsUncaughtExceptionHandler.java
- Firebase submodules showing Crashpad dependency: https://github.com/firebase/firebase-android-sdk/blob/main/.gitmodules
- NDK changelog: https://github.com/firebase/firebase-android-sdk/blob/main/firebase-crashlytics-ndk/CHANGELOG.md
- NDK setup: https://firebase.google.com/docs/crashlytics/android/get-started-ndk

---

### 2.4 Bugsnag: broad native interception and careful handler coexistence

Bugsnag’s Android documentation states that it automatically detects:

- uncaught Java/Kotlin exceptions;
- C signal failures;
- C++ exceptions;
- ANRs.

Its Apple SDK uses a KSCrash-derived crash recorder. The source includes separate handlers for:

- Mach exceptions;
- POSIX signals;
- Objective-C exceptions;
- C++ exceptions.

A particularly interesting implementation is C++ exception handling. Bugsnag/KSCrash intercepts C++ exception machinery (`__cxa_throw`) to capture a useful stack near the original throw and also installs a `std::terminate` handler. This is analogous to what a Rust panic hook gives us: capture language-level context **before** the runtime converts or unwinds the failure into something less informative.

Bugsnag’s public history also contains an important handler-coexistence fix: uninstalling/restoring signal handlers can accidentally remove handlers installed after the SDK. Their solution moved toward disabling/forwarding behavior instead of naively restoring global handler state.

On iOS, Bugsnag reports fatal crash data on the next launch and uses heuristics for terminations that cannot be trapped directly, including OS-induced OOM termination.

**Design lessons:**

- Capture the language-runtime context at the earliest reliable point.
- Do not assume a global handler can be safely “uninstalled” later.
- Signal/Mach handler chaining needs explicit lifecycle design.
- Fatal mobile crashes should primarily be persisted and uploaded on next launch.
- OOM/watchdog-style terminations require post-mortem inference rather than ordinary signal handling.

Sources:
- Android overview: https://docs.bugsnag.com/platforms/android/
- iOS overview: https://docs.bugsnag.com/platforms/ios/
- C++ exception handler source: https://github.com/bugsnag/bugsnag-cocoa/blob/master/Bugsnag/KSCrash/Source/KSCrash/Recording/Sentry/BSG_KSCrashSentry_CPPException.mm
- Signal-handler coexistence change: https://github.com/bugsnag/bugsnag-cocoa/pull/976

---

### 2.5 Datadog: native crash reporting remains a separate platform capability

Datadog’s current Android Error Tracking documentation supports NDK crash reporting as a separate native capability. Datadog’s Apple SDK has historically integrated PLCrashReporter for native crash collection.

Like Firebase and Bugsnag, Datadog does not provide a Rust-specific language panic containment model. Rust failures either need explicit Rust instrumentation or eventually enter the native crash path.

**Design lesson:** industry SDKs generally separate **language error capture** from **native fatal capture**. Rust should follow the same pattern rather than forcing all failures through one handler.

Sources:
- https://docs.datadoghq.com/real_user_monitoring/application_monitoring/android/error_tracking/
- https://github.com/DataDog/dd-sdk-ios

---

### 2.6 Crashpad itself: minimal in-process interception, external snapshotting where supported

Crashpad’s model remains the reference architecture for robust native fatal capture:

- the crashing process installs the minimum required interception mechanism;
- crash context is communicated to a handler;
- another process snapshots threads/registers/memory and writes the minidump where the platform permits it.

On Linux, Crashpad’s signal handler stores exception information including signal info, context, and thread ID, then requests the crash dump and later restores/reraises as appropriate.

A notable 2026 Android change: Crashpad removed the old standalone handler-binary model as a supported Android deployment path. The supported Android paths now use linker/Java handler startup mechanisms. This matters if building a new Android integration today: do not design around shipping an arbitrary `crashpad_handler` executable beside an APK.

**Design lesson:** use Crashpad as an architectural reference, but on Android integrate through the platform-compatible handler model or reuse the SDK’s existing native crash recorder.

Sources:
- Crashpad handler documentation: https://chromium.googlesource.com/crashpad/crashpad/+/main/handler/crashpad_handler.md
- Linux client implementation: https://chromium.googlesource.com/crashpad/crashpad/+/main/client/crashpad_client_linux.cc
- June 2026 Android handler change: https://chromium.googlesource.com/crashpad/crashpad/+/master

---

## 3. Competitive comparison

| Capability | Sentry | Firebase Crashlytics | Bugsnag | Datadog | Recommended Rust SDK |
|---|---|---|---|---|---|
| Rust panic-aware | Yes, dedicated panic hook | No | No dedicated Rust layer | No dedicated Rust layer | Yes |
| Panic containment (`catch_unwind`) | Not a general Sentry feature | N/A | N/A | N/A | Yes, at SDK boundaries |
| Chains previous language handler | Yes, Rust panic hook | Yes, JVM uncaught handler | Yes/explicit coexistence work | Platform-dependent | Mandatory |
| Native fatal capture | Separate native SDK/backend | NDK Crashpad | Native signal/Mach/C++ handlers | NDK/native crash reporting | Separate backend |
| Out-of-process crash capture | Desktop via Crashpad/native backend | Crashpad-based architecture where applicable | Mostly platform-specific/in-process mobile | Platform-specific | Preferred desktop/server |
| Mobile next-launch upload | Yes depending backend | Yes | Yes | Yes | Yes |
| Pre-unwind language stack | Yes for Rust panic | Not Rust-specific | C++ throw interception on Apple | Not Rust-specific | Yes |
| Duplicate language/native suppression | Backend dependent | Explicit managed/native session logic | Yes in unified event model | Platform dependent | Explicit correlation ID |
| Global handler coexistence emphasis | Panic hook chaining | Default handler chaining | Strong signal-handler work | Platform dependent | First-class requirement |
| Recovery after language panic | Not generally | N/A | N/A | N/A | Selective subsystem recovery |

---

## 4. Rust failure taxonomy

The design must distinguish failures by semantics rather than by their final OS signal.

### 4.1 Recoverable application errors

Use `Result<T, E>`. These are not panics and should not enter the panic/crash pipeline unless explicitly reported as diagnostics.

### 4.2 Unwinding Rust panic

Examples:

```rust
panic!("unexpected state");
assert!(condition);
vec[index]; // bounds panic
unwrap();   // on Err/None
```

With an unwinding panic strategy:

```text
panic
  → panic hook
  → stack unwinding / Drop
  → catch_unwind boundary OR thread termination
```

This is the only category where controlled recovery is generally possible.

### 4.3 Aborting Rust panic

If the final Rust artifact uses `panic=abort`, `catch_unwind` cannot recover the panic. The panic becomes a fatal process termination path.

The panic hook is still the right place to preserve Rust-specific metadata before the fatal transition, subject to runtime configuration.

### 4.4 Rust panic crossing an ABI boundary

A panic must not escape across a non-unwind-compatible foreign ABI.

Rust’s current documentation states that an `extern "C"` function will abort if a Rust panic tries to unwind through it. `catch_unwind` is therefore required when the SDK wants graceful error conversion rather than process abort.

For an SDK, every exported FFI entry point is an isolation boundary.

### 4.5 Foreign exceptions entering Rust

C++ or other foreign unwinding through Rust is a separate hazard. Rust documents foreign exceptions caught with `catch_unwind` as having unspecified outcomes (abort or opaque error).

Therefore:

- do not rely on Rust `catch_unwind` to contain C++ exceptions;
- catch C++ exceptions in a C++ shim before re-entering Rust;
- catch Objective-C exceptions in an Objective-C/Objective-C++ shim where such handling is appropriate;
- never intentionally let a foreign exception propagate through arbitrary Rust frames.

### 4.6 Native memory faults

Examples:

- `SIGSEGV`
- `SIGBUS`
- access violation
- illegal instruction
- corrupted stack
- invalid FFI pointer
- native use-after-free

These are **not recoverable SDK panics**. Capture and terminate normally.

### 4.7 Stack overflow

Stack overflow may prevent normal panic machinery from functioning and can leave very little usable stack for a signal handler. Native crash handling and alternate-signal-stack strategy are required.

### 4.8 OOM

OOM splits into multiple cases:

- allocator failure that aborts;
- OS kill due to memory pressure (especially mobile);
- host-specific low-memory termination.

An OS kill cannot be synchronously trapped. It must be inferred on next launch using launch/session markers plus platform termination information where available.

---

## 5. Design goals

### Required

1. No Rust unwind may escape an SDK-owned FFI boundary.
2. A panic inside the SDK should not crash the host when safe containment is possible.
3. Original panic message, source location, thread, and useful stack must be preserved.
4. Native fatal crashes must still be captured.
5. Existing host crash reporters must not be silently broken.
6. Crash-time code must avoid unsafe allocation/locking/network behavior.
7. Duplicate “Rust panic + SIGABRT” reports must collapse into one logical issue.
8. The SDK must know whether a panic occurred in SDK scope versus host callback scope.
9. The design must work when the SDK is linked into non-Rust applications.
10. Symbols/build IDs must support server-side symbolication.

### Non-goals

- Recovering from `SIGSEGV`, corrupted heap, corrupted stack, or arbitrary memory faults.
- Converting panics into routine control flow.
- Owning every process-global handler without cooperation from the host.
- Guaranteeing interception of OS hard kills.

---

## 6. Proposed architecture

### 6.1 Module layout

```text
rust_failure/
├── boundary.rs             catch_unwind and FFI conversion
├── panic_observer.rs       process-global Rust hook
├── scope.rs                TLS SDK/host attribution
├── panic_snapshot.rs       fixed-size pre-unwind record
├── quarantine.rs           subsystem poison/restart policy
├── thread_guard.rs         SDK thread-root containment
├── async_guard.rs          async task-root containment
├── shared_crash_context.rs bridge to native crash recorder
└── platform/
    ├── android.rs
    ├── apple.rs
    ├── linux.rs
    └── windows.rs

native_crash/
├── provider.h
├── existing_bugsee_backend
├── optional_crashpad_backend
└── no_handler_adapter
```

The Rust panic layer and native crash layer communicate only through a small shared crash-context API.

---

## 7. Layer 1 — FFI/API panic containment

Every Rust function exported to C/Objective-C/Swift/Java/JNI or another host language should execute inside a panic guard.

Example:

```rust
use std::panic::{catch_unwind, AssertUnwindSafe};

#[repr(C)]
pub enum SdkStatus {
    Ok = 0,
    InvalidArgument = 1,
    InternalError = 2,
    Panic = 3,
    Disabled = 4,
}

#[no_mangle]
pub extern "C" fn bugsee_operation(/* ... */) -> SdkStatus {
    ffi_boundary(Operation::CaptureFrame, || {
        operation_impl()?;
        Ok(())
    })
}

fn ffi_boundary<F>(op: Operation, f: F) -> SdkStatus
where
    F: FnOnce() -> Result<(), SdkError>,
{
    let _scope = SdkScope::enter(op);

    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(())) => SdkStatus::Ok,
        Ok(Err(err)) => map_error(err),
        Err(payload) => {
            // The pre-unwind hook has already written the authoritative
            // PanicSnapshot. Do not perform complex diagnostics here.
            std::mem::forget(payload); // avoid a pathological panic-on-Drop
            handle_caught_panic(op)
        }
    }
}
```

### Why not diagnose only inside `catch_unwind`?

Because by the time `catch_unwind` returns, the original frames have been unwound. A backtrace captured there points mostly at the containment boundary.

The authoritative panic snapshot must be created by the panic hook before unwinding.

### Where boundaries are required

- every exported C ABI function;
- every JNI native entry point;
- SDK-owned thread roots;
- worker-pool job roots where a panic would otherwise kill the worker or poison global state;
- asynchronous task roots;
- callback dispatch boundaries when Rust regains control after host code.

---

## 8. Layer 2 — Rust panic observer

Install a global panic hook to capture pre-unwind context.

### 8.1 Hook lifecycle

Use:

```rust
let previous = std::panic::take_hook();
std::panic::set_hook(Box::new(move |info| {
    bugsee_panic_observer(info);
    previous(info);
}));
```

But installation must be treated carefully because the hook is process-global.

Recommended policy:

- install once;
- chain the previously installed hook;
- do not attempt automatic uninstall at normal SDK shutdown;
- provide explicit documentation that later-installed hooks must chain correctly;
- expose diagnostics indicating whether the SDK hook appears active;
- optionally allow the host to disable global panic-hook installation and instead call a public panic-recording adapter.

A naive uninstall is unsafe because another library may have installed a hook after ours. Restoring the old hook can accidentally remove the newer hook — the same class of lifecycle problem Bugsnag encountered with signal handlers.

### 8.2 Hook responsibilities

The hook should do only:

1. inspect SDK TLS scope;
2. capture a monotonically increasing panic ID;
3. capture panic message into a bounded buffer;
4. capture `file`, `line`, `column` into bounded storage;
5. capture thread ID/name if already available without risky allocation;
6. capture a bounded list of raw PCs if the chosen unwinder is known safe enough in this context;
7. update a fixed-size shared panic snapshot;
8. return and invoke the chained hook.

Do **not**:

- make HTTP requests;
- synchronously flush the telemetry client;
- serialize JSON;
- acquire arbitrary SDK mutexes;
- allocate large buffers;
- symbolicate;
- perform filesystem work through high-level abstractions;
- call user callbacks.

This deliberately differs from Sentry Rust’s current panic integration, which reports and flushes directly from the hook. In an embeddable SDK, failure isolation is more important than immediate delivery.

---

## 9. Panic snapshot structure

Use a fixed-size, versioned structure.

```rust
#[repr(C)]
pub struct PanicSnapshot {
    pub version: u32,
    pub state: AtomicU32,
    pub generation: AtomicU64,
    pub panic_id: u64,

    pub timestamp_mono_ns: u64,
    pub thread_id: u64,
    pub sdk_scope_depth: u32,
    pub foreign_callback_depth: u32,
    pub operation_id: u32,

    pub message_len: u16,
    pub file_len: u16,
    pub frame_count: u16,

    pub message: [u8; 512],
    pub file: [u8; 256],
    pub pcs: [usize; 96],
}
```

Suggested states:

```text
EMPTY
WRITING
READY
CONSUMED_NONFATAL
CORRELATED_FATAL
```

Use a double-buffer or generation protocol to avoid the native crash handler reading a partially written snapshot.

---

## 10. SDK scope attribution

A global panic hook sees every Rust panic in the process, including host Rust code and other Rust libraries.

Use TLS scope tracking:

```text
sdk_scope_depth
current_operation_id
foreign_callback_depth
sdk_instance_id
```

### Example

```text
Host Swift/Java/C++
  ↓
Bugsee exported API
  sdk_scope_depth = 1
  ↓
Rust implementation
  ↓
Host callback
  foreign_callback_depth = 1
  sdk_scope considered suspended
  ↓
Host code panics/throws
```

A panic while `sdk_scope_depth > 0 && foreign_callback_depth == 0` is strongly attributable to the SDK execution scope.

A panic while a host callback is active must not automatically be classified as an SDK panic.

### Native crash attribution

Native crashes are harder to attribute. Use multiple signals:

- crashing PC belongs to a Bugsee/Rust SDK module;
- top N frames are inside SDK modules;
- active SDK TLS scope;
- operation ID is active;
- a panic snapshot exists for the same thread and timestamp window.

Classification should be probabilistic:

```text
sdk_origin = confirmed_panic_scope
sdk_origin = likely_native_sdk_frame
sdk_origin = host_or_unknown
```

Do not claim causality solely because the crash happened while an SDK API call was active; memory corruption may have occurred earlier.

---

## 11. Host callback isolation

Calling host code from Rust creates a reverse trust boundary.

### Rules

1. Mark `foreign_callback_depth += 1` before invoking host code.
2. Restore it with an RAII guard.
3. Never permit C++/Objective-C exceptions to cross arbitrary Rust frames.
4. Use platform shims:

```text
Rust
 ↓
C ABI shim
 ↓
C++ try/catch or ObjC @try/@catch where appropriate
 ↓
Host callback
```

5. Decide explicitly whether a host callback failure:
   - is translated to an error;
   - is allowed to terminate the host according to host semantics;
   - is reported as a host error rather than an SDK failure.

`catch_unwind` should only be trusted for Rust-originated unwinding from the same runtime.

---

## 12. SDK-owned thread roots

Every thread spawned by the SDK should have a panic root:

```rust
std::thread::spawn(|| {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        worker_main();
    }));

    if result.is_err() {
        quarantine_worker(WorkerKind::Recorder);
    }
});
```

The panic hook captures diagnostics before unwind. The thread root decides recovery policy.

### Why this matters

An uncaught Rust panic on an SDK-created background thread may terminate only that thread rather than the process. Without a root guard:

- the recorder silently dies;
- queues stop draining;
- later SDK calls may deadlock or accumulate memory;
- the host never sees an obvious fatal crash.

Therefore a background-thread panic should be treated as an SDK health event even when the process survives.

---

## 13. Async task roots

If the SDK uses Tokio or another async runtime, task panics need explicit supervision.

Recommended pattern:

```text
spawn task
  ↓
await/supervise JoinHandle
  ↓
if task panicked:
   correlate panic snapshot
   quarantine task/subsystem
   restart only if policy allows
```

Do not rely on the process-global panic hook alone. The hook records the event but does not repair the lost task.

For critical loops, use a supervisor with restart budgets:

```text
max 1 restart immediately
max 3 restarts / 10 min
then disable subsystem for process lifetime
```

This avoids infinite panic/restart loops.

---

## 14. Quarantine instead of blindly continuing

A caught panic means Rust memory safety may still hold, but **logical invariants may not**.

Example:

```text
update session map
remove old segment
panic while updating index
```

After unwinding, locks may be released, but the session map and index may disagree.

Therefore each subsystem needs an explicit recovery class:

### Class A — stateless/idempotent operation

Safe to return an error and continue.

Examples:
- formatting optional metadata;
- a pure parser with no external mutation.

### Class B — isolated reconstructable subsystem

Quarantine and recreate.

Examples:
- video encoder;
- compression worker;
- transient batching pipeline;
- symbol parser instance.

### Class C — core shared state

Disable or rebuild the entire SDK instance.

Examples:
- global session state corrupted mid-mutation;
- cross-thread scheduler invariants broken;
- shared storage index panic.

### Class D — uncertain safety

Disable SDK functionality for process lifetime.

The default policy should be conservative.

---

## 15. Layer 3 — native fatal crash capture

`catch_unwind` does not cover native fatal failures.

Required categories include:

- `SIGSEGV`
- `SIGBUS`
- `SIGILL`
- `SIGFPE`
- `SIGABRT`
- stack overflow
- Windows exceptions / fail-fast paths
- Mach exceptions
- native C/C++ crashes
- aborting Rust panic
- allocator aborts

### Core rule

**Never attempt to recover from these.**

The fatal handler exists only to preserve diagnostic state and then allow normal process termination semantics.

---

## 16. Reuse the existing native crash recorder when embedding into Bugsee

For Bugsee, installing an independent “Rust crash handler” would be the wrong architecture.

Instead:

```text
Existing Bugsee native crash recorder
      ▲
      │ reads
SharedCrashContext
      ▲
      │ writes
Rust panic observer / SDK scope tracker
```

Benefits:

- only one owner of POSIX/Mach/SEH crash interception;
- no new conflict with Firebase/Sentry/Bugsnag;
- same session/breadcrumb/video correlation as existing Bugsee crashes;
- no duplicate crash payload format;
- existing dSYM/ELF symbol workflow remains usable;
- Rust panic metadata becomes an enrichment channel.

Create a narrow C ABI:

```c
void bugsee_rust_set_panic_snapshot(const bugsee_rust_panic_snapshot_t *snapshot);
void bugsee_rust_mark_panic_caught(uint64_t panic_id);
void bugsee_rust_mark_sdk_scope(uint32_t operation_id, int entering);
```

Ideally the actual storage is shared/preallocated so the panic hook does not need to call complex native code.

---

## 17. Standalone native backend option

For a generic Rust SDK distributed outside Bugsee’s existing mobile crash stack, expose a provider abstraction:

```rust
trait NativeCrashProvider {
    fn initialize(&self, shared: &'static SharedCrashContext) -> Result<()>;
    fn supports_nonfatal_snapshot(&self) -> bool;
    fn request_nonfatal_snapshot(&self, reason: SnapshotReason) -> Result<()>;
}
```

Providers:

```text
ExistingHostProvider     no handlers; host integrates shared metadata
BugseeNativeProvider     existing Bugsee crash implementation
CrashpadProvider         desktop/server where deployable
InProcessMobileProvider  iOS/Android specialized recorder
NoopProvider             panic-only deployments
```

The API should allow a host that already uses Crashlytics/Sentry/Bugsnag to opt out of SDK-owned native handlers.

---

## 18. Crash-time safety model

### Signal/Mach/exception interception path

The initial fatal handler should perform the minimum possible work:

```text
capture signal/exception code
capture fault address
capture ucontext/register context
capture crashing thread ID
read preallocated SharedCrashContext
persist via crash-safe primitive or notify handler
chain/reraise/terminate
```

Avoid:

- heap allocation;
- arbitrary locks;
- normal logger;
- JSON;
- symbolication;
- Objective-C runtime calls from POSIX signal context;
- network;
- high-level filesystem APIs.

### Allocation strategy

If in-process crash event construction is unavoidable, adopt a crash-only arena/bump allocator reserved at initialization. Sentry’s own signal-handling guidance recommends switching away from normal allocator behavior after entering crash state because allocator locks may already be held.

---

## 19. Platform-specific design

### 19.1 Android

Recommended:

- reuse existing Bugsee NDK signal crash handling;
- record Rust panic metadata in shared fixed memory;
- native symbols identified by GNU build ID;
- persist crash record to app-private storage;
- upload/process on next launch;
- use `ApplicationExitInfo` as a secondary source for missed native crashes/ANRs/termination classification where appropriate.

If adopting Crashpad directly in a new implementation, account for the 2026 Crashpad Android change: supported startup is through Android-compatible Java/linker handler mechanisms, not a standalone sidecar executable shipped beside the app.

### 19.2 iOS / tvOS

Recommended:

- reuse current Bugsee Mach/signal crash pipeline;
- do not depend on an external helper process;
- persist minimal crash artifact in-process;
- upload on next launch;
- use launch-state heuristics/platform termination information for OS kills/OOM/watchdog cases;
- add Rust panic snapshot as metadata to the native crash record.

### 19.3 macOS

Preferred when deployment allows:

- out-of-process Crashpad/native snapshotter;
- Mach exception based handling;
- shared panic metadata exposed as annotations or custom stream.

For sandboxed/App Store deployments, choose a compatible in-process or sandbox-aware backend.

### 19.4 Linux

Preferred:

- out-of-process snapshotter where allowed;
- POSIX `sigaction(..., SA_SIGINFO, ...)` interception;
- alternate signal stacks for SDK-created threads;
- careful chaining/reraising of prior handlers;
- handle ptrace/Yama/container restrictions.

A library cannot reliably install an alternate signal stack on arbitrary host-created threads unless it instruments thread creation or the host cooperates.

### 19.5 Windows

Preferred:

- VEH/SEH observation plus an external minidump helper when feasible;
- preserve `EXCEPTION_POINTERS`;
- avoid doing heavy dump work in corrupted process state;
- retain PDB identity and symbols server-side.

---

## 20. Panic + native crash correlation

This is essential for `panic=abort` and any panic that ultimately results in `SIGABRT`.

### Flow

```text
panic!()
  │
  ▼
panic hook writes:
  panic_id = 9182
  tid = 42
  timestamp = T
  message/location/PCs
  state = READY
  │
  ▼
Rust aborts
  │
  ▼
SIGABRT/native crash handler
  │
  ▼
reads SharedCrashContext
  │
  ├── same tid
  ├── snapshot READY
  └── timestamp close to crash
  │
  ▼
classify native fatal as RUST_PANIC_FATAL
attach panic_id=9182
```

Backend dedup key:

```text
(process_session_id, panic_id)
```

Emit one event containing:

```json
{
  "mechanism": "rust_panic",
  "handled": false,
  "panic": {
    "message": "index out of bounds",
    "file": "src/recorder.rs",
    "line": 218
  },
  "native_exception": {
    "signal": "SIGABRT"
  },
  "threads": "...",
  "breadcrumbs": "..."
}
```

Do not create separate issues named “Rust panic” and “SIGABRT”.

---

## 21. Caught panic flow

```text
panic!()
 ↓
panic hook captures original context
 ↓
unwind
 ↓
catch_unwind
 ↓
mark snapshot CONSUMED_NONFATAL
 ↓
quarantine/restart/disable affected subsystem
 ↓
return SDK_E_PANIC to caller
 ↓
normal telemetry worker later serializes and uploads panic event
```

The hook never needs to send the report itself.

---

## 22. Optional nonfatal process snapshot

For high-value QA/debug scenarios, a caught panic can trigger a nonfatal native process snapshot after unwinding has safely reached the boundary.

This is analogous to Crashpad’s “dump without crash” concept.

Use selectively:

```text
first SDK panic in session       → full nonfatal snapshot
repeated same fingerprint        → lightweight event only
QA/debug builds                  → snapshots enabled
production default               → lightweight unless sampled
```

Do not request the heavy snapshot directly from the panic hook. Request it after `catch_unwind` returns and after the affected subsystem is isolated.

---

## 23. Panic build strategy

### For distributed final artifacts (`staticlib`, `cdylib`)

Build with unwind support when graceful containment is a requirement:

```toml
[profile.release]
panic = "unwind"
```

### For `rlib` or source-integrated distribution

The final application’s panic strategy may control behavior. Document that:

- `panic=unwind` enables containment;
- `panic=abort` disables `catch_unwind` recovery but still benefits from panic metadata + native crash correlation.

The SDK must operate correctly in both modes.

### Do not assume every panic can unwind

Even in an unwind build:

- double panic can abort;
- panic in destructor during unwinding can abort;
- stack overflow may bypass useful unwind behavior;
- foreign-runtime unwind behavior is not reliably catchable.

Therefore native crash capture remains mandatory.

---

## 24. Symbols and stack representation

### Panic event

Prefer storing raw PCs in the panic snapshot rather than formatted symbol strings.

Reasons:

- lower panic-time work;
- no symbol resolver locks;
- smaller records;
- server-side symbolication can use exact release symbols;
- inline frames can be reconstructed properly.

### Build identity

Preserve:

- ELF GNU build ID on Android/Linux;
- Mach-O UUID + dSYM on Apple platforms;
- PDB GUID/age on Windows;
- SDK version and Rust crate build identity.

A release pipeline should fail or warn if native symbols for the SDK artifact are missing.

---

## 25. Handler coexistence policy

This is one of the most important sections for an embeddable SDK.

### 25.1 Rust panic hook

- chain previous hook;
- install once;
- do not naively restore at shutdown;
- make installation configurable;
- tolerate another SDK replacing the hook after initialization;
- expose an integration API for hosts with their own Rust hook.

### 25.2 POSIX signals

If Bugsee already owns the native handler, Rust installs none.

For standalone mode:

- save previous actions;
- chain/reraise according to signal semantics;
- avoid destructive uninstall ordering;
- document incompatibility with handlers that do not chain;
- offer `native_crash_capture = false`.

### 25.3 Apple Mach exceptions

Mach exception ports are global/process-level resources with more complex chaining semantics. Do not add a second independent handler when an existing Bugsee crash pipeline is already present.

### 25.4 Android JVM

If a Java/Kotlin wrapper catches a propagated Rust status and converts it into an exception, it remains a handled SDK failure unless intentionally rethrown.

Do not manipulate Crashlytics/Sentry JVM exception handlers from Rust.

---

## 26. Data model

Suggested event schema:

```json
{
  "type": "sdk_failure",
  "failure_kind": "rust_panic_caught | rust_panic_fatal | native_crash",
  "handled": true,
  "sdk": {
    "name": "bugsee",
    "version": "x.y.z",
    "instance_id": "...",
    "operation": "capture_frame"
  },
  "rust": {
    "panic_id": 9182,
    "message": "...",
    "file": "...",
    "line": 218,
    "column": 17,
    "thread_id": 42,
    "raw_pcs": []
  },
  "native": {
    "signal": null,
    "code": null,
    "fault_address": null,
    "minidump_id": null
  },
  "recovery": {
    "action": "none | restart_subsystem | disable_subsystem | disable_sdk",
    "subsystem": "recorder"
  }
}
```

For a correlated fatal panic:

```text
failure_kind = rust_panic_fatal
handled = false
native.signal = SIGABRT
rust.panic_id present
```

---

## 27. Event fingerprinting

For Rust panics, group primarily by:

```text
panic source module/function
+ source line (normalized carefully)
+ top in-app Rust frames
+ panic message class
```

Avoid grouping solely by raw panic string when it includes IDs, indices, URLs, or dynamic values.

For caught and fatal manifestations of the same panic site, use the same base fingerprint but preserve `handled` and recovery outcome as event dimensions.

---

## 28. Privacy and PII

Panic messages can contain user data.

Examples:

```text
panic!("invalid token: {token}")
panic!("failed parsing URL {url}")
```

Therefore:

- panic message capture should follow the SDK’s normal PII policy;
- support server-side scrubbing;
- optionally truncate/hash message portions;
- do not capture arbitrary heap around panic by default;
- make full minidumps opt-in or tightly controlled because they may contain sensitive process memory.

For production mobile SDK defaults, prefer stack/register/context capture over unrestricted full-memory dumps.

---

## 29. Performance profile

### Normal path

Desired overhead:

- one TLS depth increment/decrement per public SDK boundary;
- one `catch_unwind` frame; Rust’s unwind machinery has minimal normal-path cost relative to actually panicking;
- no stack capture;
- no allocation solely for panic instrumentation.

### Panic path

Allowed because rare:

- bounded message copy;
- bounded raw PC capture;
- atomic shared-state update;
- subsystem quarantine.

### Crash path

Optimize for reliability rather than speed, but minimize in-process work.

---

## 30. Failure modes of the crash reporter itself

The crash system must defend against recursive failure.

Use a global atomic crash state:

```text
NORMAL
PANICKING
HANDLING_FATAL
SECONDARY_FAILURE
```

If a second fatal failure occurs while already handling a fatal crash:

- do not recursively construct another report;
- write only a tiny emergency marker if possible;
- immediately chain/reraise/terminate.

Similarly, if the Rust panic observer itself panics, the chained/default runtime behavior must still be allowed to terminate the process instead of looping.

---

## 31. Testing matrix

### Rust panic tests

- panic in exported API;
- panic in SDK-created thread;
- panic in async task;
- panic while holding Rust `Mutex`;
- panic payload `&str`;
- panic payload `String`;
- custom panic payload;
- payload with panicking `Drop`;
- double panic during `Drop`;
- panic inside panic observer defensive test;
- host installs panic hook before SDK;
- host installs panic hook after SDK;
- multiple SDK initialization calls.

### FFI tests

- Rust panic from C caller;
- Rust panic from ObjC/Swift caller;
- Rust panic from JNI;
- C++ exception thrown by host callback;
- Objective-C exception from host callback;
- callback reentrancy;
- nested SDK boundary calls.

### Native fatal tests

- SIGSEGV/null dereference;
- SIGBUS where platform supports;
- SIGILL;
- abort();
- stack overflow;
- corrupted stack synthetic test;
- panic=abort;
- allocator abort/OOM synthetic path.

### Correlation tests

- panic=abort → exactly one fatal event;
- caught panic → no native crash duplicate;
- stale old panic snapshot + unrelated SIGABRT → not correlated;
- panic on thread A + crash on thread B → not automatically correlated;
- recursive panic while crash handling.

### Coexistence tests

Install alongside:

- Sentry;
- Firebase Crashlytics;
- Bugsnag;
- PLCrashReporter/KSCrash-style handler;
- custom application signal handler;
- custom Rust panic hook.

Test both initialization orders.

### Recovery tests

- reconstructable subsystem panic → clean restart;
- repeated panic → restart budget exhausted → subsystem disabled;
- core-state panic → SDK disabled safely;
- host remains usable after caught panic.

---

## 32. Rollout plan

### Phase 1 — Rust panic observability

Implement:

- global chained panic hook;
- fixed-size panic snapshot;
- TLS SDK scope attribution;
- panic events for SDK-owned threads.

No recovery changes yet.

### Phase 2 — FFI containment

Wrap all exported Rust boundaries with `catch_unwind`.

Return a dedicated internal panic status and map it appropriately in Java/ObjC/Swift wrappers.

### Phase 3 — Quarantine model

Classify subsystems and implement restart/disable policies.

Start conservatively:

- encoder/compressor worker → restart allowed;
- global recorder/core state → disable SDK instance.

### Phase 4 — Native crash correlation

Expose Rust panic snapshot to existing native Bugsee crash recorder.

Deduplicate panic + `SIGABRT`.

### Phase 5 — Rich nonfatal snapshots

Optional sampled “dump without crash” for QA/internal builds or first panic per session.

### Phase 6 — Standalone crash-provider abstraction

Only if the Rust core will be distributed independently of Bugsee’s existing native crash stack.

---

## 33. Recommended API surface

Internal Rust:

```rust
pub fn install_panic_observer(config: PanicObserverConfig) -> Result<()>;

pub fn guard_ffi<T>(
    operation: Operation,
    f: impl FnOnce() -> Result<T, SdkError>,
) -> Result<T, SdkBoundaryError>;

pub fn spawn_guarded(
    subsystem: Subsystem,
    f: impl FnOnce() + Send + 'static,
) -> JoinHandle<()>;
```

Native bridge:

```c
typedef struct bugsee_rust_panic_snapshot bugsee_rust_panic_snapshot_t;

const bugsee_rust_panic_snapshot_t *
bugsee_rust_current_panic_snapshot(void);

void bugsee_rust_mark_panic_consumed(uint64_t panic_id, int handled);
```

Configuration:

```text
rustPanicCaptureEnabled = true
rustPanicContainmentEnabled = true
nativeCrashCaptureMode = existing | standalone | disabled
panicRecoveryPolicy = conservative
nonfatalSnapshotMode = off | first | sampled | always
```

For Bugsee’s integrated SDK, these should mostly be internal rather than public unless there is a strong host-coexistence reason to expose them.

---

## 34. Recommended deviations from competitor implementations

### Compared with Sentry Rust

Keep:

- process-global panic hook;
- previous-hook chaining;
- pre-unwind capture.

Change:

- do not flush networking from panic hook;
- do not build a heavyweight event in the hook;
- add SDK-scope attribution;
- add `catch_unwind` isolation boundaries;
- add subsystem quarantine;
- correlate fatal native abort with panic metadata.

### Compared with Firebase Crashlytics

Keep:

- handler forwarding/chaining philosophy;
- native crash / managed-language deduplication;
- local persistence and next-launch processing;
- build-ID-based symbol workflow.

Add:

- Rust-specific panic metadata;
- recoverable panic containment;
- per-subsystem health/restart policy.

### Compared with Bugsnag

Keep:

- language-runtime interception before the context is lost;
- careful global handler coexistence;
- next-launch mobile fatal delivery;
- broad termination classification.

Add:

- Rust-specific FFI containment;
- explicit caught-vs-fatal panic correlation;
- shared panic snapshot consumed by the existing native recorder.

---

## 35. Final recommendation for Bugsee

The most robust approach is **not** to introduce “a Rust crash reporter.” Instead, add a Rust-aware failure layer above Bugsee’s existing platform crash machinery.

### Final architecture

```text
                    ┌──────────────────────────────┐
                    │       Host Application       │
                    └──────────────┬───────────────┘
                                   │
                        Bugsee public API/FFI
                                   │
              ┌────────────────────▼────────────────────┐
              │ Rust Boundary Guard                    │
              │ catch_unwind + scope + operation ID    │
              └───────────────┬─────────────────────────┘
                              │
                ┌─────────────┴─────────────┐
                │                           │
             success                      panic
                │                           │
                │                           ▼
                │               ┌───────────────────────┐
                │               │ Rust Panic Observer   │
                │               │ fixed-size snapshot   │
                │               └───────────┬───────────┘
                │                           │
                │                    unwind or abort
                │                           │
                │          ┌────────────────┴───────────────┐
                │          │                                │
                │     catch_unwind                      fatal abort
                │          │                                │
                │          ▼                                ▼
                │  quarantine/recover         Existing Bugsee native crash
                │  nonfatal panic event        recorder reads snapshot
                │                                           │
                └───────────────────────────────┬───────────┘
                                                ▼
                                  unified Bugsee failure event
                                  + session/video/breadcrumbs
                                  + server symbolication
```

### The critical implementation decisions

1. **Compile Rust SDK artifacts with unwind support where recovery is desired.**
2. **Put `catch_unwind` around every externally callable SDK boundary and SDK-owned execution root.**
3. **Install a chained global panic observer once, but keep it minimal.**
4. **Capture original panic context before unwind into fixed/preallocated storage.**
5. **Never allow host callbacks to be misattributed as SDK panics.**
6. **Do not continue blindly after a caught panic; quarantine or rebuild the affected subsystem.**
7. **Do not install a second native crash handler when Bugsee already owns native crash capture.**
8. **Merge Rust panic metadata into the existing native crash artifact.**
9. **Correlate panic + native abort into one event.**
10. **Treat process-global handler coexistence as part of the public compatibility contract.**

This provides more resilience than Sentry’s Rust panic hook alone, more Rust-specific context than Firebase/Bugsnag native crash capture alone, and avoids the handler-conflict risk of adding an independent Rust-native crash stack inside an already instrumented SDK.

---

## 36. Primary references

1. Rust `catch_unwind` documentation  
   https://doc.rust-lang.org/std/panic/fn.catch_unwind.html

2. Rust panic reference  
   https://doc.rust-lang.org/reference/panic.html

3. Rust standard panic module  
   https://doc.rust-lang.org/std/panic/index.html

4. Sentry Rust panic integration source  
   https://raw.githubusercontent.com/getsentry/sentry-rust/master/sentry-panic/src/lib.rs

5. Sentry Rust panic integration README  
   https://github.com/getsentry/sentry-rust/tree/master/sentry-panic

6. Sentry Native backends  
   https://docs.sentry.io/platforms/native/configuration/backends/

7. Sentry Native backend tradeoffs  
   https://docs.sentry.io/platforms/native/advanced-usage/backend-tradeoffs/

8. Sentry Native signal handling  
   https://docs.sentry.io/platforms/native/advanced-usage/signal-handling/

9. Sentry experimental native out-of-process backend  
   https://docs.sentry.io/platforms/native/configuration/backends/native/

10. Firebase Crashlytics Android uncaught exception handler  
    https://github.com/firebase/firebase-android-sdk/blob/main/firebase-crashlytics/src/main/java/com/google/firebase/crashlytics/internal/common/CrashlyticsUncaughtExceptionHandler.java

11. Firebase Android repository submodules / Crashpad dependency  
    https://github.com/firebase/firebase-android-sdk/blob/main/.gitmodules

12. Firebase Crashlytics NDK changelog  
    https://github.com/firebase/firebase-android-sdk/blob/main/firebase-crashlytics-ndk/CHANGELOG.md

13. Firebase Crashlytics NDK setup  
    https://firebase.google.com/docs/crashlytics/android/get-started-ndk

14. Bugsnag Android  
    https://docs.bugsnag.com/platforms/android/

15. Bugsnag iOS  
    https://docs.bugsnag.com/platforms/ios/

16. Bugsnag/KSCrash C++ exception handler  
    https://github.com/bugsnag/bugsnag-cocoa/blob/master/Bugsnag/KSCrash/Source/KSCrash/Recording/Sentry/BSG_KSCrashSentry_CPPException.mm

17. Bugsnag signal-handler coexistence fix  
    https://github.com/bugsnag/bugsnag-cocoa/pull/976

18. Datadog Android native error tracking  
    https://docs.datadoghq.com/real_user_monitoring/application_monitoring/android/error_tracking/

19. Crashpad handler documentation  
    https://chromium.googlesource.com/crashpad/crashpad/+/main/handler/crashpad_handler.md

20. Crashpad Linux client implementation  
    https://chromium.googlesource.com/crashpad/crashpad/+/main/client/crashpad_client_linux.cc

21. Crashpad repository / June 2026 Android handler change  
    https://chromium.googlesource.com/crashpad/crashpad/+/master

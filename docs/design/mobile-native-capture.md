# Mobile native crash capture (Apple mobile + Android) — design

**Status:** Proposal for maintainer review. No code. Tracks issue #4 ("Mobile native crash capture: research, decisions and deferred plan").
**Base:** `main` @ `a1564ae`. The `CrashBackend` contract is read from `origin/refactor/crash-backend` (crate `bugsee-native-api`, commit `6189ed6`, not merged). The out-of-process design is read from `origin/docs/issue-17-oop-monitor-design` (`docs/design/out-of-process-monitor.md`, not merged).

**Provenance legend.**
- **[V]** verified in this repo, in the Bugsee backend worker source, or in the local sources of Bugsee's own iOS and Android SDKs (described generically here, per #4's rule that private repositories are not named, linked or quoted).
- **[U]** from memory, from the issue text, or from general platform knowledge; not verified. Confirm before relying on it.
- No third-party source (PLCrashReporter, Crashpad, ART, bionic, Apple headers) was available locally. Everything about them is **[U]** unless it was observed through Bugsee's own SDK code.

---

## 1. Summary

1. **The issue's "current state" is out of date in a way that matters.** The in-process handler is no longer built on the `crash-handler` crate on Unix; it is our own `sigaction` handler (commit `a1564ae`). Its Apple code (module map with `LC_UUID`, frame-pointer walk, `ucontext` decoding) already exists, and only a `cfg` on `mod imp` keeps iOS out. Section 2.1 has the full code-versus-issue table.
2. **Android is closer to shippable than the issue says, but unproven.** The handler is cross-compiled and linted for `aarch64-linux-android` only and has never run on a device or emulator. Reading it shows three Android-specific problems (section 2.3): it can replace a thread's `sigaltstack`, its module map is frozen at install time, and **the C ABI cannot pass a data directory**, so an FFI host depends on the default resolution. Open PR #24 (#9) falls back to a private directory under the system temp directory, which on Android may be app cache storage that the OS can purge (unverified [U]); a host-supplied files directory is the durable path (section 2.3, item 6). Also, the C ABI already arms the Android handler today, because `native_crash_capture` defaults to `true`.
3. **Recommendation in one paragraph.** Keep the agreed direction (PLCrashReporter for Apple mobile, Crashpad for Android) as the long-term engines behind `CrashBackend`, but start with neither. Start with (a) the host-integration gaps that block any mobile capture (data directory, native-capture mode, host coexistence policy); (b) putting the existing in-process handler under real test on an Android emulator and an iOS simulator, which also produces the crash matrix every later backend is judged against; then (c) a PLCrashReporter spike for Apple and a go/no-go for Crashpad on Android, based on measured gaps. The first thing to settle is **who the mobile consumer is** (decision D1), because a Rust library inside an app that already has a crash reporter wants native capture off.
4. **Defer:** hang/watchdog, OOM and jetsam inference, MetricKit, `ApplicationExitInfo`, C++ `__cxa_throw` hooks, Mach exceptions, tvOS/visionOS/watchOS device work, and out-of-process capture on mobile.

---

## 2. Current state (what the code does today)

### 2.1 Code versus the issue's claims

| Claim in #4 / docs | What the code does | Verdict |
|---|---|---|
| iOS: `bugsee-native` compiles to a stub because `crash-handler` has no iOS backend; `crash-context 0.8.0` defines `CrashContext` for macOS only. | The stub exists and `install()` returns `Unsupported` **[V]** (`crates/bugsee-native/src/lib.rs`, `mod unsupported`). The reason is stale: `crash-handler` is now a **Windows-only** dependency **[V]** (`crates/bugsee-native/Cargo.toml`); Linux, Android and macOS use the in-repo `posix` module. Only the `cfg` on `mod imp` (`linux`, `android`, `macos`, `windows`) keeps iOS on the stub. | Partly stale. |
| The crate's module doc says the handler "is built on the `crash-handler` crate". | Still says so (lib.rs, "Supported platforms"), contradicting the Unix code and the `posix` module doc. | Doc bug; fix with the first mobile PR. |
| Android: "compiles and is lint-checked in CI for `aarch64-linux-android`. Never run." | Confirmed **[V]** (`.github/workflows/ci.yml`, `android` job: clippy and a cdylib build; one ABI; NDK from the runner image; no emulator job). | Accurate. |
| "Apple mobile has no fatal-crash capture today." | Accurate for the shipped build. The Apple paths (`dyld` module snapshot, `LC_UUID`, `mach_vm_read_overwrite` frame-pointer walk, PAC stripping, `DarwinUcontext`) are `cfg(target_vendor = "apple")`, so they would compile for iOS if `mod imp` were widened; none of it has run on iOS. | Accurate; there is a cheap path (section 5.1). |
| CI builds core, native and the C ABI for Android and the two iOS targets (compile only). | Confirmed **[V]**: `aarch64-apple-ios` and `aarch64-apple-ios-sim`. On iOS what compiles is the stub, so the iOS job says nothing about the handler. | Accurate but weaker than it sounds. |
| PROGRESS.md: "Apple `on_crash` decodes `EXC_SOFT_SIGNAL`/`EXC_BAD_ACCESS`"; "`crash-handler` resolves a private Apple libproc symbol (App-Store sensitivity)". | `bugsee-native` has no Mach exception code (one comment mentions `EXC_BAD_ACCESS`), and `crash-handler` is Windows-only, so neither statement describes the current macOS path. | Stale; fix PROGRESS.md. |
| Open decision 6: our recovery runs on a worker thread, so "finish export and upload before host code runs" needs a design. | Next-launch recovery builds the report from `crash.info`, `crash.modules` and an optional `crash.minidump` **[V]** (`bugsee-core/src/recovery.rs`). Nothing runs at crash time except the marker write. | Accurate. |

### 2.2 What `bugsee-native` does per target today

- **Linux, Android, macOS (the `posix` module) [V].** Installs `sigaction` handlers for `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGILL`, `SIGSEGV`, `SIGTRAP` over whatever is there, with `SA_ONSTACK | SA_SIGINFO`. On a fault it writes a marker header (signal, code, address, pc, time), calls the previous handler itself, and decides whether the host recovered (marker deleted, SDK stays armed) or the process will die (frames appended). A 5-second `SIGALRM` budget stops the handler turning a crash into a hang. Fork children rebind or clear the marker path. This is what makes it safe next to a runtime that faults on purpose (`a1564ae`, `tests/crash_coexist.rs`).
- **Frames.** Linux/Android: `backtrace::trace_unsynchronized` on the crashing thread, cut to start at the interrupted PC. This is not async-signal-safe; the header is written first so a re-fault loses only the frames. Apple: PC plus a frame-pointer chain read with `mach_vm_read_overwrite`, PAC-stripped. Both capture **the crashing thread only**, with no registers beyond PC/FP/SP and no other threads.
- **Module map.** Written once at `install()` to `crash.modules` as `base, size, code_id, name`, using `dl_iterate_phdr` (GNU build-id from `PT_NOTE`) or `_dyld_image_*` (`LC_UUID`). Recovery joins crash-time PCs against it to emit `reladdr`. A PC outside every snapshotted module is skipped rather than misattributed **[V]** (DESIGN.md, "Native frame capture (as built)").
- **Report shape.** Recovery emits the "thin native variant": `source_*`, `signal`, `modules[]` (`base_addr`, `end_addr`, `code_id`, `filename`), `frames[]` (`addr`, `module`, `reladdr`), a client signature, and a minidump attached as an extra file if one exists **[V]** (`recovery.rs::build_native`; the minidump's presence is also what selects this variant, see 2.3 item 8). No threads array, no registers, no Java frames.
- **Windows** uses `crash-handler` in the unhandled-exception filter; not relevant here.
- **iOS, tvOS, watchOS, visionOS** get `mod unsupported`: `install()` returns `ErrorKind::Unsupported`, which the facade ignores (`bugsee/src/api.rs`). On these targets the SDK still captures panics and handled errors; it has no fatal-signal capture.

### 2.3 Android-specific observations from reading the handler (not from running it)

1. **`sigaltstack` is not left alone.** `ensure_alt_stack` keeps the installing thread's alternate stack only if it is at least 64 KiB; otherwise it maps a 128 KiB one and installs it (restored on uninstall) **[V]**. Bugsee's Android SDK states the opposite rule as hard-won: never replace ART's `sigaltstack` **[V]**. ART's per-thread stacks are small **[U]**, so on a Java thread this would likely replace ART's. It is also per-thread: only the installing thread is covered.
2. **Registration is plain `libc::sigaction`.** Whether that call reaches ART's sigchain, so ART orders and chains us, depends on symbol interposition in the app's namespace **[U]**. The chaining logic fits a "previous handler is a real handler" model, but only a device run shows that ART's own `SIGSEGV` use (implicit null checks, stack-overflow probes) still works and that we do not report faults ART recovers.
3. **The module map is an install-time snapshot.** Android apps routinely call `System.loadLibrary` after SDK start; frames in those libraries are dropped by design **[V]**. Bugsee's Android SDK keeps its own module table in the handler for the equivalent problem **[V]**.
4. **Frames are native only.** `_Unwind_Backtrace` cannot see Java frames and may not unwind through ART-generated code **[U]**. Bugsee's Android SDK captures Java stacks inside its handler via JNI **[V]**; the Rust SDK has no JNI surface.
5. **Only one ABI is built in CI** (`aarch64`). `armeabi-v7a`, `x86` and `x86_64` have `cfg` branches in the handler (pc/sp extraction) that CI never compiles **[V]**. Bugsee's Android SDK ships all four **[V]**.
6. **Data directory: the Rust API can set it, the C ABI cannot, and only iOS gets a sandbox-correct default.** `LaunchOptions::data_dir` exists **[V]** (`options.rs`), so a Rust-API host (scenario A) can point it at the app sandbox. `bugsee-ffi` exposes only `bugsee_launch(token)` and `bugsee_launch_with_endpoint(token, endpoint)` **[V]**, so an FFI host cannot pass one. On `main` such a host gets `std::env::temp_dir()/bugsee/<token>`, and an unwritable directory makes `launch_with` return an error (`create_dir_all(&data_dir)?`) **[V]**. Open PR #24 (#9), read from `origin/fix/issue-9` (`resolve_data_dir`, `platform_data_base`, `DataDirSource`, `StorageStatus::may_be_ephemeral` in `options.rs`/`api.rs`), changes this **[V]**. Order: explicit `data_dir` (final; if unusable, memory mode), then `BUGSEE_DATA_DIR` (also final), then a platform default, then `DataDirSource::TempFallback` = `<temp>/bugsee-<uid>/<token>` (private, `0700`), each probed for writability; memory mode only when **all** candidates fail. The handler is installed when a directory was found (`options.native_crash_capture && persistent`), so `TempFallback` arms it. The platform default is `LOCALAPPDATA` on Windows, `$HOME/Library/Application Support` on Apple targets, and `$XDG_DATA_HOME` or `$HOME/.local/share` on every other Unix. **iOS** therefore gets the app container. **Android** app processes normally have neither `XDG_DATA_HOME` nor a useful `HOME` **[U]**, so a host that passes nothing lands on `TempFallback` under `std::env::temp_dir()`. What that directory is on Android is **unverified [U]**: Rust reads `$TMPDIR`, and whether the app's cache directory is exported through it (so the fallback is writable app storage) or it falls to `/data/local/tmp` (unwritable, so memory mode) must be checked on a device in Phase 1. `may_be_ephemeral()` reports `TempFallback`, because a cache directory can be purged by the OS or by the user, and a marker lost that way means one crash goes unreported, not that the SDK fails.
7. **The C ABI already arms the Android handler.** `bugsee_launch` builds `LaunchOptions::new(token)` and never clears `native_crash_capture`, whose default is `true` **[V]** (`options.rs`, `api.rs`). Android is not a stub target (`mod imp` includes `target_os = "android"`), so a successful C ABI launch on Android installs the unproven handler today. Making the C ABI default off would be a behaviour change for existing FFI hosts.
8. **The recovery classifier and the backend route.** `recovery.rs::build_report` computes `has_native = minidump.is_some() || crash_info.is_some()` **[V]**; with `has_native` and anything other than a fresh panic snapshot plus `signal=SIGABRT` it builds the native variant. So a `crash.minidump` is not an "extra file" attached to whatever report would have been built; its presence *selects* the native variant (see Phases 3 and 4). **The `crash.info` grammar recovery parses [V]** (`parse_crash_info`, `is_fresh`, `CORRELATION_WINDOW_MS`): one `key=value` pair per line, split at the first `=`, keys trimmed, unknown keys and lines without `=` ignored. `signal=<i32>` is **numeric** (`6`, not `SIGABRT`; anything unparseable becomes 0), `code=<i32>`, `address=<text>` (or `addr=`; default `0x0`), `time=<epoch milliseconds>` (`CLOCK_REALTIME`, as the in-process handler writes it; unparseable becomes 0) and repeated `frame=<hex pc>` lines (optional `0x`). Correlation into one managed event requires all of: a fresh `panic.info`, `signal=6`, `time` **nonzero**, the panic snapshot's own timestamp nonzero, and `|time - panic.timestamp| <= 10_000` ms. A dump plus `signal=6` with `time` missing, zero or outside that window still takes `build_native`; with no `crash.info` at all it reports `signal.number` 0. The backend routes `sdk.type == 'rust'` reports through `crash/rust.py` and accepts `minidumpFile` **[V]** (worker `jobs/bundle.py`). Whether `rust.py` behaves for `platform_type` `android` or `ios` is untested here **[U]**: its docstring says "linux / windows / macos".

### 2.4 The `CrashBackend` abstraction (PR #23, unmerged)

`bugsee-native-api` is a dependency-free crate: `BackendConfig` (`#[non_exhaustive]`, holding `crash_info_path`, with `session_dir()`), `BackendGuard: Send` (drop uninstalls), and `CrashBackend { name(), install(&BackendConfig) -> io::Result<Box<dyn BackendGuard>> }` **[V]**. `bugsee-native` implements it for the in-process handler, adds `default_backend()` and `install_with()`, and keeps `bugsee_native::install(path)` as the compatibility entry. I read the code and diff stat (7 files, +247/-24); I did not rerun its tests.

What mobile needs that the contract does not yet carry:
- The **host-coexistence policy** (D4), expressed through the existing `LaunchOptions::native_crash_capture` rather than a second knob.
- **Backend-private parameters** (Crashpad handler path, launch method and database directory; PLCrashReporter base path). `non_exhaustive` allows adding them; the shape is decision D8.
- **A statement of what the backend leaves behind.** Today recovery reads `crash.info`, `crash.modules` and optionally `crash.minidump`. A PLCrashReporter or Crashpad backend writes its own artifact (`.plcrash`, `.dmp`). The contract should say whether the backend converts it into the existing trio, or recovery learns a new input (D2). **Constraint:** because `crash.minidump` or `crash.info` alone selects the native variant (section 2.3, item 8), a backend that writes a dump must also leave a `crash.info` in exactly the grammar of section 2.3, item 8: `signal=<number>` (6 for an abort), a nonzero `time=<epoch ms>` taken at crash time, and ideally `code`, `address` and `frame` lines. Without it an aborting panic with a fresh `panic.info` becomes an empty native crash. The alternative is a recovery change that correlates on a panic snapshot regardless of what the native artifact is. Either way it needs a test.
- **A late-module-refresh capability.** Mature engines record images at crash time **[U]**; the in-process backend does not. The SDK needs to know which kind it has.

---

## 3. Constraints

### 3.1 Apple mobile (iOS, iPadOS, tvOS, visionOS, watchOS)

- **No out-of-process monitor.** Apple mobile apps cannot `fork`/`exec` a helper **[U]** (the OOP design doc says the same about stores). Capture is in-process and must be async-signal-safe, or run on a dedicated Mach exception thread.
- **Mach exceptions versus BSD signals.** A Mach handler on its own thread is the only reliable way to handle stack overflow (the faulting thread has no stack for a signal frame; `sigaltstack` is unreliable on iOS **[U]**) and sees `EXC_BAD_ACCESS` before it becomes `SIGSEGV`; Swift traps arrive as `EXC_BREAKPOINT`/`SIGTRAP`. A debugger attached to the process takes the exception ports first, so crashes under Xcode never reach the reporter **[U]**. Bugsee's iOS SDK uses the **BSD signal handler by default, with Mach as an opt-in option**, and forces BSD on tvOS because `task_set_exception_ports` is prohibited there **[V]** (the SDK's own comment; the header fact itself is **[U]**).
- **Uncatchable in-process:** `EXC_CRASH` from the kernel, jetsam/OOM terminations, watchdog kills and `SIGKILL`. These can only be inferred next launch (previous-run state, MetricKit **[U]**).
- **`NSException`** needs an explicit `NSSetUncaughtExceptionHandler` hook, and C++ exceptions need a `__cxa_throw` interposer **[V]** (the iOS SDK does both outside PLCrashReporter). A Rust-only process has neither; a Rust library inside an ObjC/Swift app will see them.
- **App Store.** Private API use is the rejection risk. `mach_vm_*` functions are marked unavailable in the iOS SDK headers; calling them through a Rust `extern` bypasses the compile-time check but may still be flagged **[U]**. The in-repo code has **two** `mach_vm_read_overwrite` call sites **[V]**, both `cfg(target_vendor = "apple")` and so both compiled if `mod imp` is widened: `read_task_mem` (the frame-pointer walk) and `posix::unreadable` (used on every `SIGSEGV`/`SIGBUS` to decide whether the host recovered). The iOS-safe equivalent is `vm_read_overwrite` **[U]**. Replacing only the walker would still link the symbol. #4 also says to prefer public APIs.
- **Targets named by `target_os = "ios"`.** `cfg(target_os = "ios")` also matches Mac Catalyst (`aarch64-apple-ios-macabi`, `x86_64-apple-ios-macabi`) **[U]**, which Bugsee's iOS SDK compiles its crash layer out of **[V]** and which D12 excludes. Any widening must exclude `target_abi = "macabi"`, unless D12 is changed to include Catalyst.
- **Simulator.** Simulator processes are host-OS processes, so signals work as on macOS **[U]**. Bugsee's iOS SDK compiles its crash layer out on the Simulator and Mac Catalyst; the only recorded reason is a comment that PLCrashReporter is "unavailable on the simulator" **[V]**. Whether our own handler works there is an open empirical question (D6).
- **Rust targets.** `aarch64-apple-ios`, `aarch64-apple-ios-sim` and `x86_64-apple-ios` are tier 2 **[U]**. #4 reports the tvOS/visionOS/watchOS targets as tier 2 and notes an earlier "tier 3" claim needs re-checking; they are out of scope for the first pass.
- **Coexistence.** Two reporters in one process is the normal case. Signal handlers chain; the single `NSUncaughtExceptionHandler` slot does not. Bugsee's iOS SDK refuses to run if its handler could not be installed over another reporter's **[V]**.

### 3.2 Android

- **ART owns `SIGSEGV`.** ART uses faults for implicit null checks, stack-overflow probing and GC barriers **[U]**, through its sigchain library, which runs ART's special handlers before user handlers and is meant to be the registration path for everyone **[U]**. Our chaining logic handles a host that recovers (section 2.2); the unknown is how our `sigaction` call interposes.
- **bionic and `debuggerd`.** bionic installs `debuggerd`'s handler for fatal signals in every process **[U]**; it dumps a tombstone, resets to default and returns, so the fault re-executes and kills the process. That matches our "host declined, restored default" branch **[U]**. Tombstones and `ApplicationExitInfo` (API 30+) are the system's record and a safety net for ANR, OOM kills and native crashes we could not catch. Reading `ApplicationExitInfo` needs Java/JNI **[U]** and so a Kotlin/AAR shim.
- **`sigaltstack`.** Never replace ART's **[V]** (Bugsee's Android SDK rule): use the existing one with `SA_ONSTACK`, and do not install one on a thread we do not own.
- **Handler launch constraints (Crashpad), per Bugsee's Android SDK [V].** The handler must be started after extracting it from the APK; API 29+ starts it through the system linker, API 21-28 use a Java `app_process` path; an in-process interim handler covers the extraction window; an early-startup crash must finish export and upload on the launch path. These are the costs of "Crashpad on Android".
- **ABIs.** `arm64-v8a`, `armeabi-v7a`, `x86`, `x86_64` are what Bugsee's Android SDK builds **[V]**. The Rust equivalents (`aarch64`, `armv7`, `i686`, `x86_64` `*-linux-android`) are tier 2 **[U]**. 32-bit ABIs matter for 64-bit atomics (the Android SDK's comments call out torn reads **[V]**); the Rust handler uses `AtomicU64` generation counters, which must be tested on `armv7` **[U]**.
- **16 KB pages.** Bugsee's Android SDK links with `-z max-page-size=16384` **[V]**. #4 states the Play requirement as "from 2027-02-01" for API 35+ updates; I did not verify the date **[U]**. A Rust cdylib needs the same linker flags (or a recent NDK default **[U]**). The handler already reads the page size at runtime.
- **Where Rust runs.** Either a pure Rust app (NativeActivity / `android-activity`, Tauri, Bevy, Slint) where Rust owns the process, or a Rust library loaded by a Kotlin/Java host through JNI. They have different crash-reporter owners (section 4).

### 3.3 Engines compared

| | Own in-process handler (today) | PLCrashReporter (Apple) | Crashpad (Android) |
|---|---|---|---|
| Platforms | Linux, Android, macOS; iOS by `cfg` widening | iOS, macOS, tvOS **[U]** | Android, desktop; iOS in-process **[U]** |
| Process model | In-process | In-process | Out-of-process helper on Android **[V]** |
| Records | Marker, crashing-thread PCs, module map | All threads, registers, images with UUIDs, `NSException`; Mach or BSD **[U]** | Minidump: all threads, registers, modules, stacks **[V]**; the worker already stackwalks these |
| Stack overflow | Header only, frames best effort | Mach mode only **[U]** | Yes, separate process |
| Heap corruption | Frames may be lost | Same exposure as in-process | Yes |
| Packaging | None | C/ObjC/asm plus protobuf-c, buildable with the `cc` crate per #4 **[U]** | C++ tree plus a handler and a launch path **[V]** |
| Licence surface | Ours | MIT plus protobuf-c **[U]** | Apache-2.0 plus bundled third-party code **[U]** |
| Maturity | New; `a1564ae` is its first coexistence pass | High **[U]**; Bugsee's iOS SDK already depends on a fork **[V]** | High **[U]**; Bugsee's Android SDK vendors it **[V]** |
| Unwinding | Frame pointers (Apple), `_Unwind_Backtrace` (Android) | Compact unwind / DWARF plus frame pointers **[U]** | `libunwindstack` in Bugsee's Android SDK **[V]** |

Known weakness of our Apple walker: it starts from `(pc, fp)`, so a fault in a **frameless leaf** function skips the immediate caller (held in the link register). Bugsee's iOS SDK keeps a test scenario for exactly this **[V]**.

### 3.4 Symbolication identity (what the backend expects)

All **[V]** from `crates/bugsee-native`, `recovery.rs` and the worker (`symbolfiles/*`, `crash/rust.py`, `crash/helpers/android_ndk.py`) unless marked.

- **Android: GNU build-id.** Upload: `symbolfiles/elf.py` stores `code_id` per ELF object and **skips any object without a build-id**, with a warning that the library must be linked with `-Wl,--build-id`. Crash: `bugsee-native` reads `NT_GNU_BUILD_ID` from `PT_NOTE` and writes lowercase hex; the worker normalises it with the shared canonical function and rejects non-hex or all-zero ids. A `.so` without a build-id can never resolve. Whether packaging steps keep the note, and that the *unstripped* `.so` from `target/<triple>/release/` is what must be uploaded, are **[U]**.
- **Apple: Mach-O `LC_UUID`.** Upload: dSYM `debug_id`, canonicalised by the appserver and worker (lowercase, no dashes). Crash: `bugsee-native::image_info` emits `LC_UUID` as lowercase hex; `tests/code_id_matches_symbolic.rs` cross-checks it against `symbolic`, the library the CLI and worker use. When Rust is a static library linked into the app, its frames are in the app's Mach-O, so the app's dSYM provides symbols. That is simpler than Android.
- **`arch` and platform.** `aarch64` is reported as `arm64` to match `symbolic`; `source_platform()` is `std::env::consts::OS`, so `android` and `ios` pass through.
- **Frame model.** The thin variant sends `reladdr` per frame and `code_id` per module; the worker resolves them with SymCache primitives shared with the Apple path and appends `s.`-prefixed signatures to the client signature. Android NDK reports use a different path (minidump plus Breakpad symbols from ELF). Wiring the minidump route for `sdk.type == 'rust'` on `android` is untested **[U]**.
- **System modules.** PROGRESS.md notes Windows reports stay `missing_sym` because of unsymbolicatable system DLLs and that Android "almost certainly has the same shape" **[V]**. Mobile reports will carry `libc.so`, `libart.so` and system frameworks; this must be fixed on the worker for mobile issues to reach a clean symbolicated state.

### 3.5 Host coexistence

- The existing rule (install over, chain to previous, delete the marker when the host recovers) already covers hosts that fault on purpose **[V]**.
- It does not answer **duplication**. With a different vendor's reporter present, a real crash reported by both is expected and harmless. With Bugsee's own native SDK in the same process it produces two issues for one crash. The Rust SDK cannot reliably detect the sibling SDK (symbol probing is fragile **[U]**), so the policy must be explicit configuration with a safe default (D4).
- `NSUncaughtExceptionHandler` and `std::panic::set_hook` are single-slot. The panic hook is ours already; the `NSException` hook is not, and a PLCrashReporter backend would claim it (D5).

---

## 4. Who is the mobile consumer? (frames every decision below)

| Scenario | Who already owns native faults | What the Rust SDK adds | Native capture default |
|---|---|---|---|
| A. Pure Rust app (Tauri mobile, Bevy, Slint, a game) | Nobody, unless the app adds a reporter | Everything | **On** |
| B. Rust library inside a native Android/iOS app that has a crash reporter | The host's reporter, which also catches native faults in our code | Panics, handled errors, logs; native capture would duplicate | **Off**, opt-in |
| C. Rust SDK as the shared native layer under Bugsee's other SDKs (the #4 vision) | The Rust SDK itself | Everything, replacing per-platform native layers | **On**; parity with the shipping SDKs is then a hard requirement |
| D. Rust under Flutter/.NET/KMP/React Native | The wrapper SDK embeds a native Bugsee SDK for that platform | Panics, handled errors | **Off** |

Only scenario C demands engine parity (all threads, registers, ANR/OOM, `NSException`). A and B are served by a good in-process handler. The plan therefore delivers A/B first and treats C as a gate.

---

## 5. Recommendation per platform

### 5.1 Apple mobile

**End state:** PLCrashReporter behind `CrashBackend` (`name() == "plcrashreporter"`), BSD signal handler, Mach exceptions off by default, tvOS BSD-only, `NSException` and C++ hooks as a later increment. This follows the agreed direction in #4 and matches the shipping iOS SDK's defaults **[V]**, which is also the evidence that it works on real devices.

**Not first.** Do the cheap in-process widening first, as a baseline and fallback:
1. Widen `mod imp`'s `cfg` to include `target_os = "ios"` (device and simulator, but **excluding `target_abi = "macabi"`**, i.e. Mac Catalyst), replace `mach_vm_read_overwrite` with `vm_read_overwrite` at **both** call sites (`read_task_mem` and `posix::unreadable`) **[U]**, confirm with `nm` that no `mach_vm_*` symbol remains in an iOS build, and run the crash matrix on the iOS simulator. This gives end-to-end Apple mobile capture on the existing marker/module/recovery path with **no new dependencies**, and answers open decision 4 empirically.
2. Label it tier 2 / experimental: crashing thread only, frame-pointer unwinding (leaf-frame gap), no Mach, no `NSException`. Do not claim parity.
3. Then build the PLCrashReporter backend as its own `-sys` plus wrapper crate pair, selected by `default_backend()` on device targets, falling back to the in-process handler where PLCrashReporter is unavailable (simulator, if confirmed).

Why not skip step 1: it is a `cfg` change plus a test job, it de-risks the simulator and CI plumbing PLCrashReporter needs anyway, and it is the fallback. Why not stop there: leaf-frame loss, no all-thread stacks, no `NSException`, and no parity with what Bugsee's iOS SDK sends.

**Report shape (D2).** A PLCrashReporter crash can become (a) our thin native variant plus additive threads and registers, or (b) the iOS SDK's crash document, so the existing Apple worker path does everything. (b) gives parity immediately but needs a worker routing change (the worker routes on `sdk.type == 'rust'`) and a large converter **[V]**. Recommend (a), because the Rust path already symbolicates `modules` plus `reladdr` and it keeps one report shape across desktop and mobile; revisit (b) if the worker owners prefer one Apple path.

### 5.2 Android

**Phase A (first; default for scenarios A and B):** keep the in-process handler, make it Android-correct, prove it on an emulator.
- Never replace an existing `sigaltstack`; on Android, only install one on threads the SDK owns.
- Verify the sigchain interaction on a real ART process: a Java `NullPointerException`, a Java stack overflow and an ART-handled implicit null check must produce **no** marker; a native `SIGSEGV` in a JNI library must.
- Refresh the module map for late-loaded libraries (at launch, flush and foreground, or by tracking loader additions **[U]**).
- Build all four ABIs, link for 16 KB pages, and verify the build-id note survives packaging.
- Data directory: depend on PR #24 and the Phase 0 work (section 2.3, item 6).

**Phase B (conditional):** Crashpad behind `CrashBackend` (`name() == "crashpad"`), only if Phase A shows a gap that matters (all-thread stacks, heap corruption, stack overflow, Java frames) **and** scenario C is a goal. The costs are the ones Bugsee's Android SDK already paid **[V]**. A prerequisite decision is whether to **share one Crashpad source tree** with the Android SDK instead of vendoring a second copy (D7). The OOP design doc recommends *against* Crashpad for desktop v1; if desktop picks `minidumper`, Android would be the only Crashpad user and "one engine everywhere" is not a benefit.

**Phase C (deferred):** `ApplicationExitInfo` merge and Java-frame capture. Both need Java code, so they belong in an AAR shipped with the Rust SDK, which is a packaging decision (D9).

### 5.3 What to defer, and why

| Item | Why deferred |
|---|---|
| Hang/watchdog, ANR | Need main-thread liveness plumbing; the Rust SDK does not own the UI thread. |
| OOM/jetsam inference, MetricKit | Next-launch heuristics, a separate feature; Bugsee's iOS SDK keeps them outside PLCrashReporter **[V]**. |
| `ApplicationExitInfo` | Needs a Java shim (D9). |
| C++ `__cxa_throw` hook, ObjC `NSException` | Only matter for scenario C and ObjC/C++ hosts. |
| Mach exception backend | Opt-in in the shipping SDK **[V]**; stack-overflow reliability is its only strong benefit. |
| tvOS, visionOS, watchOS device work | Target tiers and entitlements unverified; tvOS has no Mach exception ports **[V]**. |
| Out-of-process on Apple mobile | Not possible without a helper process **[U]**. |
| Out-of-process on Android | Same decision as Crashpad (Phase 4). |
| Instruction-window capture around the PC | An internal Bugsee plan scoped to the existing Android/iOS SDKs **[V]**; needs a safe-read API per platform. Revisit after parity. |

---

## 6. Phased plan with acceptance criteria

Phases are ordered by dependency and risk, not by calendar. Each ends in something mergeable.

### Phase 0 — Prerequisites (small, no engine)
- Merge or rebase PR #23 (`CrashBackend`), and decide whether the OOP doc's backend-selection rule (`default_backend()`, fall back to in-process) is adopted as the shared rule.
- Data directory: land PR #24 (#9) or rebase on it, then settle **one** Android story. **Recommendation: accept `TempFallback` as the no-directory behaviour, and add a C ABI way to pass `data_dir`.** (a) The C ABI gets a `data_dir` setter or a launch-with-options entry; a Kotlin/Java host passes `Context.getFilesDir()`, which is durable and the supported path. (b) A host that passes nothing gets PR #24's order: `BUGSEE_DATA_DIR`, then `TempFallback`, with the native handler armed and `may_be_ephemeral()` true, or memory mode (handler off) if nothing is writable. **Justification:** the alternative (skipping `TempFallback` on Android in #24 so no-directory means memory mode) adds an Android-specific branch to a PR whose point is one defined order, and it would turn off crash capture for every FFI host that has not yet been updated to pass a path. Arming on an app-private cache directory loses at worst the one crash whose marker the OS purged before the next launch, while the handler's cost is the same as it is today (the C ABI already arms it, section 2.3 item 7); the unproven handler is gated by Phase 1, not by directory choice. Whether to flip the C ABI default to off remains D4, independent of this. **Why not an AAR for Phase 0:** Rust cannot discover `filesDir` without JNI and the host already knows it (D9). iOS needs nothing beyond PR #24. Do **not** make `temp_dir()` a hard error on `android`/`ios`.
- Native-capture control: **extend the existing `LaunchOptions::native_crash_capture`** (default `true`) and expose it through the C ABI; do not add a second knob. If a richer mode is needed later (off / own / backend name), make it a superset of that option. Changing the C ABI default to off is a behaviour change for existing FFI hosts and needs an explicit decision (D4).
- Fix the stale statements: the `bugsee-native` module doc, the PROGRESS.md items in section 2.1, and issue #4's "Current state".
- **Acceptance:** a host in a mobile sandbox can launch through the C ABI with an explicit directory and with `native_crash_capture` off. An Android FFI host that passes a files directory launches with the handler installed and `storage_status()` reporting a disk-backed, non-ephemeral directory. An Android host that passes none ends in `DataDirSource::TempFallback` with `may_be_ephemeral()` true and the handler armed (unless D4 flips the C ABI default off), or, if a device run shows `temp_dir()` is unwritable there, in memory mode with the handler off and no error; the device run records which. An iOS host with no directory uses the app container default from PR #24. Desktop behaviour is unchanged (all existing tests pass) and the doc statements match section 2.1.

### Phase 1 — Android in-process, proven
- Emulator CI job (API 34 `x86_64` for speed, plus API 30 and one `arm64` image if available) running a test binary or instrumented host that crashes on purpose, including a JNI-library host for scenario B.
- Port the applicable `crash_matrix` scenarios (null deref, abort, illegal instruction, stack overflow, heap corruption, `SIGBUS`).
- Android correctness items from 5.2 Phase A.
- **Acceptance:**
  1. each applicable scenario yields a marker with signal, address and at least one module-relative frame;
  2. a Java `NullPointerException`, a Java `StackOverflowError` and an ART-handled implicit null check yield **no** marker and the SDK stays armed;
  3. a library loaded after `launch` appears in `crash.modules`, or its frames are explicitly marked unresolved;
  4. all four ABIs build in CI and at least `x86_64` and `arm64-v8a` run;
  5. an uploaded unstripped `.so` with a build-id symbolicates a native frame end to end against staging (the same round trip PROGRESS.md records for Windows);
  6. a test reads each thread's `sigaltstack` before and after install and finds ART's unchanged.

### Phase 2 — Apple in-process on simulator and device
- Widen the `cfg`, switch to `vm_read_overwrite`, and run the Apple matrix on `aarch64-apple-ios-sim` (and on a device manually).
- Port the *scenarios* of the iOS SDK's crash catalogue (abort, null, stack smash, ObjC exception, C++ throw, corrupt malloc, undefined/privileged instruction, read-only page write, OOM, deadlock, Swift traps) as an Apple test matrix **[V]**; copy the scenarios, not the files.
- **Acceptance:**
  1. the simulator runs the signal-path matrix in CI;
  2. each scenario is recorded as captured, partially captured or not capturable, in a table here or in PROGRESS.md;
  3. a device run of the same matrix is documented;
  4. the App Store question for every symbol used is settled by an `nm` audit against public APIs or an upload;
  5. the answer to D6 is recorded.

### Phase 3 — PLCrashReporter backend (Apple)
- `-sys` crate (vendored, trimmed, licence notices, provenance naming only the public upstream, a re-vendor script, built through `cc` in `build.rs`) and a wrapper crate implementing `CrashBackend`. Distinct symbol prefix (D3). A storage directory that cannot collide with the host's. BSD signals only.
- Next-launch conversion into the chosen report shape (D2), including threads and registers.
- The backend must leave a `crash.info` in the recovery grammar (section 2.3, item 8): numeric `signal=<i32>` (6 for abort, not `SIGABRT`), `time=<epoch ms>` nonzero and from the crash itself, plus `code`, `address` and `frame` lines where known. If a PLCrashReporter report is converted at next launch instead, the converter writes that file before `build_report` runs, and the `time` must be the crash time from the report, not the conversion time (or it falls outside the 10 s window).
- **Acceptance:**
  0. an aborting-panic test that goes through `recovery::build_report` with this backend's output: a fresh `panic.info` plus a backend-produced `crash.info` (`signal=6`, nonzero `time` within 10 s of the panic timestamp) yields **one managed event** (no `signal` object, `exception_type` not `native`); the same input with a `time` outside the window yields the native variant, so the test pins both sides;
  1. the Phase 2 matrix passes at least as well as the in-process handler on every scenario and strictly better on leaf-frame, all-thread and `NSException` cases;
  2. an app linking this crate and Bugsee's iOS SDK links, launches and does not duplicate the report under the chosen policy;
  3. binary size and build-time deltas are recorded;
  4. a licence and advisory check (`cargo deny`) passes with the vendored notices;
  5. the Rust staticlib's frames symbolicate from the app's dSYM in a staging round trip.

### Phase 4 — Crashpad go/no-go and, if go, backend (Android)
- Decide from Phase 1 data and D1/D7. If go: vendor or share the source (D7), implement `CrashBackend`, carry host-supplied handler parameters through `BackendConfig` (D8), and write the dump to `crash.minidump`. A dump alone selects the native variant in `recovery.rs` (section 2.3, item 8), so the backend must also write a `crash.info` in the recovery grammar (section 2.3, item 8: numeric `signal=6`, nonzero `time=<epoch ms>` within 10 s of `panic.info`), or recovery must change. Acceptance includes the same `build_report` aborting-panic test as Phase 3, run against a dump-bearing session (dump plus `crash.info` plus fresh `panic.info` yields one managed event).
- **Acceptance:** all Phase 1 criteria still hold on the Crashpad backend; stack-overflow and heap-corruption scenarios produce a dump with all threads; the API 21-28 and API 29+ launch paths each have an emulator or device run; an early-startup crash reaches the upload path before host code can crash again (open decision 6, measured); the worker symbolicates the dump for `sdk.type == 'rust'` on `android`.

### Phase 5 — Optional
`ApplicationExitInfo` safety net (AAR), hang/ANR, OOM inference, `NSException` and C++ hooks, Mach exceptions, tvOS/visionOS/watchOS, instruction windows. Each is gated by a measured customer need.

---

## 7. Risks

| Risk | Likelihood / impact | Mitigation |
|---|---|---|
| The Android handler has never run; sigchain or `sigaltstack` behaviour breaks a host app (a reporter that crashes apps is worse than none). | Medium / high | Phase 1 gate; native capture off by default for scenarios B and D; do not enable Android capture by default before Phase 1 acceptance. |
| On Android the no-directory fallback is `TempFallback` (possibly an OS-purgeable cache directory) or, if unwritable, memory mode. | Medium / medium | Phase 0: C ABI `data_dir` with the host passing `filesDir`; `may_be_ephemeral()` surfaced; a device run records what `temp_dir()` resolves to; no hard error. |
| A dump-writing backend turns aborting panics into empty native crashes (`recovery.rs` classifier). | High if unaddressed / medium | Backend writes `crash.info` in the recovery grammar (numeric signal, nonzero ms `time`), or recovery changes; a `build_report` test in Phases 3 and 4. |
| The C ABI already arms the unproven Android handler (`native_crash_capture` defaults to `true`). | High / high | Phase 1 gate; an explicit decision on the C ABI default (D4). |
| Duplicate issues with Bugsee's own native SDK or another reporter in the same process. | High in B and C / medium | Explicit mode, off when a host reporter is expected (D4); document. |
| `NSUncaughtExceptionHandler` and signal ordering between PLCrashReporter and another reporter. | Medium / medium | Chain on install; a test with a second reporter present. |
| App Store flags a private or prohibited symbol. | Low-medium / high | Phase 2 audit; public APIs only; Mach off. |
| Symbol collision between this crate's vendored PLCrashReporter and Bugsee's iOS SDK. | High if prefixes match / high | D3; a link test in Phase 3. |
| Missing build-id (Android) or dSYM mismatch makes every mobile issue unsymbolicatable. | Medium / high | Build-id acceptance check in Phase 1; document uploading the unstripped artifact; reuse the existing `bugsee-cli` flow. |
| System modules keep issues `missing_sym` (known backend item). | High / medium | Track the worker fix as a Phase 1 dependency. |
| Crashpad on Android pushes packaging (handler library, linker launch, APK extraction) onto every host. | High if chosen / medium | Phase 4 is conditional; ship as an optional feature. |
| Vendored engine copies diverge from the SDKs' (bug fixes, notices). | Medium / medium | D7 single source of truth; re-vendor script with provenance. |
| 32-bit ABIs untested (`u64` atomics, `cfg` branches). | Medium / medium | Phase 1 builds all four and runs two. |
| Rust target tiers shift (tvOS/visionOS/watchOS). | Low / low | Re-check at the start of each phase; not in scope for Phase 2. |
| The worker's Rust route for `platform_type` `android`/`ios` is untested. | Medium / high | One staging round trip per platform in Phases 1 and 2, before any engine work. |

---

## 8. Decisions for maintainers

| # | Decision | Options | Recommendation |
|---|---|---|---|
| **D1** | Who is the mobile consumer? (section 4) | A pure Rust apps; B libraries in native apps; C shared layer under Bugsee's SDKs; D under cross-platform wrappers | Serve A and B now; treat C as a later gate. Without C there is little case for Crashpad, and PLCrashReporter is a nice-to-have. |
| **D2** | Apple report shape | (a) thin variant extended additively with threads and registers; (b) emit the iOS SDK's crash document | (a), with worker owners' sign-off on the additive fields; revisit (b) if they prefer one Apple path. |
| **D3** | Symbol prefix of the vendored PLCrashReporter | Distinct from the iOS SDK's; or the iOS SDK consumes the Rust layer | Distinct, unless D1 resolves to C with the iOS SDK adopting this layer. |
| **D4** | Native-capture mode and its default on mobile | Keep `native_crash_capture` default `true` everywhere; default off for the C ABI (a behaviour change, since it is armed today); auto | Reuse the existing `native_crash_capture` option and expose it in the C ABI. Keep `true` for a Rust-API launch. Whether the C ABI flips to off on mobile is the maintainers' call; I lean to off for iOS/Android FFI hosts until Phase 1, accepting the change. Do not rely on auto-detecting other reporters **[U]**. |
| **D5** | Ownership of `NSUncaughtExceptionHandler` / the C++ hook | Install only if free; install and chain; never | Install and chain, in Phase 5, with the contract stated in the `CrashBackend` docs. |
| **D6** | Is the iOS Simulator a supported crash-capture target? | Test our handler and record the result; follow the iOS SDK (disabled) | Test it in Phase 2. If it works, enable it for development and CI; if not, record why. |
| **D7** | Crashpad source of truth | Share one tree with the Android SDK; vendor a second copy; do not adopt | Decide after Phase 1; prefer sharing. Do not vendor a second copy without an owner for both. |
| **D8** | How backend-private parameters reach `BackendConfig` | Typed `#[non_exhaustive]` fields; a string map | Typed per-backend config structs carried by an enum, so misuse is a compile error. |
| **D9** | Ship a Java/Kotlin AAR with the SDK? | Yes (for `ApplicationExitInfo`, Java stacks, `filesDir` discovery); no (native only) | **No for Phase 0**: the host-supplied C ABI path (Phase 0) is sufficient, with `TempFallback` as the no-directory behaviour. Revisit before Phase 5, when `ApplicationExitInfo` and Java stacks need Java code; an AAR could then also remove the need for the host to pass a path. It is a distribution commitment. |
| **D10** | Early-crash recovery ("finish export and upload before host code can crash again") | Keep worker-thread recovery; add a synchronous launch-path step | Keep for A and B. Needed only for C and for Crashpad (Phase 4). |
| **D11** | Vendoring mechanics for third-party engines | Trimmed copy, notices, provenance naming only the public upstream, re-vendor script, `cargo deny` | Adopt as written in #4 and add `cargo deny` (the OOP doc reaches the same conclusion). |
| **D12** | Target scope of the first pass | iOS device and simulator, Android four ABIs; tvOS/visionOS/watchOS and Mac Catalyst (`macabi`) excluded | Exactly that; the `cfg` must exclude `target_abi = "macabi"` explicitly. |

---

## 9. Verified and unverified

**Verified locally:** `crates/bugsee-native` (`Cargo.toml`, `src/lib.rs` Unix, Apple and Android sections, module snapshot code, stub), `crates/bugsee-core/src/recovery.rs`, `crates/bugsee-core/src/model/environment.rs`, `crates/bugsee/src/{options,api}.rs`, `crates/bugsee-ffi/{src/lib.rs,include/bugsee.h}`, `.github/workflows/ci.yml`, DESIGN.md, PROGRESS.md and issue #4; `bugsee-native-api` and the `CrashBackend` wiring from `origin/refactor/crash-backend`; `docs/design/out-of-process-monitor.md` from `origin/docs/issue-17-oop-monitor-design`; the Bugsee backend worker's ELF and dSYM identity code and the `sdk.type == 'rust'` route; the structure and comments of Bugsee's Android and iOS SDK crash code (read for behaviour; nothing copied).

**Not verified (everything marked [U]):** PLCrashReporter, Crashpad and Breakpad behaviour and licences; ART sigchain interposition, `debuggerd`, what `temp_dir()` resolves to on Android; Apple SDK header availability of `mach_vm_*` and `task_set_exception_ports`, App Store review rules; Rust target tiers; the Play Store 16 KB deadline; `ApplicationExitInfo` access from C; whether the worker's Rust path works for `android` and `ios`.

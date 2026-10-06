//
//  lib.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Native fatal-crash capture.
//!
//! Installs a process crash handler (via the `crash-handler` crate) that, on a
//! fatal signal/exception, writes a tiny **async-signal-safe** crash-info marker
//! next to the current session's capture parts, then lets default handling
//! terminate the process. The marker is picked up on the next launch by
//! `bugsee-core`'s recovery path, which builds the thin native `crash.json`.
//!
//! The crash-time path does no allocation, locking, or formatting through the
//! standard library — only stack buffers and raw `open`/`write`/`close`.
//!
//! See `DESIGN.md` §5, §15.

//! # Coexisting with a host runtime
//!
//! Runtimes embedding the SDK (CLR, JVM, V8, Dart, Ruby…) fault on purpose and
//! recover. The guarantees, per platform:
//!
//! * **Linux/Android/macOS** — the SDK installs *over* the existing handlers and calls
//!   the previous one itself. The crash marker is written first and **deleted**
//!   if that handler returns without restoring the default action (or the signal
//!   was only sent), so a recovered fault leaves no report and the SDK stays
//!   armed. A handler installed *after* the SDK sits in front of it and the SDK
//!   never sees what it handles. `SIGABRT` is always treated as fatal, and a host
//!   that leaves its handler by `siglongjmp` keeps the marker (it is discarded
//!   if the session ends cleanly).
//! * **Windows** — reporting happens in the unhandled-exception filter, which
//!   runs only after every vectored/structured handler declined, so a fault a
//!   host recovers from never reaches it, whichever was installed first.
//!
//! # Supported platforms
//!
//! Linux, Android, macOS and Windows. The handler is built on the
//! `crash-handler` crate, which has no iOS (or tvOS/watchOS) backend, so on
//! every other target this crate compiles to a stub whose [`install`] returns
//! an `Unsupported` error — the facade treats that as "no native capture"
//! rather than failing the build, which is what lets the iOS C ABI link at all.
//! Native crash capture for iOS needs its own backend; it is not provided here.

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
))]
mod imp {
    use std::path::PathBuf;

    #[cfg(windows)]
    use crash_handler::{CrashContext, CrashEventResult, CrashHandler};

    /// Max crashing-thread frames captured for the dedup signature.
    // Gated to the platforms whose handler actually captures frames. Widening
    // this cfg is part of adding Windows native capture — the helper is needed
    // verbatim there, so it is gated rather than `allow(dead_code)`d.
    #[cfg(any(
        windows,
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android"
    ))]
    const MAX_FRAMES: usize = 64;
    /// Module-map file (base→name) written next to `crash.info` at install time and
    /// read back by recovery to turn crash-time frame PCs into stable module offsets.
    /// MUST match `bugsee_core::recovery::MODULES_NAME`.
    const MODULES_NAME: &str = "crash.modules";

    /// Keeps the native crash handler installed for its lifetime.
    #[cfg(windows)]
    pub struct NativeHandler {
        _handler: CrashHandler,
    }

    /// Keeps the native crash handler installed for its lifetime (dropping it puts
    /// the previously installed handlers back).
    #[cfg(unix)]
    pub struct NativeHandler {
        _private: (),
    }

    #[cfg(unix)]
    impl Drop for NativeHandler {
        fn drop(&mut self) {
            posix::uninstall();
        }
    }

    /// The marker path the handler writes to, behind an atomic so [`rebind`] can
    /// retarget a live handler (a fork child must not write into its parent's
    /// session). A load is async-signal-safe; the buffers are leaked on purpose,
    /// because a handler running on another thread may still be reading the old
    /// one, and a path is a few dozen bytes.
    static MARKER_PATH: std::sync::atomic::AtomicPtr<Vec<PathChar>> =
        std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

    /// In a `fork()` child the inherited marker path points into the PARENT's
    /// session. Until the child installs its own handler, a crash would write there
    /// and be recovered as the parent's — so the hook clears the path (an atomic
    /// store, the one thing a multithreaded child may safely do) and the handler
    /// then records nothing rather than something misattributed.
    #[cfg(unix)]
    fn install_fork_hook() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            extern "C" fn child() {
                MARKER_PATH.store(std::ptr::null_mut(), std::sync::atomic::Ordering::Release);
            }
            // SAFETY: registers a plain `extern "C"` fn that only does an atomic
            // store. Failure (ENOMEM) just leaves the hook uninstalled.
            unsafe {
                libc::pthread_atfork(None, None, Some(child));
            }
        });
    }

    fn publish_marker_path(path: &std::path::Path) {
        let leaked = Box::into_raw(Box::new(path_to_cbytes(path)));
        MARKER_PATH.store(leaked, std::sync::atomic::Ordering::Release);
    }

    /// Install the native crash handler, writing the marker to `crash_info_path` on
    /// a fatal crash. Keep the returned guard alive for the handler to stay active.
    pub fn install(crash_info_path: PathBuf) -> std::io::Result<NativeHandler> {
        #[cfg(unix)]
        install_fork_hook();
        publish_marker_path(&crash_info_path);

        // The deadline thread must exist BEFORE a crash: creating a thread inside an
        // exception handler can deadlock on the loader lock.
        #[cfg(windows)]
        Watchdog::start();

        // Snapshot the loaded module map NOW: `dyld`/`dl_iterate_phdr` are not
        // async-signal-safe, so this cannot run at crash time. Recovery joins these
        // bases with the crash-time frame PCs to derive ASLR-invariant offsets.
        if let Some(dir) = crash_info_path.parent() {
            write_modules_file(&dir.join(MODULES_NAME));
        }

        #[cfg(unix)]
        {
            posix::install()?;
            Ok(NativeHandler { _private: () })
        }
        #[cfg(windows)]
        {
            let handler = CrashHandler::attach(unsafe {
                crash_handler::make_crash_event(move |cc: &CrashContext| {
                    #[cfg(any(unix, windows))]
                    let watchdog = Watchdog::arm();
                    let path = MARKER_PATH.load(std::sync::atomic::Ordering::Acquire);
                    if !path.is_null() {
                        // Only ever a leaked `Box<Vec<_>>`, never freed (this closure is
                        // already inside the `unsafe` block that builds the handler).
                        on_crash(&*path, cc);
                    }
                    #[cfg(any(unix, windows))]
                    watchdog.disarm();
                    // Continue to the previous/default handler so the process terminates
                    // with the original signal (and any host reporter also sees it).
                    CrashEventResult::Handled(false)
                })
            })
            .map_err(|e| std::io::Error::other(format!("crash handler attach failed: {e}")))?;

            Ok(NativeHandler { _handler: handler })
        }
    }

    /// How long the crash handler may run before the process is killed outright.
    ///
    /// The handler must never be able to turn a crash into a HANG: it takes locks
    /// (`crash-handler`'s own, the allocator inside the unwinder), writes to the
    /// filesystem (which can block forever on a dead network mount), and can itself
    /// fault with a *different* signal than the one it is handling. A crashed
    /// process that never exits holds its resources, blocks a supervisor's restart
    /// and — for a service — is strictly worse than the crash it hides.
    #[cfg(any(unix, windows))]
    const HANDLER_BUDGET_SECS: u32 = 5;

    /// A `SIGALRM`-based deadline for the crash handler. Only async-signal-safe
    /// calls (`sigaction`, `sigprocmask`, `sigpending`, `sigwait`, `alarm`) are
    /// used.
    ///
    /// The host's own `SIGALRM` disposition, thread signal mask and remaining
    /// `alarm` time are saved and restored by [`Watchdog::disarm`], because the
    /// handler can return and the process can go on living (a signal that was
    /// *sent* rather than faulted, swallowed by a previous handler) — the host's
    /// timers must survive that.
    #[cfg(unix)]
    struct Watchdog {
        previous: libc::sigaction,
        old_mask: libc::sigset_t,
        leftover: libc::c_uint,
    }

    #[cfg(unix)]
    impl Watchdog {
        fn arm() -> Self {
            // SAFETY: plain async-signal-safe libc calls on stack data.
            unsafe {
                let mut previous: libc::sigaction = core::mem::zeroed();
                let mut dfl: libc::sigaction = core::mem::zeroed();
                dfl.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut dfl.sa_mask);
                let mut set: libc::sigset_t = core::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGALRM);
                let mut old_mask: libc::sigset_t = core::mem::zeroed();
                // Block SIGALRM first so nothing can be delivered while the
                // disposition is switched to the default (terminate) action.
                libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old_mask);
                libc::sigaction(libc::SIGALRM, &dfl, &mut previous);
                // A host SIGALRM that already expired is pending: consume it, or
                // unblocking below would kill the process before the marker is
                // written.
                loop {
                    let mut pending: libc::sigset_t = core::mem::zeroed();
                    libc::sigemptyset(&mut pending);
                    if libc::sigpending(&mut pending) != 0
                        || libc::sigismember(&pending, libc::SIGALRM) != 1
                    {
                        break;
                    }
                    let mut sig: libc::c_int = 0;
                    if libc::sigwait(&set, &mut sig) != 0 {
                        break;
                    }
                }
                // Start the deadline, then let it be delivered (the default
                // action only fires if some thread can take it).
                let leftover = libc::alarm(HANDLER_BUDGET_SECS);
                libc::sigprocmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut());
                Watchdog {
                    previous,
                    old_mask,
                    leftover,
                }
            }
        }

        fn disarm(self) {
            // SAFETY: as in `arm`.
            unsafe {
                libc::alarm(0);
                libc::sigaction(libc::SIGALRM, &self.previous, core::ptr::null_mut());
                if self.leftover != 0 {
                    libc::alarm(self.leftover);
                }
                libc::sigprocmask(libc::SIG_SETMASK, &self.old_mask, core::ptr::null_mut());
            }
        }
    }

    /// Windows counterpart of the unix `SIGALRM` watchdog. There is no async-signal
    /// equivalent of `alarm`, so a dedicated thread is created at install time and
    /// parked on an event; the handler signals it on entry and again on exit. If the
    /// exit signal does not arrive within the budget the thread ends the process
    /// with `TerminateProcess` — which, unlike anything the wedged thread could do,
    /// does not depend on the state the handler is stuck in.
    #[cfg(windows)]
    struct Watchdog;

    #[cfg(windows)]
    static WD_ARM: core::sync::atomic::AtomicPtr<core::ffi::c_void> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    #[cfg(windows)]
    static WD_DONE: core::sync::atomic::AtomicPtr<core::ffi::c_void> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

    /// Exit status the watchdog ends a wedged process with ("BUGS").
    #[cfg(windows)]
    #[doc(hidden)]
    pub const WATCHDOG_EXIT_CODE: u32 = 0x4255_4753;

    /// Test hook: when set, the handler wedges itself, standing in for a blocked
    /// filesystem or a deadlock so the watchdog can be exercised on Windows (which
    /// has no FIFO to block an `open` on).
    #[cfg(windows)]
    static TEST_WEDGE_HANDLER: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);

    /// Make the next crash handler wedge itself. Tests only.
    #[cfg(windows)]
    #[doc(hidden)]
    pub fn wedge_handler_for_test() {
        TEST_WEDGE_HANDLER.store(true, core::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(windows)]
    unsafe extern "system" fn watchdog_thread(_: *mut core::ffi::c_void) -> u32 {
        use core::sync::atomic::Ordering::SeqCst;
        const WAIT_TIMEOUT: u32 = 0x102;
        loop {
            let arm = WD_ARM.load(SeqCst);
            let done = WD_DONE.load(SeqCst);
            unsafe {
                win32::WaitForSingleObject(arm, u32::MAX);
                if win32::WaitForSingleObject(done, HANDLER_BUDGET_SECS * 1000) == WAIT_TIMEOUT {
                    win32::TerminateProcess(win32::GetCurrentProcess(), WATCHDOG_EXIT_CODE);
                }
            }
        }
    }

    #[cfg(windows)]
    impl Watchdog {
        /// Create the events and the parked thread. Idempotent; on any failure the
        /// watchdog is simply absent (the handler still works, just unbounded).
        fn start() {
            use core::sync::atomic::Ordering::SeqCst;
            if !WD_ARM.load(SeqCst).is_null() {
                return;
            }
            unsafe {
                let null = core::ptr::null_mut();
                let arm = win32::CreateEventW(null, 0, 0, core::ptr::null());
                let done = win32::CreateEventW(null, 0, 0, core::ptr::null());
                if arm.is_null() || done.is_null() {
                    return;
                }
                WD_DONE.store(done, SeqCst);
                WD_ARM.store(arm, SeqCst);
                let th = win32::CreateThread(null, 0, Some(watchdog_thread), null, 0, null.cast());
                if th.is_null() {
                    WD_ARM.store(core::ptr::null_mut(), SeqCst);
                    return;
                }
                win32::CloseHandle(th);
            }
        }

        fn arm() -> Self {
            use core::sync::atomic::Ordering::SeqCst;
            let arm = WD_ARM.load(SeqCst);
            if !arm.is_null() {
                unsafe { win32::SetEvent(arm) };
            }
            Watchdog
        }

        fn disarm(self) {
            use core::sync::atomic::Ordering::SeqCst;
            let done = WD_DONE.load(SeqCst);
            if !done.is_null() {
                unsafe { win32::SetEvent(done) };
            }
        }
    }

    /// The element type of the prepared path: bytes for the POSIX `open`, UTF-16
    /// units for `CreateFileW`. Both are NUL-terminated at install time so the
    /// crash-time path allocates nothing.
    #[cfg(unix)]
    type PathChar = u8;
    #[cfg(windows)]
    type PathChar = u16;
    #[cfg(not(any(unix, windows)))]
    type PathChar = u8;

    /// The loaded-module map, exposed for tests only.
    ///
    /// Lets a test cross-check the identity this crate reports at crash time
    /// against `symbolic` — the crate the CLI keys an upload on and the worker keys
    /// the symbol store on. A mismatch there is silent: symbolication simply
    /// resolves nothing.
    #[doc(hidden)]
    pub fn snapshot_modules_for_test() -> Vec<(usize, usize, String, String)> {
        snapshot_modules()
    }

    /// NUL-terminated path, prepared at install time so the crash-time path
    /// performs no allocation.
    #[cfg(unix)]
    fn path_to_cbytes(path: &std::path::Path) -> Vec<PathChar> {
        use std::os::unix::ffi::OsStrExt;
        // Use the real OS bytes (paths are not guaranteed UTF-8); a lossy conversion
        // would target the wrong file and silently lose the crash marker.
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    /// UTF-16 for `CreateFileW`, deliberately not the ANSI `CreateFileA`.
    ///
    /// A Windows path is UTF-16 natively, and the ANSI form goes through the
    /// active code page: a data directory under a profile like `C:\Users\Ünal`
    /// would resolve to a different path or fail outright, losing the crash marker
    /// on exactly the machines least able to report it. `encode_wide` is the
    /// lossless conversion.
    #[cfg(windows)]
    fn path_to_cbytes(path: &std::path::Path) -> Vec<PathChar> {
        use std::os::windows::ffi::OsStrExt;
        let mut units: Vec<u16> = path.as_os_str().encode_wide().collect();
        units.push(0);
        units
    }

    #[cfg(not(any(unix, windows)))]
    fn path_to_cbytes(path: &std::path::Path) -> Vec<PathChar> {
        let mut bytes = path.to_string_lossy().into_owned().into_bytes();
        bytes.push(0);
        bytes
    }

    // Unix platforms deliver a POSIX signal; `posix::handler` decodes it and calls
    // this with the kernel's `ucontext`.
    #[cfg(unix)]
    fn on_crash(
        path_cbytes: &[PathChar],
        signo: i32,
        code: i32,
        addr: usize,
        uc: *const libc::c_void,
    ) {
        // F29: persist the GUARANTEED marker (signal/code/addr) FIRST, THEN capture
        // frames. Linux frame capture runs `backtrace::trace_unsynchronized` on the
        // crashing thread, which is NOT async-signal-safe (it may lock / re-fault);
        // if it does, the essential crash info has already been written rather than
        // the whole report being lost.
        unsafe {
            write_marker(path_cbytes, signo, code, addr);
        }
        let mut frames = [0usize; MAX_FRAMES];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let n = capture_frames(
            unsafe { context_pc(uc as *const libc::ucontext_t) },
            &mut frames,
        );
        #[cfg(target_vendor = "apple")]
        let n = unsafe {
            let (pc, fp) = context_regs(uc as *const DarwinUcontext);
            capture_frames(pc, fp, &mut frames)
        };
        unsafe {
            append_frames(path_cbytes, &frames[..n]);
        }
    }

    /// The fault address for a signal that carries one (SIGSEGV/SIGBUS/SIGILL/
    /// SIGFPE/SIGTRAP); `0` otherwise.
    #[cfg(unix)]
    unsafe fn fault_address(si: *const libc::siginfo_t, signo: i32) -> usize {
        // `libc` constants: SIGBUS differs between Linux (7) and BSD/Apple (10).
        if !matches!(
            signo,
            libc::SIGILL | libc::SIGTRAP | libc::SIGBUS | libc::SIGFPE | libc::SIGSEGV
        ) {
            return 0;
        }
        // SAFETY: `si` is the kernel-provided `siginfo_t` of a fault signal, whose
        // `_sigfault` union member holds the address.
        unsafe { (*si).si_addr() as usize }
    }

    /// A small stack-only formatter — no heap, no locks (async-signal-safe).
    #[cfg(any(unix, windows))]
    struct StackBuf {
        buf: [u8; 160],
        len: usize,
    }

    #[cfg(any(unix, windows))]
    impl StackBuf {
        fn new() -> Self {
            StackBuf {
                buf: [0; 160],
                len: 0,
            }
        }
        fn byte(&mut self, b: u8) {
            if self.len < self.buf.len() {
                self.buf[self.len] = b;
                self.len += 1;
            }
        }
        fn s(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.byte(b);
            }
        }
        fn dec(&mut self, v: i64) {
            // Work in unsigned magnitude so `i64::MIN` (whose negation overflows) is
            // handled correctly: `(v as u64).wrapping_neg()` is the two's-complement
            // magnitude for any negative `v`, including `i64::MIN`.
            let mut m: u64 = if v < 0 {
                self.byte(b'-');
                (v as u64).wrapping_neg()
            } else {
                v as u64
            };
            if m == 0 {
                self.byte(b'0');
                return;
            }
            let mut tmp = [0u8; 20];
            let mut n = 0;
            while m > 0 {
                tmp[n] = b'0' + (m % 10) as u8;
                m /= 10;
                n += 1;
            }
            while n > 0 {
                n -= 1;
                self.byte(tmp[n]);
            }
        }
        fn hex(&mut self, mut v: usize) {
            if v == 0 {
                self.byte(b'0');
                return;
            }
            let mut tmp = [0u8; 16];
            let mut n = 0;
            while v > 0 {
                let d = (v & 0xf) as u8;
                tmp[n] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
                v >>= 4;
                n += 1;
            }
            while n > 0 {
                n -= 1;
                self.byte(tmp[n]);
            }
        }
    }

    /// Write the guaranteed crash-info marker (signal/code/address/time) using only
    /// async-signal-safe primitives. Frame lines are appended separately via
    /// [`append_frames`] so this essential header is persisted BEFORE any (possibly
    /// non-async-signal-safe) frame capture runs — a re-fault there then loses only
    /// the frames, not the whole crash report (F29).
    #[cfg(unix)]
    unsafe fn write_marker(path_cbytes: &[PathChar], signo: i32, code: i32, addr: usize) {
        let fd = unsafe {
            libc::open(
                path_cbytes.as_ptr() as *const libc::c_char,
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o600,
            )
        };
        if fd < 0 {
            return;
        }
        // Crash time (epoch ms) via clock_gettime — async-signal-safe, lets
        // next-launch recovery bound panic↔crash correlation by freshness.
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // `tv_sec`/`tv_nsec` are `time_t`/`c_long`, whose widths vary by target
        // (e.g. 32-bit on some platforms), so the casts to `i64` are needed for
        // portability even where clippy sees them as no-ops on this host.
        #[allow(clippy::unnecessary_cast)]
        let time_ms = if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) } == 0 {
            ts.tv_sec as i64 * 1000 + ts.tv_nsec as i64 / 1_000_000
        } else {
            0
        };
        let mut b = StackBuf::new();
        b.s(b"signal=");
        b.dec(signo as i64);
        b.s(b"\ncode=");
        b.dec(code as i64);
        b.s(b"\naddress=0x");
        b.hex(addr);
        b.s(b"\ntime=");
        b.dec(time_ms);
        b.byte(b'\n');
        unsafe {
            let _ = libc::write(fd, b.buf.as_ptr() as *const libc::c_void, b.len);
            let _ = libc::close(fd);
        }
    }

    /// Append one `frame=0x<pc>` line per captured absolute PC to an already-written
    /// marker (opened `O_APPEND`). Kept separate from [`write_marker`] so the header
    /// survives even if the frame capture that produced `frames` faulted (F29). Each
    /// line is formatted in its own stack buffer and written immediately (no heap),
    /// so an arbitrary frame count never overruns a single fixed buffer.
    #[cfg(unix)]
    unsafe fn append_frames(path_cbytes: &[PathChar], frames: &[usize]) {
        if frames.is_empty() {
            return;
        }
        let fd = unsafe {
            libc::open(
                path_cbytes.as_ptr() as *const libc::c_char,
                libc::O_WRONLY | libc::O_APPEND,
                0o600,
            )
        };
        if fd < 0 {
            return;
        }
        for &pc in frames {
            let mut fb = StackBuf::new();
            fb.s(b"frame=0x");
            fb.hex(pc);
            fb.byte(b'\n');
            unsafe {
                let _ = libc::write(fd, fb.buf.as_ptr() as *const libc::c_void, fb.len);
            }
        }
        unsafe {
            let _ = libc::close(fd);
        }
    }

    /// Map a Windows exception code onto the POSIX signal number the wire contract
    /// uses, mirroring `mach_to_signal` on Apple.
    ///
    /// `crash.json`'s `signal` object is shared across SDKs and consumers read a
    /// POSIX signal there, so the platform vocabulary is translated once, here.
    /// An unrecognised exception reports 0 rather than being rounded to SIGABRT —
    /// the same choice the Apple mapping makes, because a wrong signal is worse
    /// than an honestly unknown one.
    #[cfg(any(windows, test))]
    fn exception_to_signal(code: i32) -> i32 {
        const SIGILL: i32 = 4;
        const SIGTRAP: i32 = 5;
        const SIGABRT: i32 = 6;
        const SIGBUS: i32 = 7;
        const SIGFPE: i32 = 8;
        const SIGSEGV: i32 = 11;

        match code as u32 {
            0xC000_0005 => SIGSEGV, // ACCESS_VIOLATION
            // A stack overflow IS a memory fault; reporting it as SIGSEGV keeps it
            // with the other faults rather than inventing a Windows-only number.
            0xC000_00FD => SIGSEGV, // STACK_OVERFLOW
            0xC000_001D => SIGILL,  // ILLEGAL_INSTRUCTION
            0xC000_0096 => SIGILL,  // PRIVILEGED_INSTRUCTION
            0x8000_0002 => SIGBUS,  // DATATYPE_MISALIGNMENT
            0xC000_008C => SIGSEGV, // ARRAY_BOUNDS_EXCEEDED
            0xC000_0094 => SIGFPE,  // INT_DIVIDE_BY_ZERO
            0xC000_0095 => SIGFPE,  // INT_OVERFLOW
            0xC000_008E => SIGFPE,  // FLT_DIVIDE_BY_ZERO
            0x8000_0003 => SIGTRAP, // BREAKPOINT
            // The CRT/abort family: these really are "the program gave up".
            0xC000_0374 => SIGABRT, // HEAP_CORRUPTION
            0x4000_0015 => SIGABRT, // FATAL_APP_EXIT
            0xC000_000D => SIGABRT, // INVALID_PARAMETER
            0xC000_0025 => SIGABRT, // NONCONTINUABLE_EXCEPTION (purecall)
            _ => 0,
        }
    }

    /// The faulting ADDRESS for an access violation, or 0.
    ///
    /// Deliberately not `ExceptionAddress`, which is the instruction that faulted —
    /// the unix `si_addr` this field mirrors is the address that was *accessed*,
    /// and for an access violation Windows puts that in `ExceptionInformation[1]`.
    /// Reporting the instruction pointer instead would look plausible and be wrong.
    #[cfg(windows)]
    unsafe fn fault_address(cc: &CrashContext) -> usize {
        const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;

        if cc.exception_pointers.is_null() {
            return 0;
        }
        let record = unsafe { (*cc.exception_pointers).ExceptionRecord };
        if record.is_null() {
            return 0;
        }
        let record = unsafe { &*record };
        if record.ExceptionCode as u32 != EXCEPTION_ACCESS_VIOLATION {
            return 0;
        }
        // [0] is the access type (read/write/execute), [1] the address touched.
        record.ExceptionInformation[1]
    }

    /// Capture the crashing thread's PCs.
    ///
    /// The handler runs ON the faulting thread (SEH delivers it there), so the
    /// live stack IS the crash stack — the same property the Linux path relies on
    /// when it walks its own stack with `backtrace`.
    ///
    /// Frame 0 is taken from `ExceptionAddress` rather than from the walk: that is
    /// the instruction that faulted, and it is what the crash site must be. The
    /// walk alone would start inside this handler, which is precisely the
    /// misattribution that made every Rust panic group together before
    /// `is_internal_frame` existed.
    ///
    /// KNOWN LIMITATION: the frames after index 0 may still include a few of this
    /// handler's own, because `RtlCaptureStackBackTrace` starts where it is called
    /// and no reliable skip count exists across optimisation levels. They are
    /// deterministic, so grouping is unaffected (the dedup signature is built from
    /// module+offset pairs), but a displayed stack can carry them until a
    /// CONTEXT-based `RtlVirtualUnwind` replaces this.
    #[cfg(windows)]
    unsafe fn capture_frames(cc: &CrashContext, out: &mut [usize; MAX_FRAMES]) -> usize {
        let mut n = 0;

        // The faulting instruction first, so the crash site is right even if the
        // walk below yields nothing (a stack overflow leaves almost no stack).
        if !cc.exception_pointers.is_null() {
            let record = unsafe { (*cc.exception_pointers).ExceptionRecord };
            if !record.is_null() {
                let pc = unsafe { (*record).ExceptionAddress } as usize;
                if pc != 0 {
                    out[0] = pc;
                    n = 1;
                }
            }
        }

        let mut raw =
            [core::ptr::null_mut::<core::ffi::c_void>(); MAX_FRAMES + HANDLER_FRAME_SLACK];
        let got = unsafe {
            win32::RtlCaptureStackBackTrace(
                0,
                raw.len() as u32,
                raw.as_mut_ptr(),
                core::ptr::null_mut(),
            )
        } as usize;

        let mut pcs = [0usize; MAX_FRAMES + HANDLER_FRAME_SLACK];
        let mut m = 0;
        for &p in raw.iter().take(got) {
            if p.is_null() {
                break;
            }
            pcs[m] = p as usize;
            m += 1;
        }

        // The walk starts inside the handler. When it reaches the faulting
        // instruction, keep everything from there (callers included) and drop the
        // handler frames above it; otherwise keep the old behaviour — the PC
        // recorded above followed by whatever the walk produced.
        if n == 1 {
            if let Some(k) = select_crash_frames(&pcs[..m], Some(out[0]), out) {
                return k;
            }
        }
        for &p in pcs.iter().take(m.min(MAX_FRAMES - n)) {
            out[n] = p;
            n += 1;
        }

        n
    }

    #[cfg(windows)]
    fn on_crash(path_cbytes: &[PathChar], cc: &CrashContext) {
        if TEST_WEDGE_HANDLER.load(core::sync::atomic::Ordering::SeqCst) {
            loop {
                core::hint::spin_loop();
            }
        }
        let signo = exception_to_signal(cc.exception_code);
        let addr = unsafe { fault_address(cc) };
        // `code` stays 0: it mirrors POSIX `si_code`, and Windows has no equivalent
        // vocabulary. The access type IS available in ExceptionInformation[0], but
        // putting it in a field consumers read as an si_code would be a wrong
        // answer in a right-shaped slot.
        // Header first and independently: it must survive even if the capture
        // below faults (F29).
        unsafe {
            write_marker(path_cbytes, signo, 0, addr);
        }

        let mut frames = [0usize; MAX_FRAMES];
        let n = unsafe { capture_frames(cc, &mut frames) };
        if n > 0 {
            unsafe { append_frames(path_cbytes, &frames[..n]) };
        }
    }

    /// Open the marker for writing, creating/truncating it. Returns the raw handle
    /// or `INVALID_HANDLE_VALUE`.
    ///
    /// `CreateFileW`/`WriteFile` are the analogue of the unix `open`/`write` used
    /// here: plain kernel calls that take no loader lock and allocate nothing, so
    /// they are usable from an exception handler.
    #[cfg(windows)]
    unsafe fn open_marker(path_cbytes: &[PathChar], append: bool) -> *mut core::ffi::c_void {
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const FILE_APPEND_DATA: u32 = 0x0004;
        const CREATE_ALWAYS: u32 = 2;
        const OPEN_EXISTING: u32 = 3;
        const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

        let (access, disposition) = if append {
            (FILE_APPEND_DATA, OPEN_EXISTING)
        } else {
            (GENERIC_WRITE, CREATE_ALWAYS)
        };

        unsafe {
            win32::CreateFileW(
                path_cbytes.as_ptr(),
                access,
                0,
                core::ptr::null_mut(),
                disposition,
                FILE_ATTRIBUTE_NORMAL,
                core::ptr::null_mut(),
            )
        }
    }

    #[cfg(windows)]
    unsafe fn write_all(handle: *mut core::ffi::c_void, bytes: &[u8]) {
        let mut written: u32 = 0;
        unsafe {
            win32::WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                core::ptr::null_mut(),
            );
        }
    }

    /// Write the crash-info marker. Byte-for-byte the same document the unix path
    /// writes, so recovery parses it with no platform knowledge at all.
    #[cfg(windows)]
    unsafe fn write_marker(path_cbytes: &[PathChar], signo: i32, code: i32, addr: usize) {
        const INVALID_HANDLE_VALUE: isize = -1;

        let handle = unsafe { open_marker(path_cbytes, false) };
        if handle as isize == INVALID_HANDLE_VALUE {
            return;
        }

        // Crash time (epoch ms), so next-launch recovery can bound panic<->crash
        // correlation by freshness exactly as it does on unix. FILETIME counts
        // 100ns ticks from 1601; the constant is the offset to the Unix epoch.
        const TICKS_PER_MS: u64 = 10_000;
        const EPOCH_DELTA_MS: u64 = 11_644_473_600_000;
        let mut ft = win32::FileTime { low: 0, high: 0 };
        unsafe { win32::GetSystemTimeAsFileTime(&mut ft) };
        let ticks = ((ft.high as u64) << 32) | ft.low as u64;
        let time_ms = (ticks / TICKS_PER_MS).saturating_sub(EPOCH_DELTA_MS) as i64;

        let mut b = StackBuf::new();
        b.s(b"signal=");
        b.dec(signo as i64);
        b.s(b"\ncode=");
        b.dec(code as i64);
        b.s(b"\naddress=0x");
        b.hex(addr);
        b.s(b"\ntime=");
        b.dec(time_ms);
        b.byte(b'\n');
        unsafe {
            write_all(handle, &b.buf[..b.len]);
            win32::CloseHandle(handle);
        }
    }

    /// Append one `frame=0x<pc>` line per captured PC, mirroring the unix path:
    /// kept separate so the header survives even if frame capture faulted (F29),
    /// and each line formatted in its own stack buffer so an arbitrary frame count
    /// cannot overrun one fixed buffer.
    #[cfg(windows)]
    unsafe fn append_frames(path_cbytes: &[PathChar], frames: &[usize]) {
        const INVALID_HANDLE_VALUE: isize = -1;

        if frames.is_empty() {
            return;
        }
        let handle = unsafe { open_marker(path_cbytes, true) };
        if handle as isize == INVALID_HANDLE_VALUE {
            return;
        }
        for pc in frames {
            let mut fb = StackBuf::new();
            fb.s(b"frame=0x");
            fb.hex(*pc);
            fb.byte(b'\n');
            unsafe { write_all(handle, &fb.buf[..fb.len]) };
        }
        unsafe { win32::CloseHandle(handle) };
    }

    #[cfg(not(any(unix, windows)))]
    unsafe fn write_marker(_path_cbytes: &[PathChar], _signo: i32, _code: i32, _addr: usize) {
        // Windows marker writing is added with the Windows exception path.
    }

    // NOTE: there is deliberately no `cfg(not(unix))` `append_frames`. The
    // non-unix `on_crash` captures no frames, so a stub would itself be dead code;
    // the Windows exception path adds both together.

    // ---------------------------------------------------------------------------
    // Module map (captured at install) + crash-time frame capture.
    // ---------------------------------------------------------------------------

    /// Write the loaded-module map, one `<base_hex>\t<size_hex>\t<code_id>\t<name>`
    /// line per module, so recovery can:
    /// - turn crash-time frame PCs into ASLR-invariant `pc - base` module offsets,
    /// - using `size`, reject a PC that falls OUTSIDE the module's loaded range
    ///   instead of misattributing it to the nearest-below module (F25), and
    /// - report each module's **code id** — the Mach-O `LC_UUID` on Apple, the GNU
    ///   build-id (`.note.gnu.build-id`) on Linux/Android.
    ///
    /// The code id is what the backend's symbol store is keyed on (the worker
    /// extracts the same value from uploaded symbols via `symbolic`), so without it
    /// a native crash can never be symbolicated no matter what the user uploads.
    /// `code_id` is empty when the module carries none (e.g. an ELF linked without
    /// `-Wl,--build-id`); recovery then simply omits it.
    fn write_modules_file(path: &std::path::Path) {
        let modules = snapshot_modules();
        if modules.is_empty() {
            return;
        }
        let mut body = String::with_capacity(modules.len() * 64);
        for (base, size, code_id, name) in modules {
            body.push_str(&format!("{base:x}\t{size:x}\t{code_id}\t{name}\n"));
        }
        let _ = std::fs::write(path, body);
    }

    /// Lowercase hex of raw id bytes (the form the symbol service stores).
    #[cfg(any(
        windows,
        test,
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android"
    ))]
    fn hex_lower(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }

    /// The trailing path component (module file name).
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android"))]
    fn basename(path: &str) -> String {
        path.rsplit('/').next().unwrap_or(path).to_string()
    }

    /// Enumerate loaded modules as `(load_base, vm_size, code_id, name)`. NOT
    /// async-signal-safe — call only from `install` (normal context). `vm_size` is
    /// the module's loaded address extent, used by recovery to reject an
    /// out-of-module PC (F25); `code_id` is the Mach-O `LC_UUID` (empty if absent).
    #[cfg(target_vendor = "apple")]
    fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
        use mach2::dyld::{_dyld_get_image_header, _dyld_get_image_name, _dyld_image_count};
        let mut modules = Vec::new();
        let count = unsafe { _dyld_image_count() };
        for i in 0..count {
            let header = unsafe { _dyld_get_image_header(i) } as usize;
            let name_ptr = unsafe { _dyld_get_image_name(i) };
            if header == 0 || name_ptr.is_null() {
                continue;
            }
            let name = unsafe { std::ffi::CStr::from_ptr(name_ptr) }.to_string_lossy();
            let (size, code_id) = image_info(header);
            modules.push((header, size, code_id, basename(&name)));
        }
        modules
    }

    // Minimal Mach-O structures needed to compute a loaded image's vm extent and read
    // its LC_UUID (mach2 only exposes the 32-bit `mach_header`). Layout matches
    // `<mach-o/loader.h>`.
    #[cfg(target_vendor = "apple")]
    const LC_SEGMENT_64: u32 = 0x19;
    #[cfg(target_vendor = "apple")]
    const LC_UUID: u32 = 0x1b;

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct MachHeader64 {
        magic: u32,
        cputype: i32,
        cpusubtype: i32,
        filetype: u32,
        ncmds: u32,
        sizeofcmds: u32,
        flags: u32,
        reserved: u32,
    }

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct LoadCommand {
        cmd: u32,
        cmdsize: u32,
    }

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct SegmentCommand64 {
        cmd: u32,
        cmdsize: u32,
        segname: [u8; 16],
        vmaddr: u64,
        vmsize: u64,
        fileoff: u64,
        filesize: u64,
        maxprot: i32,
        initprot: i32,
        nsects: u32,
        flags: u32,
    }

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct UuidCommand {
        cmd: u32,
        cmdsize: u32,
        uuid: [u8; 16],
    }

    /// One walk of the Mach-O load commands at `header`, returning
    /// `(vm_extent, code_id)`:
    /// - **vm_extent** — the span from the lowest to the highest `LC_SEGMENT_64` vm
    ///   address. `base + extent` bounds the module, so recovery can tell whether a
    ///   crash PC actually belongs to it.
    /// - **code_id** — the `LC_UUID` as lowercase hex (empty when the image has no
    ///   `LC_UUID`). This is the identity the symbol store is keyed on; `symbolic`
    ///   reads the same value out of an uploaded dSYM/Mach-O.
    #[cfg(target_vendor = "apple")]
    fn image_info(header: usize) -> (usize, String) {
        if header == 0 {
            return (0, String::new());
        }
        // SAFETY: `header` is a valid loaded mach_header from dyld; we only read the
        // header and walk `ncmds` load commands, each bounded by its own `cmdsize`.
        unsafe {
            let mh = &*(header as *const MachHeader64);
            let mut lc = header + core::mem::size_of::<MachHeader64>();
            let mut min_vmaddr = u64::MAX;
            let mut max_vmend: u64 = 0;
            let mut code_id = String::new();
            for _ in 0..mh.ncmds {
                let cmd = &*(lc as *const LoadCommand);
                if cmd.cmdsize == 0 {
                    break; // malformed — avoid an infinite loop
                }
                if cmd.cmd == LC_SEGMENT_64 {
                    let seg = &*(lc as *const SegmentCommand64);
                    min_vmaddr = min_vmaddr.min(seg.vmaddr);
                    max_vmend = max_vmend.max(seg.vmaddr.saturating_add(seg.vmsize));
                } else if cmd.cmd == LC_UUID
                    && cmd.cmdsize as usize >= core::mem::size_of::<UuidCommand>()
                {
                    let uc = &*(lc as *const UuidCommand);
                    code_id = hex_lower(&uc.uuid);
                }
                lc += cmd.cmdsize as usize;
            }
            let extent = if max_vmend > min_vmaddr {
                (max_vmend - min_vmaddr) as usize
            } else {
                0
            };
            (extent, code_id)
        }
    }

    /// ELF note type for the GNU build-id (`NT_GNU_BUILD_ID`), in a `PT_NOTE`
    /// segment whose note name is `"GNU\0"`.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const NT_GNU_BUILD_ID: u32 = 3;

    /// Scan a loaded `PT_NOTE` segment for the GNU build-id, returning it as
    /// lowercase hex.
    ///
    /// Note layout (`Elf_Nhdr` + payloads, each 4-byte aligned):
    /// `n_namesz | n_descsz | n_type | name[n_namesz] | desc[n_descsz]`.
    ///
    /// # Safety
    /// `start` must point at `len` readable bytes of a loaded PT_NOTE segment.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn gnu_build_id_from_notes(start: *const u8, len: usize) -> Option<String> {
        let align4 = |n: usize| n.div_ceil(4) * 4;
        let mut off = 0usize;
        // Each iteration consumes a full note; the header alone is 12 bytes.
        while off + 12 <= len {
            let hdr = unsafe { start.add(off) } as *const u32;
            let namesz = unsafe { *hdr } as usize;
            let descsz = unsafe { *hdr.add(1) } as usize;
            let ntype = unsafe { *hdr.add(2) };

            let name_off = off + 12;
            let desc_off = name_off + align4(namesz);
            let next = desc_off + align4(descsz);
            if next > len || desc_off > len {
                break; // malformed/truncated — stop rather than read out of bounds
            }

            if ntype == NT_GNU_BUILD_ID && namesz == 4 {
                let name = unsafe { std::slice::from_raw_parts(start.add(name_off), 4) };
                if name == b"GNU\0" && descsz > 0 {
                    let desc = unsafe { std::slice::from_raw_parts(start.add(desc_off), descsz) };
                    return Some(hex_lower(desc));
                }
            }
            off = next;
        }
        None
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
        // Widths of `p_vaddr`/`p_memsz` differ between Elf32/Elf64 phdrs, so the
        // `as u64` casts are needed for portability even where they are no-ops.
        #[allow(clippy::unnecessary_cast)]
        extern "C" fn collect(
            info: *mut libc::dl_phdr_info,
            _size: libc::size_t,
            data: *mut libc::c_void,
        ) -> libc::c_int {
            unsafe {
                let modules = &mut *(data as *mut Vec<(usize, usize, String, String)>);
                let info = &*info;
                let base = info.dlpi_addr as usize;
                // One phdr walk yields both the module extent (highest
                // `p_vaddr + p_memsz` over PT_LOAD, so recovery can bound offsets)
                // and the GNU build-id from PT_NOTE — the identity the symbol store
                // is keyed on.
                let mut extent: u64 = 0;
                let mut code_id = String::new();
                if !info.dlpi_phdr.is_null() && info.dlpi_phnum > 0 {
                    let phdrs =
                        std::slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum as usize);
                    for ph in phdrs {
                        if ph.p_type == libc::PT_LOAD {
                            let end = (ph.p_vaddr as u64).saturating_add(ph.p_memsz as u64);
                            extent = extent.max(end);
                        } else if ph.p_type == libc::PT_NOTE && code_id.is_empty() {
                            // PT_NOTE p_vaddr is link-time; add the load bias.
                            let addr = base.wrapping_add(ph.p_vaddr as usize);
                            if addr != 0 {
                                if let Some(id) =
                                    gnu_build_id_from_notes(addr as *const u8, ph.p_memsz as usize)
                                {
                                    code_id = id;
                                }
                            }
                        }
                    }
                }
                let name = if info.dlpi_name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(info.dlpi_name)
                        .to_string_lossy()
                        .into_owned()
                };
                // The main executable reports an empty name; recover its real file
                // name (as Apple and Windows do) so the module is identifiable.
                // Allocates, which is fine: this runs at install, not in a handler.
                let name = if name.is_empty() {
                    std::env::current_exe()
                        .ok()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .unwrap_or_else(|| "main".to_string())
                } else {
                    basename(&name)
                };
                modules.push((base, extent as usize, code_id, name));
            }
            0
        }
        let mut modules: Vec<(usize, usize, String, String)> = Vec::new();
        unsafe {
            libc::dl_iterate_phdr(Some(collect), &mut modules as *mut _ as *mut libc::c_void);
        }
        modules
    }

    // ---------------------------------------------------------------------------
    // Windows: loaded-module map with PE debug ids.
    //
    // The backend keys its symbol store on the identity `symbolic` reads from an
    // uploaded PDB, so the SDK has to report the SAME string or an upload resolves
    // nothing — the exact failure that made Mach-O crashes unsymbolicatable before
    // `code_id` existed. For PE that identity is the CodeView record's GUID + age,
    // rendered the way the `debugid` crate renders it.
    //
    // The parsing is deliberately split from the Win32 enumeration and kept pure so
    // it can be unit-tested on ANY host: the byte-order transform below is the part
    // that is easy to get wrong and impossible to eyeball.
    // ---------------------------------------------------------------------------

    /// Read a little-endian `u16` at `off`, or `None` if it would run off the end.
    #[cfg(any(windows, test))]
    fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
    }

    /// Read a little-endian `u32` at `off`, or `None` if it would run off the end.
    #[cfg(any(windows, test))]
    fn rd_u32(b: &[u8], off: usize) -> Option<u32> {
        Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
    }

    /// The debug id of a loaded PE image, in the canonical dashless-lowercase form.
    ///
    /// `image` is the module as MAPPED, so data-directory RVAs index straight into
    /// it (`AddressOfRawData`, not the file-offset `PointerToRawData`).
    ///
    /// Every field access is bounds-checked against the slice: this walks
    /// attacker-influenced-in-principle headers of arbitrary DLLs, and a malformed
    /// or truncated image must yield `None` rather than read out of bounds.
    ///
    /// Returns `None` when the image carries no CodeView record — a DLL built
    /// without debug info. Recovery then simply omits the id, exactly as it does
    /// for an ELF linked without `--build-id`.
    #[cfg(any(windows, test))]
    fn pe_debug_id(image: &[u8]) -> Option<String> {
        const DOS_MAGIC: u16 = 0x5A4D; // "MZ"
        const PE_SIG: u32 = 0x0000_4550; // "PE\0\0"
        const MAGIC_PE32: u16 = 0x010B;
        const MAGIC_PE32PLUS: u16 = 0x020B;
        const DIR_DEBUG: usize = 6;
        const DEBUG_TYPE_CODEVIEW: u32 = 2;
        const CV_SIG_RSDS: u32 = 0x5344_5352; // "RSDS", little-endian
                                              // A deterministic (Portable PDB) entry stores the id's second half in
                                              // TimeDateStamp instead of Age — see the PE-COFF spec's CodeView entry.
        const MINOR_DETERMINISTIC: u16 = 0x504D;

        if rd_u16(image, 0)? != DOS_MAGIC {
            return None;
        }
        let pe_off = rd_u32(image, 0x3C)? as usize;
        if rd_u32(image, pe_off)? != PE_SIG {
            return None;
        }

        // COFF header is 20 bytes; the optional header follows it.
        let opt_off = pe_off.checked_add(24)?;
        // The data directory sits after the magic-dependent tail of the optional
        // header: 96 bytes for PE32, 112 for PE32+ (the extra 16 come from the
        // 64-bit fields).
        let dir_off = match rd_u16(image, opt_off)? {
            MAGIC_PE32 => opt_off.checked_add(96)?,
            MAGIC_PE32PLUS => opt_off.checked_add(112)?,
            _ => return None,
        };

        let debug_rva = rd_u32(image, dir_off + DIR_DEBUG * 8)? as usize;
        let debug_size = rd_u32(image, dir_off + DIR_DEBUG * 8 + 4)? as usize;
        if debug_rva == 0 || debug_size == 0 {
            return None;
        }

        // Walk the IMAGE_DEBUG_DIRECTORY array (28 bytes per entry) for CodeView.
        let count = debug_size / 28;
        for i in 0..count {
            let e = debug_rva.checked_add(i * 28)?;
            if rd_u32(image, e + 12)? != DEBUG_TYPE_CODEVIEW {
                continue;
            }
            let minor = rd_u16(image, e + 10)?;
            let cv = rd_u32(image, e + 20)? as usize; // AddressOfRawData (an RVA here)
            if rd_u32(image, cv)? != CV_SIG_RSDS {
                continue;
            }

            let guid: &[u8] = image.get(cv + 4..cv + 20)?;
            let age = if minor == MINOR_DETERMINISTIC {
                rd_u32(image, e + 4)? // TimeDateStamp
            } else {
                rd_u32(image, cv + 20)? // Age
            };
            return Some(format_pe_debug_id(guid, age));
        }

        None
    }

    /// Render a CodeView GUID + age the way the symbol store stores it.
    ///
    /// **The byte order is the whole point.** A CodeView GUID is stored
    /// mixed-endian — the first three fields little-endian, the last eight bytes
    /// as-is — so the printed id is NOT the raw bytes in order. Getting this wrong
    /// produces a plausible-looking id that matches no symbol file ever, which is
    /// invisible without an end-to-end upload.
    ///
    /// The age is appended as lowercase hex and **omitted entirely when zero**,
    /// matching how `debugid` renders it; always appending `0` would mismatch every
    /// module whose age is zero.
    #[cfg(any(windows, test))]
    fn format_pe_debug_id(guid: &[u8], age: u32) -> String {
        let swapped = [
            guid[3], guid[2], guid[1], guid[0], // Data1, LE
            guid[5], guid[4], // Data2, LE
            guid[7], guid[6], // Data3, LE
            guid[8], guid[9], guid[10], guid[11], guid[12], guid[13], guid[14], guid[15],
        ];
        let mut s = hex_lower(&swapped);
        if age != 0 {
            use std::fmt::Write as _;
            let _ = write!(s, "{age:x}");
        }
        s
    }

    /// Minimal Win32 bindings.
    ///
    /// Declared here rather than pulling a binding crate, mirroring what
    /// `crash-handler` itself does (it dropped `winapi` for embedded bindings). The
    /// surface is four stable, decades-old entry points; a whole crate to reach
    /// them would be a poor trade for a library that keeps its dependency list
    /// deliberately short. All four live in `kernel32`, which Rust links by default
    /// on Windows targets — the `K32`-prefixed forms exist precisely so `psapi` is
    /// not needed.
    #[cfg(windows)]
    mod win32 {
        #[repr(C)]
        pub struct ModuleInfo {
            pub base_of_dll: *mut core::ffi::c_void,
            pub size_of_image: u32,
            pub entry_point: *mut core::ffi::c_void,
        }

        #[repr(C)]
        pub struct FileTime {
            pub low: u32,
            pub high: u32,
        }

        unsafe extern "system" {
            pub fn GetCurrentProcess() -> *mut core::ffi::c_void;
            pub fn TerminateProcess(process: *mut core::ffi::c_void, exit_code: u32) -> i32;
            pub fn CreateEventW(
                attrs: *mut core::ffi::c_void,
                manual_reset: i32,
                initial: i32,
                name: *const u16,
            ) -> *mut core::ffi::c_void;
            pub fn SetEvent(event: *mut core::ffi::c_void) -> i32;
            pub fn WaitForSingleObject(handle: *mut core::ffi::c_void, millis: u32) -> u32;
            pub fn CreateThread(
                attrs: *mut core::ffi::c_void,
                stack: usize,
                start: Option<unsafe extern "system" fn(*mut core::ffi::c_void) -> u32>,
                param: *mut core::ffi::c_void,
                flags: u32,
                thread_id: *mut u32,
            ) -> *mut core::ffi::c_void;
            pub fn CreateFileW(
                name: *const u16,
                access: u32,
                share: u32,
                security: *mut core::ffi::c_void,
                disposition: u32,
                flags: u32,
                template: *mut core::ffi::c_void,
            ) -> *mut core::ffi::c_void;
            pub fn WriteFile(
                handle: *mut core::ffi::c_void,
                buf: *const u8,
                len: u32,
                written: *mut u32,
                overlapped: *mut core::ffi::c_void,
            ) -> i32;
            pub fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
            pub fn GetSystemTimeAsFileTime(ft: *mut FileTime);
            pub fn RtlCaptureStackBackTrace(
                skip: u32,
                capture: u32,
                frames: *mut *mut core::ffi::c_void,
                hash: *mut u32,
            ) -> u16;
            pub fn K32EnumProcessModules(
                process: *mut core::ffi::c_void,
                modules: *mut *mut core::ffi::c_void,
                cb: u32,
                needed: *mut u32,
            ) -> i32;
            pub fn K32GetModuleInformation(
                process: *mut core::ffi::c_void,
                module: *mut core::ffi::c_void,
                info: *mut ModuleInfo,
                cb: u32,
            ) -> i32;
            pub fn K32GetModuleFileNameExW(
                process: *mut core::ffi::c_void,
                module: *mut core::ffi::c_void,
                filename: *mut u16,
                size: u32,
            ) -> u32;
        }
    }

    /// Enumerate loaded modules as `(load_base, image_size, code_id, name)`.
    ///
    /// Runs from `install`, in normal context — NOT from the handler. That is what
    /// makes ordinary enumeration safe here: calling this on a crashing thread is
    /// the classic crash-reporter deadlock, because the module APIs can take the
    /// loader lock the faulting thread may already hold.
    ///
    /// The consequence is the same one the Apple and Linux paths carry: a module
    /// loaded AFTER install is absent from the map, so a crash inside it reports
    /// but cannot be symbolicated. That bites harder on Windows, where plugins and
    /// delay-loaded DLLs are routine — a known limitation, not an oversight.
    #[cfg(windows)]
    fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
        // Generous but bounded: a large process can map a few hundred modules, and
        // this runs once at startup.
        const MAX_MODULES: usize = 1024;
        let mut out = Vec::new();

        unsafe {
            let process = win32::GetCurrentProcess();
            let mut handles: Vec<*mut core::ffi::c_void> = vec![core::ptr::null_mut(); MAX_MODULES];
            let mut needed: u32 = 0;
            let cb = (handles.len() * core::mem::size_of::<*mut core::ffi::c_void>()) as u32;
            if win32::K32EnumProcessModules(process, handles.as_mut_ptr(), cb, &mut needed) == 0 {
                return out;
            }
            // `needed` reports the bytes REQUIRED, which may exceed what was
            // written; clamp so a process with more modules than the cap truncates
            // instead of reading uninitialised handles.
            let count = (needed as usize / core::mem::size_of::<*mut core::ffi::c_void>())
                .min(handles.len());

            for &module in handles.iter().take(count) {
                if module.is_null() {
                    continue;
                }

                let mut info = win32::ModuleInfo {
                    base_of_dll: core::ptr::null_mut(),
                    size_of_image: 0,
                    entry_point: core::ptr::null_mut(),
                };
                if win32::K32GetModuleInformation(
                    process,
                    module,
                    &mut info,
                    core::mem::size_of::<win32::ModuleInfo>() as u32,
                ) == 0
                {
                    continue;
                }
                let base = info.base_of_dll as usize;
                let size = info.size_of_image as usize;
                if base == 0 || size == 0 {
                    continue;
                }

                let mut buf = [0u16; 260]; // MAX_PATH
                let n = win32::K32GetModuleFileNameExW(process, module, buf.as_mut_ptr(), 260);
                let path = String::from_utf16_lossy(&buf[..n as usize]);

                // The mapped image, read through the loaded pages: data-directory
                // RVAs index straight into this.
                let image = core::slice::from_raw_parts(base as *const u8, size);
                let code_id = pe_debug_id(image).unwrap_or_default();

                out.push((base, size, code_id, basename(&path)));
            }
        }

        out
    }

    /// Last path component. Windows separates with `\`, and accepts `/` too, so
    /// both are honoured — a module reported by its full path would never match the
    /// frame's module name, which the worker joins on.
    #[cfg(windows)]
    fn basename(path: &str) -> String {
        path.rsplit(['\\', '/']).next().unwrap_or(path).to_string()
    }

    #[cfg(not(any(
        windows,
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android"
    )))]
    fn snapshot_modules() -> Vec<(usize, usize, String, String)> {
        Vec::new()
    }

    /// Strip ARM pointer-authentication (PAC) bits from a return address read off the
    /// stack. On arm64e these are signed; leaving them in breaks `pc - base` module
    /// offsets and the dedup signature. `xpaci` exists on all Apple Silicon
    /// (armv8.3+) and is a no-op on an unsigned pointer, so it is safe for both plain
    /// arm64 and arm64e binaries. Async-signal-safe (a single register op, no memory
    /// access, no side effects).
    #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
    #[inline]
    fn strip_pac(ptr: usize) -> usize {
        let mut p = ptr;
        // SAFETY: `xpaci` only transforms the register value in place.
        unsafe {
            core::arch::asm!("xpaci {p}", p = inout(reg) p, options(nomem, nostack, preserves_flags));
        }
        p
    }

    /// x86_64 has no pointer authentication — return addresses are already plain.
    #[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
    #[inline]
    fn strip_pac(ptr: usize) -> usize {
        ptr
    }

    /// Capture up to [`MAX_FRAMES`] absolute PCs of the crashing thread into `out`,
    /// returning the count. Must be async-signal-safe.
    ///
    /// Apple: start from the interrupted `pc`/`fp` in the signal's context and walk
    /// the frame-pointer chain with the fault-safe `mach_vm_read_overwrite`.
    #[cfg(target_vendor = "apple")]
    unsafe fn capture_frames(pc: usize, mut fp: usize, out: &mut [usize; MAX_FRAMES]) -> usize {
        let task = mach2::traps::mach_task_self();
        let mut n = 0;
        if pc != 0 {
            out[n] = strip_pac(pc);
            n += 1;
        }
        // Frame-pointer chain: [fp] = caller's fp, [fp + word] = return address
        // (same layout on arm64 and x86_64).
        while n < out.len() && fp >= 0x1000 {
            let mut slot = [0usize; 2];
            if !unsafe { read_task_mem(task, fp, &mut slot) } {
                break;
            }
            // On arm64e a return address on the stack is PAC-signed; strip the
            // authentication bits so `pc - base` module offsets (and the dedup
            // signature) are stable (F31). No-op for plain arm64 / x86_64.
            let (next_fp, ra) = (slot[0], strip_pac(slot[1]));
            if ra == 0 {
                break;
            }
            out[n] = ra;
            n += 1;
            // Stack grows down, so a valid caller frame is at a strictly higher
            // address; anything else means the chain is corrupt — stop.
            if next_fp <= fp {
                break;
            }
            fp = next_fp;
        }
        n
    }

    /// The interrupted `(pc, fp)` from the signal's saved register state.
    #[cfg(target_vendor = "apple")]
    unsafe fn context_regs(uc: *const DarwinUcontext) -> (usize, usize) {
        // SAFETY: `uc` and its `uc_mcontext` come from the kernel for this signal.
        unsafe {
            let ss = &(*(*uc).uc_mcontext).ss;
            #[cfg(target_arch = "aarch64")]
            return (ss.__pc as usize, ss.__fp as usize);
            #[cfg(target_arch = "x86_64")]
            return (ss.__rip as usize, ss.__rbp as usize);
        }
    }

    /// `ucontext_t` as laid out by Darwin (`libc` does not expose it): the mcontext
    /// is `{exception state (16 bytes on both arm64 and x86_64), thread state}`.
    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct DarwinUcontext {
        uc_onstack: i32,
        uc_sigmask: u32,
        uc_stack: libc::stack_t,
        uc_link: *mut DarwinUcontext,
        uc_mcsize: usize,
        uc_mcontext: *mut DarwinMcontext,
    }

    #[cfg(target_vendor = "apple")]
    #[repr(C)]
    struct DarwinMcontext {
        es: [u8; 16],
        #[cfg(target_arch = "aarch64")]
        ss: mach2::structs::arm_thread_state64_t,
        #[cfg(target_arch = "x86_64")]
        ss: mach2::structs::x86_thread_state64_t,
    }

    /// Fault-safe read of two words at `addr` from `task` (returns false on bad
    /// memory instead of faulting — critical on the crash path).
    #[cfg(target_vendor = "apple")]
    unsafe fn read_task_mem(
        task: mach2::mach_types::task_t,
        addr: usize,
        dst: &mut [usize; 2],
    ) -> bool {
        use mach2::vm::mach_vm_read_overwrite;
        let want = core::mem::size_of::<[usize; 2]>() as u64;
        let mut outsize: u64 = 0;
        let kr = unsafe {
            mach_vm_read_overwrite(
                task,
                addr as u64,
                want,
                dst.as_mut_ptr() as u64,
                &mut outsize,
            )
        };
        kr == mach2::kern_return::KERN_SUCCESS && outsize == want
    }

    /// Linux/Android: the handler runs on the crashing thread, so an unwind of the
    /// current stack captures the fault (handles x86_64 and aarch64).
    ///
    /// That unwind starts INSIDE the handler, so its first frames are our own
    /// (`capture_frames`, `on_crash`, `crash-handler`, the signal trampoline) and
    /// only then the interrupted code. Those frames are noise at best and, on a
    /// stack overflow, they spend the frame budget before the interesting part. The
    /// interrupted PC is known exactly from the signal's `ucontext`, so the walk is
    /// cut to start there; if the unwinder never reaches it, the PC alone is kept
    /// rather than a list of handler frames.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn capture_frames(pc: Option<usize>, out: &mut [usize; MAX_FRAMES]) -> usize {
        let mut raw = [0usize; MAX_FRAMES + HANDLER_FRAME_SLACK];
        let mut n = 0;
        unsafe {
            backtrace::trace_unsynchronized(|frame| {
                if n < raw.len() {
                    raw[n] = frame.ip() as usize;
                    n += 1;
                    true
                } else {
                    false
                }
            });
        }
        match select_crash_frames(&raw[..n], pc, out) {
            Some(k) => k,
            // The unwinder never reached the interrupted PC: keep the exact PC
            // first, then whatever the walk produced (as on Windows), so callers
            // are not lost from the report and the signature.
            None => match pc {
                Some(pc) => {
                    out[0] = pc;
                    let mut k = 1;
                    for &f in &raw[..n] {
                        if k == MAX_FRAMES {
                            break;
                        }
                        if f != pc && f != 0 {
                            out[k] = f;
                            k += 1;
                        }
                    }
                    k
                }
                None => {
                    let k = n.min(MAX_FRAMES);
                    out[..k].copy_from_slice(&raw[..k]);
                    k
                }
            },
        }
    }

    /// How many extra raw frames to unwind so that discarding the handler's own
    /// frames still leaves a full `MAX_FRAMES` of the interrupted stack.
    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    const HANDLER_FRAME_SLACK: usize = 24;

    /// Cut an unwind that began inside the handler down to the interrupted code:
    /// copy `raw` from the first frame equal (or within one byte) to `pc` (the
    /// faulting instruction).
    /// `None` when `pc` is unknown or never appears — the caller picks a fallback.
    #[cfg(any(windows, target_os = "linux", target_os = "android", test))]
    fn select_crash_frames(raw: &[usize], pc: Option<usize>, out: &mut [usize]) -> Option<usize> {
        let pc = pc.filter(|&p| p != 0)?;
        // Allow for the unwinder reporting the return-address-adjusted IP (pc - 1)
        // and for the Thumb/low bit, but prefer an exact hit.
        let start = raw.iter().position(|&f| f == pc).or_else(|| {
            raw.iter()
                .position(|&f| f.abs_diff(pc) <= 1 || (f & !1) == (pc & !1))
        })?;
        let n = (raw.len() - start).min(out.len());
        out[..n].copy_from_slice(&raw[start..start + n]);
        Some(n)
    }

    /// The interrupted instruction pointer from the signal's saved register state.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn context_pc(uc: *const libc::ucontext_t) -> Option<usize> {
        // SAFETY: `uc` is the kernel-provided `ucontext_t` of the fault.
        let mc = unsafe { &(*uc).uc_mcontext };
        #[cfg(target_arch = "x86_64")]
        let pc = mc.gregs[16] as usize; // REG_RIP
        #[cfg(target_arch = "x86")]
        let pc = mc.gregs[14] as usize; // REG_EIP
        #[cfg(target_arch = "aarch64")]
        let pc = mc.pc as usize;
        #[cfg(target_arch = "arm")]
        let pc = mc.arm_pc as usize;
        #[cfg(not(any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "arm"
        )))]
        let pc = {
            let _ = mc;
            0usize
        };
        (pc != 0).then_some(pc)
    }

    #[cfg(unix)]
    /// The Unix signal path (Linux, Android, macOS).
    ///
    /// Written here rather than taken from `crash-handler` because that crate's
    /// handler cannot coexist with a host that recovers from faults: after ONE
    /// signal it restores every previous disposition and returns, so (a) it never
    /// learns whether the host handled the fault, leaving a crash marker for a
    /// process that goes on living, and (b) the SDK is uninstalled for good after
    /// the first recoverable fault.
    ///
    /// # Chaining contract
    ///
    /// Our handler is installed *over* whatever was there (the host's runtime,
    /// another reporter, Rust's own `SIGSEGV` handler). On a fault it:
    ///
    /// 1. writes the marker (header first, frames after — see [`on_crash`]);
    /// 2. calls the previous handler **itself**, with the real `siginfo`/`ucontext`
    ///    so the host can fix the interrupted context;
    /// 3. if that handler returned and did not reset the disposition to default
    ///    (or the signal was merely *sent*, not faulted), the fault was recovered:
    ///    the marker is deleted and our handler stays installed;
    /// 4. otherwise the marker stands and the process dies of the original fault.
    ///
    /// Not covered: a host that leaves the handler by `siglongjmp` (the marker
    /// stays, and is only attributed to a later death if the session does not end
    /// cleanly), and `SIGABRT`, which is always treated as fatal because `abort()`
    /// terminates the process even when a handler returns.
    mod posix {
        use super::*;
        use core::cell::UnsafeCell;
        use core::mem::MaybeUninit;
        use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

        const SIGNALS: [libc::c_int; 6] = [
            libc::SIGABRT,
            libc::SIGBUS,
            libc::SIGFPE,
            libc::SIGILL,
            libc::SIGSEGV,
            libc::SIGTRAP,
        ];
        /// Size of the alternate stack we provide to a thread that has none.
        const ALT_STACK_BYTES: usize = 128 * 1024;

        struct Previous(UnsafeCell<MaybeUninit<[libc::sigaction; 6]>>);
        // SAFETY: written only by `install` under `STATE`, before the handler is
        // armed, and only read by the handler afterwards.
        unsafe impl Sync for Previous {}
        static PREVIOUS: Previous = Previous(UnsafeCell::new(MaybeUninit::zeroed()));

        /// Number of live `NativeHandler`s; the handlers are armed while non-zero.
        static STATE: std::sync::Mutex<usize> = std::sync::Mutex::new(0);
        /// Mirror of the above for the handler (which must not lock).
        static ARMED: AtomicUsize = AtomicUsize::new(0);
        /// Thread currently writing the marker (0 = none).
        static WRITING: AtomicI32 = AtomicI32::new(0);
        /// Thread currently inside a host handler we are chaining to (0 = none).
        static CHAINING: AtomicI32 = AtomicI32::new(0);

        fn gettid() -> i32 {
            // SAFETY: plain syscall / libc call.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            unsafe {
                libc::syscall(libc::SYS_gettid) as i32
            }
            #[cfg(target_vendor = "apple")]
            unsafe {
                let mut id = 0u64;
                libc::pthread_threadid_np(0, &mut id);
                id as i32
            }
        }

        /// A handler needs somewhere to run when the faulting thread's own stack is
        /// the problem (stack overflow). Rust gives its own threads one; give the
        /// installing thread one if nothing did.
        fn ensure_alt_stack() {
            // SAFETY: sigaltstack on a zeroed/filled `stack_t`; the buffer is leaked
            // for the thread's lifetime on purpose.
            unsafe {
                let mut cur: libc::stack_t = core::mem::zeroed();
                if libc::sigaltstack(core::ptr::null(), &mut cur) == 0
                    && (cur.ss_flags & libc::SS_DISABLE != 0 || cur.ss_sp.is_null())
                {
                    let buf = Box::leak(vec![0u8; ALT_STACK_BYTES].into_boxed_slice());
                    let st = libc::stack_t {
                        ss_sp: buf.as_mut_ptr().cast(),
                        ss_flags: 0,
                        ss_size: ALT_STACK_BYTES,
                    };
                    libc::sigaltstack(&st, core::ptr::null_mut());
                }
            }
        }

        pub(super) fn install() -> std::io::Result<()> {
            let mut count = STATE.lock().unwrap_or_else(|e| e.into_inner());
            if *count > 0 {
                *count += 1;
                return Ok(());
            }
            ensure_alt_stack();
            // SAFETY: sigaction with zeroed + filled structs; `PREVIOUS` is not
            // read by the handler until `ARMED` is set below.
            unsafe {
                let prev = &mut *PREVIOUS.0.get();
                let prev = prev.as_mut_ptr().cast::<libc::sigaction>();
                let mut sa: libc::sigaction = core::mem::zeroed();
                libc::sigemptyset(&mut sa.sa_mask);
                for sig in SIGNALS {
                    libc::sigaddset(&mut sa.sa_mask, sig);
                }
                sa.sa_sigaction = handler as *const () as usize;
                sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO;
                for (i, sig) in SIGNALS.iter().enumerate() {
                    if libc::sigaction(*sig, &sa, prev.add(i)) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }
            ARMED.store(1, Ordering::Release);
            *count = 1;
            Ok(())
        }

        pub(super) fn uninstall() {
            let mut count = STATE.lock().unwrap_or_else(|e| e.into_inner());
            *count = count.saturating_sub(1);
            if *count > 0 {
                return;
            }
            ARMED.store(0, Ordering::Release);
            // SAFETY: restores the dispositions saved by `install`.
            unsafe {
                let prev = (*PREVIOUS.0.get()).as_ptr().cast::<libc::sigaction>();
                for (i, sig) in SIGNALS.iter().enumerate() {
                    let mut cur: libc::sigaction = core::mem::zeroed();
                    // Only undo what is still ours: a host that installed its own
                    // handler on top since must not be clobbered.
                    if libc::sigaction(*sig, core::ptr::null(), &mut cur) == 0
                        && cur.sa_sigaction == handler as *const () as usize
                    {
                        libc::sigaction(*sig, prev.add(i), core::ptr::null_mut());
                    }
                }
            }
        }

        /// Let the default action happen: reset the disposition and, for a signal
        /// that was *sent* (no fault will retrigger), deliver it again.
        unsafe fn die_of(sig: libc::c_int, info: *const libc::siginfo_t) {
            // SAFETY: async-signal-safe calls only.
            unsafe {
                let mut dfl: libc::sigaction = core::mem::zeroed();
                dfl.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut dfl.sa_mask);
                libc::sigaction(sig, &dfl, core::ptr::null_mut());
                if (*info).si_code <= 0 || sig == libc::SIGABRT {
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    let failed = {
                        let tid = libc::syscall(libc::SYS_gettid) as i32;
                        libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, sig) < 0
                    };
                    #[cfg(target_vendor = "apple")]
                    let failed = libc::pthread_kill(libc::pthread_self(), sig) != 0;
                    if failed {
                        libc::_exit(1);
                    }
                }
            }
        }

        unsafe extern "C" fn handler(
            sig: libc::c_int,
            info: *mut libc::siginfo_t,
            uc: *mut libc::c_void,
        ) {
            let tid = gettid();
            let Some(idx) = SIGNALS.iter().position(|&s| s == sig) else {
                return;
            };
            if ARMED.load(Ordering::Acquire) == 0 {
                // Uninstalled while a signal was in flight.
                // SAFETY: as in `die_of`.
                unsafe { die_of(sig, info) };
                return;
            }
            // SAFETY: `PREVIOUS` is initialised before `ARMED` is set.
            let prev = unsafe {
                (*PREVIOUS.0.get())
                    .as_ptr()
                    .cast::<libc::sigaction>()
                    .add(idx)
                    .read()
            };

            // A fault while WE are writing the marker: nothing more can be done.
            if WRITING.load(Ordering::Acquire) == tid {
                // SAFETY: as in `die_of`.
                unsafe { die_of(sig, info) };
                return;
            }
            // The host's handler aborting the process (Rust's stack-overflow
            // message path does): the marker for the original fault stands.
            if sig == libc::SIGABRT && CHAINING.load(Ordering::Acquire) == tid {
                return;
            }
            // One crashing thread at a time; another thread's crash waits (the
            // watchdog on the writer bounds this).
            while WRITING
                .compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                let ts = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 1_000_000,
                };
                // SAFETY: nanosleep is async-signal-safe.
                unsafe { libc::nanosleep(&ts, core::ptr::null_mut()) };
            }

            let path = MARKER_PATH.load(Ordering::Acquire);
            let mut wrote = false;
            if !path.is_null() {
                let watchdog = Watchdog::arm();
                // SAFETY: `info`/`uc` come from the kernel; the path is a leaked box.
                unsafe {
                    let addr = fault_address(info, sig);
                    on_crash(&*path, sig, (*info).si_code, addr, uc);
                }
                watchdog.disarm();
                wrote = true;
            }
            WRITING.store(0, Ordering::Release);

            let h = prev.sa_sigaction;
            if h == libc::SIG_DFL || (h == libc::SIG_IGN && (unsafe { (*info).si_code } > 0)) {
                // Nobody else handles it: the marker stands, the process dies.
                // SAFETY: as in `die_of`.
                unsafe { die_of(sig, info) };
                return;
            }
            if h != libc::SIG_IGN {
                CHAINING.store(tid, Ordering::Release);
                // SAFETY: `h` was a valid handler of the recorded kind when saved.
                unsafe {
                    if prev.sa_flags & libc::SA_SIGINFO != 0 {
                        let f: unsafe extern "C" fn(
                            libc::c_int,
                            *mut libc::siginfo_t,
                            *mut libc::c_void,
                        ) = core::mem::transmute(h);
                        f(sig, info, uc);
                    } else {
                        let f: unsafe extern "C" fn(libc::c_int) = core::mem::transmute(h);
                        f(sig);
                    }
                }
                CHAINING.store(0, Ordering::Release);
            }

            // Did the host recover? A returned handler that put the default back
            // is declining (the fault retriggers and kills the process); anything
            // else — or a signal that was only sent — means the process lives.
            let declined = sig == libc::SIGABRT || {
                // SAFETY: reads the current disposition.
                let mut cur: libc::sigaction = unsafe { core::mem::zeroed() };
                let ok = unsafe { libc::sigaction(sig, core::ptr::null(), &mut cur) } == 0;
                let sent = unsafe { (*info).si_code } <= 0;
                ok && cur.sa_sigaction == libc::SIG_DFL && !sent
            };
            if !declined && wrote {
                // SAFETY: unlink of the leaked, NUL-terminated path.
                unsafe { libc::unlink((*path).as_ptr() as *const libc::c_char) };
            }
        }
    }

    #[cfg(test)]
    mod pe_tests {
        use super::*;

        /// Build a minimal mapped PE32+ image carrying one CodeView record.
        ///
        /// Synthetic on purpose: the real check is the byte-order transform, and a
        /// hand-built image lets it be asserted on every host rather than only on a
        /// Windows runner — where a wrong id would still *look* like a plausible id.
        fn image_with_codeview(guid: &[u8; 16], age: u32, minor: u16) -> Vec<u8> {
            let mut img = vec![0u8; 0x800];
            img[0..2].copy_from_slice(&0x5A4Du16.to_le_bytes()); // "MZ"
            let pe = 0x100usize;
            img[0x3C..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
            img[pe..pe + 4].copy_from_slice(&0x0000_4550u32.to_le_bytes()); // "PE\0\0"

            let opt = pe + 24;
            img[opt..opt + 2].copy_from_slice(&0x020Bu16.to_le_bytes()); // PE32+
            let dir = opt + 112; // data directory for PE32+

            // DataDirectory[6] = DEBUG -> one 28-byte entry at RVA 0x400.
            let dbg_rva = 0x400usize;
            img[dir + 6 * 8..dir + 6 * 8 + 4].copy_from_slice(&(dbg_rva as u32).to_le_bytes());
            img[dir + 6 * 8 + 4..dir + 6 * 8 + 8].copy_from_slice(&28u32.to_le_bytes());

            let cv_rva = 0x500usize;
            img[dbg_rva + 4..dbg_rva + 8].copy_from_slice(&age.to_le_bytes()); // TimeDateStamp
            img[dbg_rva + 10..dbg_rva + 12].copy_from_slice(&minor.to_le_bytes());
            img[dbg_rva + 12..dbg_rva + 16].copy_from_slice(&2u32.to_le_bytes()); // CODEVIEW
            img[dbg_rva + 20..dbg_rva + 24].copy_from_slice(&(cv_rva as u32).to_le_bytes());

            img[cv_rva..cv_rva + 4].copy_from_slice(&0x5344_5352u32.to_le_bytes()); // "RSDS"
            img[cv_rva + 4..cv_rva + 20].copy_from_slice(guid);
            img[cv_rva + 20..cv_rva + 24].copy_from_slice(&age.to_le_bytes()); // Age
            img
        }

        /// The GUID bytes as a CodeView record stores them, and the id the symbol
        /// store holds for them. Mixed-endian: first three fields byte-swapped,
        /// last eight verbatim.
        const GUID: [u8; 16] = [
            0x3A, 0xE4, 0xB8, 0xDF, // Data1 LE -> dfb8e43a
            0x42, 0xF2, // Data2 LE -> f242
            0x73, 0x3D, // Data3 LE -> 3d73
            0xA4, 0x53, 0xAE, 0xB6, 0xA7, 0x77, 0xEF, 0x75, // Data4 verbatim
        ];
        const UUID_HEX: &str = "dfb8e43af2423d73a453aeb6a777ef75";

        #[test]
        fn guid_is_byte_swapped_not_copied_in_order() {
            // The regression that matters: a straight hex dump of the raw bytes
            // yields `3ae4b8df...`, which matches no symbol file ever uploaded.
            let id = format_pe_debug_id(&GUID, 1);
            assert_eq!(id, format!("{UUID_HEX}1"));
            assert!(!id.starts_with("3ae4b8df"), "raw byte order leaked: {id}");
        }

        #[test]
        fn a_zero_age_is_omitted_entirely() {
            // `debugid` renders the age only when non-zero. Appending a literal
            // "0" would mismatch every module built with age 0.
            assert_eq!(format_pe_debug_id(&GUID, 0), UUID_HEX);
        }

        #[test]
        fn a_multi_digit_age_is_lowercase_hex_not_decimal() {
            assert_eq!(format_pe_debug_id(&GUID, 26), format!("{UUID_HEX}1a"));
        }

        #[test]
        fn parses_a_mapped_image_end_to_end() {
            let img = image_with_codeview(&GUID, 1, 0);
            assert_eq!(pe_debug_id(&img).as_deref(), Some(&*format!("{UUID_HEX}1")));
        }

        #[test]
        fn a_deterministic_entry_takes_its_age_from_the_timestamp() {
            // Portable-PDB / deterministic builds (MinorVersion 0x504d) carry the
            // second half of the id in TimeDateStamp instead of Age.
            let img = image_with_codeview(&GUID, 0x2A, 0x504D);
            assert_eq!(
                pe_debug_id(&img).as_deref(),
                Some(&*format!("{UUID_HEX}2a"))
            );
        }

        #[test]
        fn malformed_or_debugless_images_yield_none_rather_than_reading_wild() {
            // This walks the headers of arbitrary third-party DLLs, so anything
            // unexpected must return None, never index out of bounds.
            assert_eq!(pe_debug_id(&[]), None);
            assert_eq!(pe_debug_id(&[0u8; 64]), None, "no MZ");

            let mut no_debug = image_with_codeview(&GUID, 1, 0);
            let pe = 0x100usize;
            let dir = pe + 24 + 112;
            no_debug[dir + 48..dir + 56].fill(0); // clear the DEBUG directory
            assert_eq!(pe_debug_id(&no_debug), None);

            let truncated = &image_with_codeview(&GUID, 1, 0)[..0x420];
            assert_eq!(pe_debug_id(truncated), None, "CodeView record off the end");

            let mut bad_pe = image_with_codeview(&GUID, 1, 0);
            bad_pe[0x3C..0x40].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            assert_eq!(pe_debug_id(&bad_pe), None, "e_lfanew past the image");
        }
    }

    #[cfg(test)]
    mod windows_signal_tests {
        use super::*;

        #[test]
        fn faults_map_to_the_posix_signals_the_contract_uses() {
            // `crash.json`'s `signal` object is shared across SDKs; consumers read a
            // POSIX number there, so the Windows vocabulary is translated once.
            assert_eq!(exception_to_signal(0xC000_0005u32 as i32), 11, "SIGSEGV");
            assert_eq!(exception_to_signal(0xC000_001Du32 as i32), 4, "SIGILL");
            assert_eq!(exception_to_signal(0xC000_0094u32 as i32), 8, "SIGFPE");
            assert_eq!(exception_to_signal(0x8000_0003u32 as i32), 5, "SIGTRAP");
            assert_eq!(exception_to_signal(0x8000_0002u32 as i32), 7, "SIGBUS");
        }

        #[test]
        fn a_stack_overflow_is_reported_as_a_memory_fault() {
            // It IS one. Grouping it with the other faults beats inventing a
            // Windows-only number the rest of the pipeline has never seen.
            assert_eq!(exception_to_signal(0xC000_00FDu32 as i32), 11);
        }

        #[test]
        fn the_abort_family_maps_to_sigabrt() {
            for code in [
                0xC000_0374u32, // HEAP_CORRUPTION
                0x4000_0015,    // FATAL_APP_EXIT
                0xC000_000D,    // INVALID_PARAMETER
                0xC000_0025,    // NONCONTINUABLE_EXCEPTION (purecall)
            ] {
                assert_eq!(exception_to_signal(code as i32), 6, "code {code:#x}");
            }
        }

        #[test]
        fn an_unknown_exception_is_zero_not_rounded_to_sigabrt() {
            // Same choice the Apple mapping makes: a wrong signal is worse than an
            // honestly unknown one, and 0 is what the wire contract expects for
            // "could not be named".
            assert_eq!(exception_to_signal(0xDEAD_BEEFu32 as i32), 0);
            assert_eq!(exception_to_signal(0), 0);
        }
    }

    #[cfg(test)]
    mod frame_selection_tests {
        use super::*;

        #[test]
        fn drops_handler_frames_above_the_interrupted_pc() {
            let raw = [0x10, 0x11, 0x12, 0xAAA, 0xBBB, 0xCCC];
            let mut out = [0usize; 8];
            let n = select_crash_frames(&raw, Some(0xAAA), &mut out).unwrap();
            assert_eq!(&out[..n], &[0xAAA, 0xBBB, 0xCCC]);
        }

        #[test]
        fn output_is_capped_to_the_destination() {
            let raw = [1, 2, 3, 4, 5];
            let mut out = [0usize; 2];
            let n = select_crash_frames(&raw, Some(2), &mut out).unwrap();
            assert_eq!(&out[..n], &[2, 3]);
        }

        #[test]
        fn unknown_or_unreached_pc_selects_nothing() {
            let mut out = [0usize; 4];
            assert!(select_crash_frames(&[1, 2, 3], None, &mut out).is_none());
            assert!(select_crash_frames(&[1, 2, 3], Some(0), &mut out).is_none());
            assert!(select_crash_frames(&[1, 2, 3], Some(9), &mut out).is_none());
            assert!(select_crash_frames(&[], Some(9), &mut out).is_none());
        }

        #[test]
        fn off_by_one_pc_still_selects() {
            let raw = [0x10, 0x11, 0xAAB, 0xBBB];
            let mut out = [0usize; 8];
            let n = select_crash_frames(&raw, Some(0xAAA), &mut out).unwrap();
            assert_eq!(&out[..n], &[0xAAB, 0xBBB]);
        }
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
))]
pub use imp::*;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
mod unsupported {
    use std::path::PathBuf;

    /// Placeholder: there is no native handler on this target.
    pub struct NativeHandler;

    // Matches the real handler's Drop contract, so callers' deliberate
    // `drop(handler)` is not `clippy::drop_non_drop` on this target.
    impl Drop for NativeHandler {
        fn drop(&mut self) {}
    }

    /// Always fails: no native crash handler exists for this target.
    pub fn install(_crash_info_path: PathBuf) -> std::io::Result<NativeHandler> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "native crash capture is not implemented for this target",
        ))
    }

    /// No modules are enumerated on an unsupported target.
    #[doc(hidden)]
    pub fn snapshot_modules_for_test() -> Vec<(usize, usize, String, String)> {
        Vec::new()
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "windows"
)))]
pub use unsupported::*;

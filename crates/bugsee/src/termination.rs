//
//  termination.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Opt-in flush on `SIGTERM`/`SIGINT` (unix) or console control events
//! (Windows). See [`crate::Bugsee::install_termination_flush`].
//!
//! # Unix
//!
//! The signal handler does only async-signal-safe things: it writes one byte
//! (the signal number) to a self-pipe, then **chains to whatever handler was
//! installed before ours**. A watcher thread reads the pipe and does the real
//! work on an ordinary thread, where taking locks, allocating and joining
//! threads are all fine:
//!
//! - the previous disposition was the default (terminate): `shutdown` (flush,
//!   then a clean stop, because the exit is deliberate), restore the default
//!   disposition and re-raise the signal, so the process ends exactly as it
//!   would have — same exit status — only later;
//! - the previous disposition was a host handler, or "ignore": flush only. The
//!   host decides whether and how to exit; it ends the session cleanly by
//!   dropping the launch guard (or calling `stop`) on its way out.
//!
//! A second signal while the default-disposition shutdown is still running
//! terminates immediately: an operator pressing Ctrl+C twice is not made to
//! wait out the flush timeout.
//!
//! # Windows
//!
//! A console control handler. Windows runs it on a thread of its own, so it
//! flushes in place, then returns `FALSE` so the next handler (the host's, or
//! the default) still runs.

use std::io;
use std::time::Duration;

/// Install the hook, or refresh its timeout if it is already installed.
pub(crate) fn install(timeout: Duration) -> io::Result<()> {
    imp::install(timeout)
}

#[cfg(unix)]
mod imp {
    use std::cell::UnsafeCell;
    use std::io;
    use std::mem::MaybeUninit;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use libc::{c_int, c_void};

    use crate::Bugsee;

    const SIGNALS: [c_int; 2] = [libc::SIGTERM, libc::SIGINT];

    /// The dispositions in force before ours, indexed like [`SIGNALS`].
    ///
    /// Written only by `install`, before the `sigaction` that arms the handler,
    /// and read by the handler afterwards — the same discipline the native crash
    /// handler uses for its table.
    struct Previous(UnsafeCell<[MaybeUninit<libc::sigaction>; 2]>);
    // SAFETY: see above; there is no concurrent write once the handler is armed.
    unsafe impl Sync for Previous {}
    static PREVIOUS: Previous = Previous(UnsafeCell::new([MaybeUninit::uninit(); 2]));

    /// Write end of the self-pipe; `-1` until installed.
    static PIPE_W: AtomicI32 = AtomicI32::new(-1);
    /// The process that owns the watcher thread. A `fork` child inherits the
    /// handler but not the thread.
    static OWNER_PID: AtomicU32 = AtomicU32::new(0);
    static TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
    /// Set by the watcher once a terminating shutdown has begun.
    static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
    /// Serialises `install` (never taken by the handler).
    static INSTALL: Mutex<bool> = Mutex::new(false);

    fn index(sig: c_int) -> Option<usize> {
        SIGNALS.iter().position(|s| *s == sig)
    }

    /// The disposition that was in force before ours for `SIGNALS[idx]`.
    ///
    /// # Safety
    /// The table must have been filled in (the handler only runs after that).
    unsafe fn previous(idx: usize) -> libc::sigaction {
        unsafe { (*PREVIOUS.0.get())[idx].assume_init() }
    }

    fn is_default(prev: &libc::sigaction) -> bool {
        prev.sa_sigaction == libc::SIG_DFL
    }

    /// Restore the default disposition and send ourselves `sig`. Every call is
    /// async-signal-safe.
    fn die(sig: c_int) {
        // SAFETY: plain syscalls on a zeroed, filled-in struct.
        unsafe {
            let mut dfl: libc::sigaction = core::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(sig, &dfl, core::ptr::null_mut());
            let mut set: libc::sigset_t = core::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, sig);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, core::ptr::null_mut());
            libc::raise(sig);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe fn errno_location() -> *mut c_int {
        unsafe { libc::__errno_location() }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    unsafe fn errno_location() -> *mut c_int {
        unsafe { libc::__error() }
    }

    /// Async-signal-safe: atomics, `getpid`, `write`, `sigaction`, `raise`, and a
    /// call into the previous handler.
    extern "C" fn handler(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
        // SAFETY: errno access and the calls below are async-signal-safe; the
        // table is filled in before this handler is armed.
        unsafe {
            let saved_errno = *errno_location();
            let Some(idx) = index(sig) else { return };
            let prev = previous(idx);
            let prev_default = is_default(&prev);
            let owner = libc::getpid() as u32 == OWNER_PID.load(Ordering::Relaxed);

            if !owner {
                // A fork child: no watcher thread here. Behave as if we were not
                // installed.
                if prev_default {
                    die(sig);
                }
            } else {
                if prev_default && SHUTTING_DOWN.load(Ordering::SeqCst) {
                    die(sig);
                }
                let fd = PIPE_W.load(Ordering::Relaxed);
                if fd >= 0 {
                    let byte = sig as u8;
                    // A full pipe means wake-ups are already pending: ignore.
                    libc::write(fd, (&byte as *const u8).cast(), 1);
                }
            }

            if !prev_default && prev.sa_sigaction != libc::SIG_IGN {
                if prev.sa_flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                        core::mem::transmute(prev.sa_sigaction);
                    f(sig, info, ctx);
                } else {
                    let f: extern "C" fn(c_int) = core::mem::transmute(prev.sa_sigaction);
                    f(sig);
                }
            }
            *errno_location() = saved_errno;
        }
    }

    fn watcher(read_fd: c_int) {
        let mut byte = 0u8;
        loop {
            // SAFETY: reading one byte into a local.
            let n = unsafe { libc::read(read_fd, (&mut byte as *mut u8).cast(), 1) };
            if n == 0 {
                return;
            }
            if n < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
            let sig = byte as c_int;
            let Some(idx) = index(sig) else { continue };
            // SAFETY: the table is filled in before the pipe can carry anything.
            let prev = unsafe { previous(idx) };
            let timeout = Duration::from_millis(TIMEOUT_MS.load(Ordering::Relaxed));
            if is_default(&prev) {
                // The process is about to end because of this signal: finish
                // cleanly, then let it end the way it would have.
                SHUTTING_DOWN.store(true, Ordering::SeqCst);
                let _ = std::panic::catch_unwind(|| Bugsee::shutdown(timeout));
                die(sig);
                // Still here: the signal did not kill us (blocked or ignored
                // elsewhere). Carry on serving later signals.
                SHUTTING_DOWN.store(false, Ordering::SeqCst);
            } else {
                let _ = std::panic::catch_unwind(|| Bugsee::flush(timeout));
            }
        }
    }

    /// Create the pipe and start the watcher; record the new owner.
    fn start_watcher() -> io::Result<()> {
        let mut fds = [0 as c_int; 2];
        // SAFETY: `fds` has room for the two descriptors `pipe` writes.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let (r, w) = (fds[0], fds[1]);
        // SAFETY: fcntl on descriptors this function owns.
        unsafe {
            for fd in [r, w] {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            // The handler must never block on a full pipe.
            let flags = libc::fcntl(w, libc::F_GETFL);
            libc::fcntl(w, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let spawned = std::thread::Builder::new()
            .name("bugsee-termination".into())
            .spawn(move || watcher(r));
        if let Err(e) = spawned {
            // SAFETY: closing the descriptors created above.
            unsafe {
                libc::close(r);
                libc::close(w);
            }
            return Err(e);
        }
        // The old write end (a fork child's inherited copy, or none) is left
        // open on purpose: closing a descriptor the handler may be using races.
        OWNER_PID.store(std::process::id(), Ordering::SeqCst);
        PIPE_W.store(w, Ordering::SeqCst);
        Ok(())
    }

    pub(super) fn install(timeout: Duration) -> io::Result<()> {
        let mut installed = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
        TIMEOUT_MS.store(
            timeout.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if *installed {
            // Already armed. In a fork child the watcher thread is gone: start one.
            if OWNER_PID.load(Ordering::SeqCst) != std::process::id() {
                SHUTTING_DOWN.store(false, Ordering::SeqCst);
                start_watcher()?;
            }
            return Ok(());
        }

        // Record what is in force now, BEFORE arming.
        for (i, sig) in SIGNALS.iter().enumerate() {
            // SAFETY: querying with a null new-action fills the old one.
            unsafe {
                let mut cur: libc::sigaction = core::mem::zeroed();
                if libc::sigaction(*sig, core::ptr::null(), &mut cur) != 0 {
                    return Err(io::Error::last_os_error());
                }
                (*PREVIOUS.0.get())[i] = MaybeUninit::new(cur);
            }
        }
        start_watcher()?;

        for (i, sig) in SIGNALS.iter().enumerate() {
            // SAFETY: arming our handler with a zeroed + filled struct.
            unsafe {
                let prev = previous(i);
                let mut sa: libc::sigaction = core::mem::zeroed();
                sa.sa_sigaction = handler as *const () as usize;
                // Keep the restart semantics the host chose; with no host
                // handler there is nothing to preserve, and EINTR on every
                // blocking call of every thread would only be a side effect of
                // ours.
                let host_handler = !is_default(&prev) && prev.sa_sigaction != libc::SIG_IGN;
                sa.sa_flags = libc::SA_SIGINFO
                    | if !host_handler || prev.sa_flags & libc::SA_RESTART != 0 {
                        libc::SA_RESTART
                    } else {
                        0
                    };
                libc::sigemptyset(&mut sa.sa_mask);
                if libc::sigaction(*sig, &sa, core::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        *installed = true;
        Ok(())
    }
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::Bugsee;

    const CTRL_CLOSE_EVENT: u32 = 2;
    const CTRL_LOGOFF_EVENT: u32 = 5;
    const CTRL_SHUTDOWN_EVENT: u32 = 6;

    type HandlerRoutine = unsafe extern "system" fn(u32) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
    }

    static TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
    static ARMED: AtomicBool = AtomicBool::new(false);
    static INSTALL: Mutex<()> = Mutex::new(());

    /// Runs on a thread Windows creates for the event, so blocking is fine.
    unsafe extern "system" fn on_event(kind: u32) -> i32 {
        let timeout = Duration::from_millis(TIMEOUT_MS.load(Ordering::Relaxed));
        let _ = std::panic::catch_unwind(|| match kind {
            // Windows ends the process once the handlers return, whatever they
            // answer: this exit is deliberate, so finish cleanly.
            CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
                Bugsee::shutdown(timeout);
            }
            // Ctrl+C / Ctrl+Break: a handler registered before ours may swallow
            // it and keep running, so only flush. The host ends the session by
            // dropping the launch guard.
            _ => {
                Bugsee::flush(timeout);
            }
        });
        // FALSE: pass the event on to the next handler (the host's, or the
        // default one). We never swallow it.
        0
    }

    pub(super) fn install(timeout: Duration) -> io::Result<()> {
        let _lock = INSTALL.lock().unwrap_or_else(|e| e.into_inner());
        TIMEOUT_MS.store(
            timeout.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        if ARMED.load(Ordering::SeqCst) {
            return Ok(());
        }
        // SAFETY: registering a handler routine with the documented signature.
        if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        ARMED.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;
    use std::time::Duration;

    pub(super) fn install(_: Duration) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "termination hooks are not supported on this platform",
        ))
    }
}

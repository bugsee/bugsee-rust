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
//! - the previous disposition was the default (terminate) and ours is still in
//!   force: `shutdown` (flush, then a clean stop, because the exit is
//!   deliberate), restore the default disposition and re-raise the signal, so
//!   the process ends exactly as it would have — same exit status — only later;
//! - anything else — the previous disposition was a host handler, or the host
//!   installed its own handler AFTER ours (tokio, `signal-hook`), which is
//!   detected by asking the kernel what is in force before acting: flush only,
//!   never terminate. The host decides whether and how to exit; it ends the
//!   session cleanly by dropping the launch guard (or calling `stop`);
//! - a signal that was ignored at install time (`SIG_IGN`: a background job,
//!   `nohup`) is left alone and never armed.
//!
//! A second terminating signal while the first is still being served, or any
//! signal when the watcher is gone (or in a `fork` child), terminates at once:
//! an operator pressing Ctrl+C twice is not made to wait out the flush timeout.
//!
//! # Windows
//!
//! A console control handler. Windows runs it on a thread of its own, so it
//! flushes in place, then returns `FALSE` so the next handler (the host's, or
//! the default) still runs. Close, logoff and shutdown also stop the SDK, as
//! the process ends regardless. Ctrl+C and Ctrl+Break only flush: with no host
//! handler the default one ends the process without a clean stop, so the next
//! launch reports an abnormal exit (an earlier handler may instead swallow the
//! event and keep running, which a stop here would wrongly cut short).

use std::io;
use std::time::Duration;

/// Install the hook, or refresh its timeout if it is already installed.
pub(crate) fn install(timeout: Duration) -> io::Result<()> {
    imp::install(timeout)
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use bugsee_core::fork::ForkMutex;
    use libc::{c_int, c_void};

    use crate::Bugsee;

    const SIGNALS: [c_int; 2] = [libc::SIGTERM, libc::SIGINT];

    /// What was in force before ours for one signal. Atomics, because the
    /// handler itself may reset the handler (`SA_RESETHAND` emulation). The
    /// host's signal mask is not kept here: it is installed as ours.
    struct Slot {
        handler: AtomicUsize,
        flags: AtomicI32,
    }

    #[allow(clippy::declare_interior_mutable_const)]
    const SLOT: Slot = Slot {
        handler: AtomicUsize::new(libc::SIG_DFL),
        flags: AtomicI32::new(0),
    };
    static SLOTS: [Slot; 2] = [SLOT; 2];

    /// Write end of the self-pipe; `-1` until installed.
    static PIPE_W: AtomicI32 = AtomicI32::new(-1);
    /// The process that owns the watcher thread. A `fork` child inherits the
    /// handler but not the thread. Stored AFTER `PIPE_W` (release) and loaded
    /// before it (acquire), so an owner always sees its own pipe.
    static OWNER_PID: AtomicU32 = AtomicU32::new(0);
    /// Whether the watcher thread is running to act on a pipe byte.
    static WATCHER_ALIVE: AtomicBool = AtomicBool::new(false);
    /// Terminating signals seen since the watcher last handled one. A second
    /// one means the operator is impatient: die at once.
    static PENDING: AtomicU32 = AtomicU32::new(0);
    static TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
    /// Whether the handler is armed. Serialises `install`; the lock is never
    /// taken by the handler, and being a fork-aware lock a child never inherits
    /// it held by a vanished thread.
    static INSTALL: ForkMutex<bool> = ForkMutex::new(|| false);

    fn index(sig: c_int) -> Option<usize> {
        SIGNALS.iter().position(|s| *s == sig)
    }

    /// Whether our handler is still the one in force for `sig`. If not, someone
    /// installed over us (the host after launch, e.g. tokio or `signal-hook`) and
    /// owns the signal: we then only flush and never terminate the process.
    fn current_is_ours(sig: c_int) -> bool {
        // SAFETY: querying with a null new-action; async-signal-safe.
        unsafe {
            let mut cur: libc::sigaction = core::mem::zeroed();
            libc::sigaction(sig, core::ptr::null(), &mut cur) == 0
                && cur.sa_sigaction == handler as *const () as usize
        }
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
        // slot is filled in before this handler is armed.
        unsafe {
            let saved_errno = *errno_location();
            let Some(idx) = index(sig) else { return };
            let slot = &SLOTS[idx];
            let prev = slot.handler.load(Ordering::SeqCst);
            let prev_default = prev == libc::SIG_DFL;
            let owner = OWNER_PID.load(Ordering::Acquire) == libc::getpid() as u32;
            let live = owner && WATCHER_ALIVE.load(Ordering::SeqCst);

            if prev_default && current_is_ours(sig) {
                // Nobody to flush (a fork child, or the watcher is gone), or a
                // second signal while the first is still being served: behave
                // as if we were not installed.
                if !live || PENDING.fetch_add(1, Ordering::SeqCst) >= 1 {
                    die(sig);
                }
            }
            if live {
                let fd = PIPE_W.load(Ordering::Acquire);
                if fd >= 0 {
                    let byte = sig as u8;
                    // A full pipe means wake-ups are already pending: ignore.
                    libc::write(fd, (&byte as *const u8).cast(), 1);
                }
            }

            if !prev_default && prev != libc::SIG_IGN {
                let flags = slot.flags.load(Ordering::SeqCst);
                if flags & libc::SA_RESETHAND != 0 {
                    // The host's one-shot handler has now fired: from here on
                    // the previous disposition is the default again.
                    slot.handler.store(libc::SIG_DFL, Ordering::SeqCst);
                }
                if flags & libc::SA_SIGINFO != 0 {
                    let f: extern "C" fn(c_int, *mut libc::siginfo_t, *mut c_void) =
                        core::mem::transmute(prev);
                    f(sig, info, ctx);
                } else {
                    let f: extern "C" fn(c_int) = core::mem::transmute(prev);
                    f(sig);
                }
            }
            *errno_location() = saved_errno;
        }
    }

    /// Clears [`WATCHER_ALIVE`] however the watcher thread ends.
    struct AliveGuard;
    impl Drop for AliveGuard {
        fn drop(&mut self) {
            WATCHER_ALIVE.store(false, Ordering::SeqCst);
        }
    }

    fn watcher(read_fd: c_int) {
        let _alive = AliveGuard;
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
            let timeout = Duration::from_millis(TIMEOUT_MS.load(Ordering::Relaxed));
            let prev_default = SLOTS[idx].handler.load(Ordering::SeqCst) == libc::SIG_DFL;
            if prev_default && current_is_ours(sig) {
                // The process is about to end because of this signal: finish
                // cleanly, then let it end the way it would have.
                let _ = std::panic::catch_unwind(|| Bugsee::shutdown(timeout));
                // The host may have taken the signal over meanwhile.
                if current_is_ours(sig) {
                    die(sig);
                }
                // Still here: the signal did not kill us (blocked or taken over).
                // Carry on serving later signals.
                PENDING.store(0, Ordering::SeqCst);
            } else {
                // Host-handled (its handler was before ours, or replaced ours):
                // the host decides whether and how to exit.
                let _ = std::panic::catch_unwind(|| Bugsee::flush(timeout));
            }
        }
    }

    /// Create the pipe and start the watcher; record the new owner.
    fn start_watcher() -> io::Result<()> {
        let mut fds = [0 as c_int; 2];
        // SAFETY: `fds` has room for the two descriptors `pipe` writes.
        unsafe {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            if libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                if libc::pipe(fds.as_mut_ptr()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                for fd in fds {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
        }
        let (r, w) = (fds[0], fds[1]);
        // SAFETY: fcntl on a descriptor this function owns. The handler must
        // never block on a full pipe (the read end stays blocking).
        unsafe {
            let flags = libc::fcntl(w, libc::F_GETFL);
            libc::fcntl(w, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        WATCHER_ALIVE.store(true, Ordering::SeqCst);
        let spawned = std::thread::Builder::new()
            .name("bugsee-termination".into())
            .spawn(move || watcher(r));
        if let Err(e) = spawned {
            WATCHER_ALIVE.store(false, Ordering::SeqCst);
            // SAFETY: closing the descriptors created above.
            unsafe {
                libc::close(r);
                libc::close(w);
            }
            return Err(e);
        }
        // The old write end (a fork child's inherited copy, or none) is left
        // open on purpose: closing a descriptor the handler may be using races.
        PIPE_W.store(w, Ordering::Release);
        OWNER_PID.store(std::process::id(), Ordering::Release);
        Ok(())
    }

    pub(super) fn install(timeout: Duration) -> io::Result<()> {
        let mut installed = INSTALL.lock();
        TIMEOUT_MS.store(
            timeout.as_millis().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        // First install, a retry after a failed one, or a fork child that
        // inherited the handler without its watcher thread.
        if OWNER_PID.load(Ordering::Acquire) != std::process::id() {
            PENDING.store(0, Ordering::SeqCst);
            start_watcher()?;
        }
        if *installed {
            return Ok(());
        }

        let mut armed: Vec<(c_int, libc::sigaction)> = Vec::new();
        for (i, sig) in SIGNALS.iter().enumerate() {
            // SAFETY: querying, then arming, with zeroed + filled structs. The
            // slot is filled in before the `sigaction` that arms the handler.
            let result = unsafe {
                let mut cur: libc::sigaction = core::mem::zeroed();
                if libc::sigaction(*sig, core::ptr::null(), &mut cur) != 0 {
                    Err(io::Error::last_os_error())
                } else if cur.sa_sigaction == libc::SIG_IGN {
                    // Ignored on purpose (a shell running us in the background,
                    // `nohup`): that is the host's choice, leave it.
                    continue;
                } else {
                    let slot = &SLOTS[i];
                    slot.handler.store(cur.sa_sigaction, Ordering::SeqCst);
                    slot.flags.store(cur.sa_flags, Ordering::SeqCst);

                    let host_handler = cur.sa_sigaction != libc::SIG_DFL;
                    let mut sa: libc::sigaction = core::mem::zeroed();
                    sa.sa_sigaction = handler as *const () as usize;
                    // The host's mask, stack and restart choices carry over to
                    // the chained call. With no host handler there is nothing to
                    // preserve, and EINTR on every blocking call of every thread
                    // would only be a side effect of ours.
                    sa.sa_flags = libc::SA_SIGINFO
                        | (cur.sa_flags & (libc::SA_RESTART | libc::SA_ONSTACK | libc::SA_NODEFER))
                        | if host_handler { 0 } else { libc::SA_RESTART };
                    sa.sa_mask = cur.sa_mask;
                    if libc::sigaction(*sig, &sa, core::ptr::null_mut()) != 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        armed.push((*sig, cur));
                        Ok(())
                    }
                }
            };
            if let Err(e) = result {
                // Leave nothing half-armed, or a retry would record our own
                // handler as "the previous one" and recurse into itself.
                for (sig, old) in armed {
                    // SAFETY: restoring what was read above.
                    unsafe { libc::sigaction(sig, &old, core::ptr::null_mut()) };
                }
                return Err(e);
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

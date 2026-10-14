//
//  crash_flush.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Best-effort flush of in-flight capture entries at crash time (DESIGN §12).
//!
//! # What is lost without this
//!
//! A producer thread never writes to disk: it pushes the entry into the capture
//! worker's channel and returns. The worker serializes and appends it to the
//! part file a little later. Anything still in the channel (or in the worker's
//! hands) when the process dies is gone, and those are exactly the last
//! breadcrumbs, log lines and network events before the fault, the ones a crash
//! report is read for. The window is normally microseconds, but it grows to
//! many milliseconds under a burst, or when the worker is descheduled.
//!
//! # What this does
//!
//! A fatal-crash handler cannot take the channel lock, allocate, or serialize
//! (the faulting thread may hold the allocator or the lock). So it does not
//! touch the queue at all. It performs one `write(2)` / `SetEvent` on a
//! pre-created wake handle and then **polls an atomic** for a bounded time:
//!
//! 1. a dedicated, ordinary thread (created at launch, long before any crash)
//!    is blocked reading that handle;
//! 2. woken, it posts a `Flush` marker to the capture worker through the
//!    normal channel. The channel is FIFO, so by the time the worker
//!    acknowledges the marker every entry queued before the crash has been
//!    serialized and appended to its part file;
//! 3. the helper publishes the acknowledgement in an atomic, which the waiting
//!    crash handler observes and then proceeds to terminate.
//!
//! The crash handler's side ([`flush_in_flight`]) uses only an atomic load and
//! `fetch_add`, `getpid`, one non-blocking `write`, `clock_gettime` and
//! `nanosleep` (Windows: `SetEvent`, `Sleep`, `GetTickCount64`) — all
//! async-signal-safe. There is no allocation and no lock, and the wait is capped
//! by the caller's budget, so a stuck or dead worker (or a corrupted heap that
//! blocks the helper in the allocator) costs the budget and no more. It is a
//! best effort: when the budget runs out we give up and the process dies
//! exactly as it would have without this module.
//!
//! The expensive work (JSON, file I/O) runs on the worker, with the process
//! otherwise healthy; the crashing thread is only parked. Other threads keep
//! running while a *synchronous* fault is handled, which is what makes
//! this possible, and also why events emitted by other threads in the meantime
//! may also land.
//!
//! # Limits (deliberate, documented tradeoffs)
//!
//! * Costs nothing on the happy path (no extra copy or serialization on the
//!   producer) but depends on the worker and allocator being alive. A fault that
//!   corrupts the heap, or kills the worker thread itself, forfeits the flush.
//! * Entries are flushed to the OS page cache like every other append; this
//!   survives the process dying, not the machine.
//! * A fork child has none of the parent's threads: the pid recorded at
//!   registration lets the handler skip the flush there until the child's
//!   revived recorder registers again.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Flush requests issued by crash handlers (a ticket counter).
static REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Highest ticket the helper has seen acknowledged by the worker.
static ACKED: AtomicU64 = AtomicU64::new(0);

/// Owns the helper thread and the wake handle for one capture pipeline.
/// Dropping it deregisters (if it is still the registered one) and stops the
/// thread.
pub struct CrashFlush {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    handle: imp::Handle,
}

impl CrashFlush {
    /// Start the helper thread and make this the process's registered flusher.
    ///
    /// `flush` is called on the helper thread each time a crash handler asks, and
    /// must return `true` once everything queued before the call is on disk. It
    /// must bound its own waiting.
    pub fn install(flush: impl Fn() -> bool + Send + 'static) -> std::io::Result<CrashFlush> {
        let stop = Arc::new(AtomicBool::new(false));
        let (handle, waiter) = imp::create()?;
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("bugsee-crash-flush".into())
            .spawn(move || {
                // `waiter` is closed by its own Drop when the loop ends.
                while imp::wait(&waiter) {
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let target = REQUESTED.load(Ordering::Acquire);
                    if target <= ACKED.load(Ordering::Acquire) {
                        continue;
                    }
                    let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&flush))
                        .unwrap_or(false);
                    if ok {
                        ACKED.fetch_max(target, Ordering::AcqRel);
                    }
                }
            })?;
        imp::register(&handle);
        Ok(CrashFlush {
            stop,
            thread: Some(thread),
            handle,
        })
    }
}

impl Drop for CrashFlush {
    fn drop(&mut self) {
        imp::deregister(&self.handle);
        self.stop.store(true, Ordering::Release);
        imp::wake(&self.handle);
        if let Some(thread) = self.thread.take() {
            // The helper polls `stop` only between flushes; a flush is bounded by
            // the closure's own timeout, so this join is too.
            let _ = thread.join();
        }
        imp::close(&mut self.handle);
    }
}

/// Ask the registered capture pipeline to drain everything queued so far, and
/// wait at most `budget` for it to finish. Returns whether it confirmed.
///
/// **Async-signal-safe**: callable from a signal handler or exception filter.
/// No allocation, no locks, bounded time. Returns `false` immediately when no
/// pipeline is registered or the registration belongs to another process (a
/// `fork()` child).
pub fn flush_in_flight(budget: Duration) -> bool {
    imp::request_and_wait(budget)
}

#[cfg(unix)]
mod imp {
    use super::{ACKED, REQUESTED};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    /// Write end of the wake pipe the handler writes to (`-1` when none).
    static WAKE_FD: AtomicI32 = AtomicI32::new(-1);
    /// Pid that created the registration (a fork child must not use it).
    static OWNER_PID: AtomicI32 = AtomicI32::new(0);

    pub struct Handle {
        write_fd: i32,
    }

    /// The read end, closed when the helper thread drops it.
    pub struct Waiter(i32);

    impl Drop for Waiter {
        fn drop(&mut self) {
            // SAFETY: closing a descriptor this value owns.
            unsafe { libc::close(self.0) };
        }
    }

    pub fn create() -> std::io::Result<(Handle, Waiter)> {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a valid two-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just created and are ours.
        unsafe {
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            // The handler must never block on a full pipe.
            let fl = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Ok((Handle { write_fd: fds[1] }, Waiter(fds[0])))
    }

    /// Block until woken. `false` means the pipe is gone: stop.
    pub fn wait(w: &Waiter) -> bool {
        let mut buf = [0u8; 64];
        loop {
            // SAFETY: reads into a valid buffer.
            let n = unsafe { libc::read(w.0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                return true;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
    }

    pub fn register(h: &Handle) {
        // SAFETY: getpid has no preconditions.
        OWNER_PID.store(unsafe { libc::getpid() }, Ordering::Release);
        WAKE_FD.store(h.write_fd, Ordering::Release);
    }

    pub fn deregister(h: &Handle) {
        let _ = WAKE_FD.compare_exchange(h.write_fd, -1, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn wake(h: &Handle) {
        let b = 1u8;
        // SAFETY: writes one byte from a valid buffer; non-blocking.
        unsafe { libc::write(h.write_fd, (&b as *const u8).cast(), 1) };
    }

    pub fn close(h: &mut Handle) {
        if h.write_fd >= 0 {
            // SAFETY: closing the descriptor this handle owns, once.
            unsafe { libc::close(h.write_fd) };
            h.write_fd = -1;
        }
    }

    fn now_ns() -> u64 {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime is async-signal-safe and `ts` is valid.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    }

    pub fn request_and_wait(budget: Duration) -> bool {
        let fd = WAKE_FD.load(Ordering::Acquire);
        // SAFETY: getpid is async-signal-safe.
        if fd < 0 || OWNER_PID.load(Ordering::Acquire) != unsafe { libc::getpid() } {
            return false;
        }
        let ticket = REQUESTED.fetch_add(1, Ordering::AcqRel) + 1;
        let b = 1u8;
        // SAFETY: async-signal-safe write of one byte. A full pipe (EAGAIN) means
        // wakeups are already pending, and the helper reads the ticket counter.
        unsafe { libc::write(fd, (&b as *const u8).cast(), 1) };
        let deadline = now_ns().saturating_add(budget.as_nanos().min(u64::MAX as u128) as u64);
        while ACKED.load(Ordering::Acquire) < ticket {
            if now_ns() >= deadline {
                return false;
            }
            let ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 200_000,
            };
            // SAFETY: nanosleep is async-signal-safe.
            unsafe { libc::nanosleep(&ts, core::ptr::null_mut()) };
        }
        true
    }
}

#[cfg(windows)]
mod imp {
    use super::{ACKED, REQUESTED};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    type Hnd = usize;

    extern "system" {
        fn CreateEventW(attrs: *const u8, manual: i32, initial: i32, name: *const u16) -> Hnd;
        fn SetEvent(h: Hnd) -> i32;
        fn WaitForSingleObject(h: Hnd, ms: u32) -> u32;
        fn CloseHandle(h: Hnd) -> i32;
        fn Sleep(ms: u32);
        fn GetTickCount64() -> u64;
    }

    /// The auto-reset event the handler signals (`0` when none).
    static WAKE: AtomicUsize = AtomicUsize::new(0);

    pub struct Handle(Hnd);
    pub struct Waiter(Hnd);

    pub fn create() -> std::io::Result<(Handle, Waiter)> {
        // SAFETY: plain Win32 call; auto-reset, initially unsignalled, unnamed.
        let h = unsafe { CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()) };
        if h == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((Handle(h), Waiter(h)))
    }

    /// Block until signalled (`WAIT_OBJECT_0`).
    pub fn wait(w: &Waiter) -> bool {
        // SAFETY: waits on an event handle that outlives the thread (it is closed
        // only after the thread is joined).
        unsafe { WaitForSingleObject(w.0, u32::MAX) == 0 }
    }

    pub fn register(h: &Handle) {
        WAKE.store(h.0, Ordering::Release);
    }

    pub fn deregister(h: &Handle) {
        let _ = WAKE.compare_exchange(h.0, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn wake(h: &Handle) {
        // SAFETY: signals a live event.
        unsafe { SetEvent(h.0) };
    }

    pub fn close(h: &mut Handle) {
        if h.0 != 0 {
            // SAFETY: closes the handle this value owns, once.
            unsafe { CloseHandle(h.0) };
            h.0 = 0;
        }
    }

    pub fn request_and_wait(budget: Duration) -> bool {
        let h = WAKE.load(Ordering::Acquire);
        if h == 0 {
            return false;
        }
        let ticket = REQUESTED.fetch_add(1, Ordering::AcqRel) + 1;
        // SAFETY: SetEvent/Sleep/GetTickCount64 take no locks we could be holding.
        unsafe {
            SetEvent(h);
            let deadline = GetTickCount64().saturating_add(budget.as_millis() as u64);
            while ACKED.load(Ordering::Acquire) < ticket {
                if GetTickCount64() >= deadline {
                    return false;
                }
                Sleep(1);
            }
        }
        true
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::time::Duration;

    pub struct Handle;
    pub struct Waiter;

    pub fn create() -> std::io::Result<(Handle, Waiter)> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
    pub fn wait(_: &Waiter) -> bool {
        false
    }
    pub fn register(_: &Handle) {}
    pub fn deregister(_: &Handle) {}
    pub fn wake(_: &Handle) {}
    pub fn close(_: &mut Handle) {}
    pub fn request_and_wait(_: Duration) -> bool {
        false
    }
}

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
//! breadcrumbs, log lines and network events before the fault. The window is
//! microseconds when idle, but grows to many milliseconds under a burst or when
//! the worker is descheduled.
//!
//! # What this does
//!
//! A fatal-crash handler cannot take the channel lock, allocate, or serialize
//! (the faulting thread may hold the allocator or the lock). So it does not
//! touch the queue at all. It performs one `write(2)` / `SetEvent` on a
//! process-wide wake handle created at first launch, then waits for an atomic
//! acknowledgement for a bounded time:
//!
//! 1. one helper thread per process (created at launch, long before any crash;
//!    re-created in a `fork()` child) is blocked on that handle;
//! 2. woken, it calls the registered flush target, which posts a `Flush` marker
//!    to the capture worker through the normal channel. The channel is FIFO, so
//!    when the worker acknowledges the marker every entry queued before the
//!    crash has been serialized and appended;
//! 3. the helper publishes the acknowledgement in an atomic, which the waiting
//!    handler observes before letting the process die.
//!
//! The handler's side ([`flush_for_crash`]) uses only atomics, `getpid`, one
//! non-blocking `write`, `clock_gettime` and `poll` (Windows: `SetEvent`,
//! `WaitForSingleObject`, `QueryPerformanceCounter`), all async-signal-safe: no
//! allocation, no lock. The wait is capped by the caller's budget, so a stuck or
//! dead worker (or a corrupted heap that blocks the helper in the allocator)
//! costs the budget and no more.
//!
//! The wake handles are **never closed** (one pipe or event pair per process,
//! not per recorder), so a handler racing a recorder's shutdown can never write
//! to a recycled descriptor, and the read end outlives every writer, so a C host
//! (through `bugsee-ffi`) never sees `SIGPIPE`.
//!
//! # The dying flag
//!
//! [`flush_for_crash`] first raises a process-wide flag. During the (up to
//! budget) window the process is still running other threads, and an uploader
//! that started a report now would be killed halfway through a claim. While the
//! flag is up the runtime starts no claims and no recoveries ([`is_dying`]).
//!
//! # Limits (deliberate, documented tradeoffs)
//!
//! * Costs nothing on the happy path (no extra copy or serialization on the
//!   producer) but depends on the worker and allocator being alive. A fault that
//!   corrupts the heap, or kills the worker thread itself, forfeits the flush.
//!   A fault on the worker or the helper thread skips the wait entirely.
//! * Entries are flushed to the OS page cache like every other append; this
//!   survives the process dying, not the machine.
//! * **Last registered wins.** The flush target is process-wide: a second
//!   `Recorder` launched in the same process takes over, and dropping a recorder
//!   that is not the current target does nothing.
//! * A fork child has none of the parent's threads: the pid recorded at
//!   registration makes the handler skip the flush there until the child's
//!   revived recorder registers again (which re-creates the pipe and helper).
//! * **Windows ordering:** the flush runs inside the unhandled-exception filter,
//!   before the host's previously installed filter (if any) is called, so a host
//!   reporter that runs afterwards sees the process as it was.
//! * A panic hook that calls [`flush_in_flight`] for a main-thread panic a host
//!   goes on to catch outside our guards pays up to the budget once for it.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::fork::ForkMutex;

/// How long a dying process waits for the capture worker by default.
pub const DEFAULT_BUDGET: Duration = Duration::from_millis(50);

/// Flush requests issued by crash handlers (a ticket counter).
static REQUESTED: AtomicU64 = AtomicU64::new(0);
/// Highest ticket the helper has seen acknowledged by the worker.
static ACKED: AtomicU64 = AtomicU64::new(0);
/// Raised by a fatal-crash handler: the process is about to die.
static DYING: AtomicBool = AtomicBool::new(false);

/// Threads whose own crash must not wait for the flush (the worker and helper:
/// they cannot answer while handling a fault).
const PIPELINE_SLOTS: usize = 8;
static PIPELINE_THREADS: [AtomicUsize; PIPELINE_SLOTS] =
    [const { AtomicUsize::new(0) }; PIPELINE_SLOTS];

type Target = Arc<dyn Fn() -> bool + Send + Sync>;

struct State {
    target: Option<(u64, Target)>,
    next_id: u64,
    /// Process that created the helper and pipe (`0`: none yet). A different
    /// value means we are in a `fork()` child, where they do not exist.
    helper_pid: u32,
}

static STATE: ForkMutex<State> = ForkMutex::new(|| State {
    target: None,
    next_id: 1,
    helper_pid: 0,
});

/// The registered flush target. Dropping it deregisters (if still current).
pub struct CrashFlush {
    id: u64,
}

impl CrashFlush {
    /// Make `flush` the process's crash-time flush target, starting the helper
    /// thread and wake handle if this process has none yet.
    ///
    /// `flush` is called on the helper thread each time a crash handler asks, and
    /// must return `true` once everything queued before the call is on disk. It
    /// must bound its own waiting.
    pub fn install(
        flush: impl Fn() -> bool + Send + Sync + 'static,
    ) -> std::io::Result<CrashFlush> {
        let mut st = STATE.lock();
        let pid = std::process::id();
        if st.helper_pid != pid {
            imp::start_helper()?;
            st.helper_pid = pid;
        }
        let id = st.next_id;
        st.next_id += 1;
        st.target = Some((id, Arc::new(flush)));
        Ok(CrashFlush { id })
    }
}

impl Drop for CrashFlush {
    fn drop(&mut self) {
        let mut st = STATE.lock();
        if st.target.as_ref().is_some_and(|(id, _)| *id == self.id) {
            st.target = None;
        }
    }
}

/// The current flush target, for the helper thread.
fn current_target() -> Option<Target> {
    STATE.lock().target.as_ref().map(|(_, t)| Arc::clone(t))
}

/// Marks the current thread as part of the capture pipeline for its lifetime.
pub struct PipelineThread(usize);

/// Record that the calling thread is a pipeline thread (see [`PIPELINE_SLOTS`]).
pub fn register_pipeline_thread() -> PipelineThread {
    let me = imp::thread_id();
    for slot in &PIPELINE_THREADS {
        if slot
            .compare_exchange(0, me, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return PipelineThread(me);
        }
    }
    PipelineThread(0)
}

impl Drop for PipelineThread {
    fn drop(&mut self) {
        if self.0 == 0 {
            return;
        }
        for slot in &PIPELINE_THREADS {
            let _ = slot.compare_exchange(self.0, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

fn on_pipeline_thread() -> bool {
    let me = imp::thread_id();
    PIPELINE_THREADS
        .iter()
        .any(|s| s.load(Ordering::Acquire) == me)
}

/// Has a fatal-crash handler declared the process about to die? The runtime
/// starts no report claims or recoveries while this is set.
pub fn is_dying() -> bool {
    DYING.load(Ordering::Acquire)
}

/// Clear the dying flag. Tests only.
#[doc(hidden)]
pub fn reset_dying_for_test() {
    DYING.store(false, Ordering::Release);
}

/// For a fatal-crash handler: declare the process dying, then ask the registered
/// capture pipeline to drain everything queued so far, waiting at most `budget`.
/// Returns whether it confirmed.
///
/// **Async-signal-safe**: callable from a signal handler or exception filter.
/// No allocation, no locks, bounded time.
pub fn flush_for_crash(budget: Duration) -> bool {
    DYING.store(true, Ordering::Release);
    flush_in_flight(budget)
}

/// Like [`flush_for_crash`] without declaring the process dying: for a caller
/// that only *expects* to die (a main-thread panic hook). Returns immediately
/// when no pipeline is registered, the registration belongs to another process
/// (a `fork()` child), or the caller is itself a pipeline thread.
pub fn flush_in_flight(budget: Duration) -> bool {
    if on_pipeline_thread() {
        return false;
    }
    imp::request_and_wait(budget)
}

/// Drain: bump the ack to `target` after a successful flush.
fn ack(target: u64) {
    ACKED.fetch_max(target, Ordering::AcqRel);
}

/// What the helper does each time it is woken.
fn serve_request() {
    let target = REQUESTED.load(Ordering::Acquire);
    if target <= ACKED.load(Ordering::Acquire) {
        return;
    }
    let ok = match current_target() {
        Some(flush) => {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| flush())).unwrap_or(false)
        }
        // Nothing registered: nothing is queued anywhere that we could drain.
        None => true,
    };
    if ok {
        ack(target);
        imp::signal_ack();
    }
}

#[cfg(unix)]
mod imp {
    use super::{serve_request, ACKED, PIPELINE_THREADS, REQUESTED};
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration;

    /// Write end of the wake pipe (`-1` when none). Never closed once set.
    static WAKE_FD: AtomicI32 = AtomicI32::new(-1);
    /// Read end, kept only so a fork child can close the parent's copy.
    static READ_FD: AtomicI32 = AtomicI32::new(-1);
    /// Pid that owns `WAKE_FD` (a fork child must not use it).
    static OWNER_PID: AtomicI32 = AtomicI32::new(0);

    pub fn thread_id() -> usize {
        // SAFETY: pthread_self is async-signal-safe and has no preconditions.
        unsafe { libc::pthread_self() as usize }
    }

    fn make_pipe() -> std::io::Result<(i32, i32)> {
        let mut fds = [0i32; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: `fds` is a valid two-element array.
        let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: as above.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both descriptors were just created and are ours.
        unsafe {
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            for fd in fds {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            // The handler must never block on a full pipe.
            let fl = libc::fcntl(fds[1], libc::F_GETFL);
            libc::fcntl(fds[1], libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        Ok((fds[0], fds[1]))
    }

    /// Create the pipe and the one helper thread for this process. In a fork
    /// child this replaces the parent's (which are closed here, after the new
    /// ones are published).
    pub fn start_helper() -> std::io::Result<()> {
        // Unpublish first, so a handler that runs mid-switch sees nothing.
        let old_w = WAKE_FD.swap(-1, Ordering::AcqRel);
        let old_r = READ_FD.swap(-1, Ordering::AcqRel);
        // SAFETY: getpid has no preconditions.
        OWNER_PID.store(unsafe { libc::getpid() }, Ordering::Release);
        for slot in &PIPELINE_THREADS {
            slot.store(0, Ordering::Release);
        }
        // Tickets of the parent's requests are meaningless here.
        ACKED.store(REQUESTED.load(Ordering::Acquire), Ordering::Release);

        let (r, w) = make_pipe()?;
        let spawned = std::thread::Builder::new()
            .name("bugsee-crash-flush".into())
            .spawn(move || {
                let _me = super::register_pipeline_thread();
                let mut buf = [0u8; 64];
                loop {
                    // SAFETY: reads into a valid buffer from a descriptor that is
                    // never closed while this thread runs.
                    let n = unsafe { libc::read(r, buf.as_mut_ptr().cast(), buf.len()) };
                    if n > 0 {
                        serve_request();
                    } else if n < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                        continue;
                    } else {
                        break;
                    }
                }
            });
        if let Err(e) = spawned {
            // SAFETY: closing descriptors made above and shared with nobody.
            unsafe {
                libc::close(r);
                libc::close(w);
            }
            return Err(e);
        }
        READ_FD.store(r, Ordering::Release);
        WAKE_FD.store(w, Ordering::Release);
        // The parent's copies are of no use here.
        for fd in [old_w, old_r] {
            if fd >= 0 {
                // SAFETY: inherited descriptors only this child's code could use,
                // unpublished above.
                unsafe { libc::close(fd) };
            }
        }
        Ok(())
    }

    pub fn signal_ack() {}

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
            // SAFETY: poll with no descriptors is a plain async-signal-safe sleep.
            unsafe { libc::poll(core::ptr::null_mut(), 0, 1) };
        }
        true
    }
}

#[cfg(windows)]
mod imp {
    use super::{serve_request, ACKED, REQUESTED};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    type Hnd = usize;

    extern "system" {
        fn CreateEventW(attrs: *const u8, manual: i32, initial: i32, name: *const u16) -> Hnd;
        fn SetEvent(h: Hnd) -> i32;
        fn WaitForSingleObject(h: Hnd, ms: u32) -> u32;
        fn GetCurrentThreadId() -> u32;
        fn QueryPerformanceCounter(v: *mut i64) -> i32;
        fn QueryPerformanceFrequency(v: *mut i64) -> i32;
    }

    /// Auto-reset events, never closed: the handler signals `WAKE`, the helper
    /// signals `ACK`. (`0` when none.)
    static WAKE: AtomicUsize = AtomicUsize::new(0);
    static ACK: AtomicUsize = AtomicUsize::new(0);

    pub fn thread_id() -> usize {
        // SAFETY: plain Win32 call.
        unsafe { GetCurrentThreadId() as usize }
    }

    pub fn start_helper() -> std::io::Result<()> {
        // SAFETY: plain Win32 calls; auto-reset, initially unsignalled, unnamed.
        let (wake, ack) = unsafe {
            (
                CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()),
                CreateEventW(core::ptr::null(), 0, 0, core::ptr::null()),
            )
        };
        if wake == 0 || ack == 0 {
            return Err(std::io::Error::last_os_error());
        }
        std::thread::Builder::new()
            .name("bugsee-crash-flush".into())
            .spawn(move || {
                let _me = super::register_pipeline_thread();
                loop {
                    // SAFETY: waits on an event that is never closed.
                    if unsafe { WaitForSingleObject(wake, u32::MAX) } != 0 {
                        break;
                    }
                    serve_request();
                }
            })?;
        ACK.store(ack, Ordering::Release);
        WAKE.store(wake, Ordering::Release);
        Ok(())
    }

    pub fn signal_ack() {
        let h = ACK.load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: signals a live event.
            unsafe { SetEvent(h) };
        }
    }

    fn now_ticks() -> (i64, i64) {
        let (mut t, mut f) = (0i64, 1i64);
        // SAFETY: valid out-pointers; both calls cannot fail on supported Windows.
        unsafe {
            QueryPerformanceCounter(&mut t);
            QueryPerformanceFrequency(&mut f);
        }
        (t, f.max(1))
    }

    pub fn request_and_wait(budget: Duration) -> bool {
        let wake = WAKE.load(Ordering::Acquire);
        let ack = ACK.load(Ordering::Acquire);
        if wake == 0 || ack == 0 {
            return false;
        }
        let ticket = REQUESTED.fetch_add(1, Ordering::AcqRel) + 1;
        let (start, freq) = now_ticks();
        let budget_ticks = (budget.as_secs_f64() * freq as f64) as i64;
        // SAFETY: SetEvent/WaitForSingleObject take no locks we could be holding.
        unsafe { SetEvent(wake) };
        while ACKED.load(Ordering::Acquire) < ticket {
            let (now, _) = now_ticks();
            let left = budget_ticks - (now - start);
            if left <= 0 {
                return false;
            }
            // Wake on the helper's ack; the slice cap covers another waiter having
            // consumed the (auto-reset) signal.
            let ms = ((left as f64 / freq as f64 * 1000.0).ceil() as u32).clamp(1, 5);
            // SAFETY: as above.
            unsafe { WaitForSingleObject(ack, ms) };
        }
        true
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::time::Duration;

    pub fn thread_id() -> usize {
        0
    }
    pub fn start_helper() -> std::io::Result<()> {
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    }
    pub fn signal_ack() {}
    pub fn request_and_wait(_: Duration) -> bool {
        false
    }
}

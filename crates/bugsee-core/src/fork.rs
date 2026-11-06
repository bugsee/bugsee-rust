//
//  fork.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Surviving `fork()`.
//!
//! PHP-FPM, Puma/Unicorn and the worker pools of many runtimes `fork()` after the
//! SDK has started. The child inherits a copy of the process with **only the
//! forking thread alive**: the SDK's capture and uploader threads are gone, and
//! any lock another thread held at that instant stays locked forever.
//!
//! The standard cure for the locks is the `pthread_atfork` lock protocol, and
//! this module is that protocol: every SDK lock that can be held across the
//! fork registers here; just *before* `fork` the forking thread takes them all
//! (so no other thread can be holding one), and right *after* it releases them —
//! in the parent and in the child. The child therefore inherits every lock
//! free, with consistent data, instead of one a vanished thread owned.
//!
//! What it deliberately does NOT do in the child's `atfork` hook is rebuild
//! anything: between `fork` and `exec` a multithreaded process's child may only
//! do async-signal-safe things. The hook releases locks and bumps [`fork_epoch`];
//! the SDK notices the new epoch on its next call and rebuilds then.
//!
//! The protocol needs locks that can be released *without* a guard value (they
//! are taken in one callback and released in another), so this module uses its
//! own small spin lock rather than `std::sync::Mutex`. The critical sections
//! guarded are a few instructions long (enqueue, clone a handle), which is the
//! regime where a spin-then-yield lock is the right tool.
//!
//! Lock-order rule: every registered lock has a **rank** and `prepare` takes the
//! locks in ascending rank, regardless of when each one registered (statics
//! register lazily on first use, so registration order says nothing about
//! nesting). SDK code that holds two registered locks at once must take them in
//! ascending rank too, or `prepare` could deadlock against it. The ranks in use:
//! [`ForkMutex::ranked`] globals that nest (the facade's recorder lock and the
//! locks taken under it), then [`RANK_INSTANCE`] for per-recorder locks (which
//! may be taken under the facade lock), then [`RANK_LEAF`] for everything else.
//! A leaf lock may be taken while a lower-ranked one is held, but no other
//! registered lock may be taken while a leaf is held.
//!
//! Registration itself takes the registry lock, which `prepare` holds while it
//! waits for every registered lock. So a lock must never be *registered for the
//! first time* while another registered lock is held: call
//! [`ForkMutex::ensure_registered`] on such a lock up front (the facade does, at
//! launch).
//!
//! Windows has no `fork`, so there the protocol is inert (the locks still work).

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Once, Weak};

static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Number of times this process has been the child of a `fork()` since the hook
/// was installed. Compare against a remembered value to detect "forked since I
/// last looked".
pub fn fork_epoch() -> u64 {
    EPOCH.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// The lock
// ---------------------------------------------------------------------------

/// A guard-less spin-then-yield lock: `lock` and `unlock` may happen in
/// different callbacks, which an RAII guard cannot express.
struct RawLock(AtomicBool);

impl RawLock {
    const fn new() -> Self {
        RawLock(AtomicBool::new(false))
    }

    fn lock(&self) {
        let mut spins = 0u32;
        while self
            .0
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    fn try_lock(&self) -> bool {
        self.0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn unlock(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Rank of per-instance locks registered through [`register_weak`]: taken after
/// every explicitly ranked global (the facade may lock its recorder, then the
/// recorder locks its own state) and before the leaf locks.
pub const RANK_INSTANCE: u16 = 1000;

/// Default rank of a [`ForkMutex`]: a leaf lock. It may be taken under a
/// lower-ranked lock, but no other registered lock is ever taken while it is
/// held, so it can go last.
pub const RANK_LEAF: u16 = 2000;

/// Something with locks that must be quiesced across a `fork()`.
pub trait ForkLockable: Send + Sync {
    /// Take every lock (called in the forking thread, before `fork`).
    fn fork_lock(&self);
    /// Release every lock taken by [`fork_lock`](Self::fork_lock) (called in the
    /// same thread, after `fork`, in both parent and child).
    fn fork_unlock(&self);
}

/// A mutual-exclusion lock that takes part in the fork protocol. Use it
/// for any state a forked child will touch. Registration is the owner's job
/// (see [`ForkMutex`] for statics, [`register_weak`] for per-instance locks).
pub struct ForkLock<T> {
    raw: RawLock,
    data: UnsafeCell<T>,
}

// SAFETY: access to `data` is serialised by `raw`.
unsafe impl<T: Send> Send for ForkLock<T> {}
unsafe impl<T: Send> Sync for ForkLock<T> {}

impl<T> ForkLock<T> {
    /// A lock around `value`.
    pub const fn new(value: T) -> Self {
        ForkLock {
            raw: RawLock::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Lock, blocking (spin, then yield) until available.
    pub fn lock(&self) -> ForkGuard<'_, T> {
        self.raw.lock();
        ForkGuard { lock: self }
    }

    /// Lock without blocking; `None` if it is held.
    pub fn try_lock(&self) -> Option<ForkGuard<'_, T>> {
        self.raw.try_lock().then_some(ForkGuard { lock: self })
    }

    /// Take the lock for the fork protocol (no guard; paired with
    /// [`ForkLock::fork_unlock`]).
    pub fn fork_lock(&self) {
        self.raw.lock();
    }

    /// Release a lock taken by [`ForkLock::fork_lock`].
    pub fn fork_unlock(&self) {
        self.raw.unlock();
    }
}

/// RAII guard for a [`ForkLock`].
pub struct ForkGuard<'a, T> {
    lock: &'a ForkLock<T>,
}

impl<T> Deref for ForkGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the lock is held for the guard's lifetime.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for ForkGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the lock is held for the guard's lifetime, exclusively.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for ForkGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.raw.unlock();
    }
}

impl<T: Send> ForkLockable for ForkLock<T> {
    fn fork_lock(&self) {
        self.raw.lock();
    }
    fn fork_unlock(&self) {
        self.raw.unlock();
    }
}

/// A `static`-friendly [`ForkLock`] that registers itself for the fork protocol
/// on first use. `lock` takes `&'static self`, which a plain `static` provides.
pub struct ForkMutex<T: 'static> {
    inner: ForkLock<Option<T>>,
    init: fn() -> T,
    rank: u16,
    /// Set under the registry lock together with the registration itself, so a
    /// fork can never see a half-done registration (a `Once` mid-flight would
    /// leave the child waiting on a thread that no longer exists).
    registered: AtomicBool,
}

impl<T: Send + 'static> ForkMutex<T> {
    /// A mutex whose initial value is `init()`, computed on first lock.
    pub const fn new(init: fn() -> T) -> Self {
        Self::ranked(init, RANK_LEAF)
    }

    /// Like [`new`](Self::new), with an explicit position in the fork lock
    /// order: lower ranks are taken first. Give one to any lock that is held
    /// while another registered lock is taken (the outer lock gets the lower
    /// rank); use a rank below [`RANK_INSTANCE`] for a lock held while a
    /// recorder's own locks are taken.
    pub const fn ranked(init: fn() -> T, rank: u16) -> Self {
        ForkMutex {
            inner: ForkLock::new(None),
            init,
            rank,
            registered: AtomicBool::new(false),
        }
    }

    fn register_once(&'static self) {
        if !self.registered.load(Ordering::Acquire) {
            register(
                Entry::Static(&self.inner),
                self.rank,
                Some(&self.registered),
            );
        }
    }

    /// Register with the fork protocol without locking.
    pub fn ensure_registered(&'static self) {
        self.register_once();
    }

    /// Lock, registering with the fork protocol and initialising on first use.
    pub fn lock(&'static self) -> ForkMutexGuard<T> {
        self.register_once();
        let mut guard = self.inner.lock();
        if guard.is_none() {
            *guard = Some((self.init)());
        }
        ForkMutexGuard { guard }
    }

    /// Lock without blocking; `None` if it is held.
    pub fn try_lock(&'static self) -> Option<ForkMutexGuard<T>> {
        self.register_once();
        let mut guard = self.inner.try_lock()?;
        if guard.is_none() {
            *guard = Some((self.init)());
        }
        Some(ForkMutexGuard { guard })
    }
}

/// Guard for a [`ForkMutex`]; derefs to the protected value.
pub struct ForkMutexGuard<T: 'static> {
    guard: ForkGuard<'static, Option<T>>,
}

impl<T> Deref for ForkMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().expect("initialised on lock")
    }
}

impl<T> DerefMut for ForkMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("initialised on lock")
    }
}

// ---------------------------------------------------------------------------
// The registry and the atfork callbacks
// ---------------------------------------------------------------------------

// On Windows there is no `fork`, so nothing reads the registry; it is kept so the
// lock types behave identically on every platform.
#[cfg_attr(not(unix), allow(dead_code))]
enum Entry {
    Static(&'static dyn ForkLockable),
    Weak(Weak<dyn ForkLockable>),
}

#[cfg_attr(not(unix), allow(dead_code))]
enum Held {
    Static(&'static dyn ForkLockable),
    Strong(Arc<dyn ForkLockable>),
}

#[cfg_attr(not(unix), allow(dead_code))]
struct Registry {
    lock: RawLock,
    entries: UnsafeCell<Vec<(u16, Entry)>>,
    /// What `prepare` locked, so `parent`/`child` unlock exactly that set.
    held: UnsafeCell<Vec<Held>>,
}

// SAFETY: `entries` and `held` are only touched with `lock` held. `prepare`
// takes it and `parent`/`child` release it, on the same (forking) thread.
unsafe impl Sync for Registry {}

static REGISTRY: Registry = Registry {
    lock: RawLock::new(),
    entries: UnsafeCell::new(Vec::new()),
    held: UnsafeCell::new(Vec::new()),
};

/// Add `entry`; with `flag`, only if it is not yet set, setting it before the
/// registry lock is released.
fn register(entry: Entry, rank: u16, flag: Option<&AtomicBool>) {
    REGISTRY.lock.lock();
    if let Some(flag) = flag {
        if flag.load(Ordering::Relaxed) {
            REGISTRY.lock.unlock();
            return;
        }
        flag.store(true, Ordering::Release);
    }
    // SAFETY: registry lock held.
    let entries = unsafe { &mut *REGISTRY.entries.get() };
    // Drop registrations whose owner is gone, so a long-lived process that
    // relaunches repeatedly does not grow the list without bound.
    entries.retain(|(_, e)| match e {
        Entry::Static(_) => true,
        Entry::Weak(w) => w.strong_count() > 0,
    });
    // Keep the list sorted by rank (stable among equals), so `prepare` can just
    // walk it: this is where the fixed lock order is enforced.
    let at = entries.partition_point(|(r, _)| *r <= rank);
    entries.insert(at, (rank, entry));
    REGISTRY.lock.unlock();
}

/// Register a per-instance lock holder (held weakly: when it is dropped the
/// registration lapses on its own). Call once per instance.
pub fn register_weak(target: &Arc<impl ForkLockable + 'static>) {
    let arc: Arc<dyn ForkLockable> = target.clone();
    register(Entry::Weak(Arc::downgrade(&arc)), RANK_INSTANCE, None);
}

/// Serialises the SDK's own stack walks. The `backtrace` crate guards symbol
/// resolution with a process-wide `std::sync::Mutex`, which does not survive
/// `fork()`: if a fork lands while another thread is inside it, the child
/// inherits it locked with no owner and its next stack walk hangs. Holding this
/// registered lock across every walk makes the `prepare` step wait the walk out,
/// so the child inherits the backtrace mutex free. It is deliberately separate
/// from the recorder lock, so a multi-millisecond symbolisation never stalls
/// capture. Take it only around a walk, never while holding another SDK lock.
static BACKTRACE: ForkMutex<()> = ForkMutex::new(|| ());

/// Guard for [`lock_backtrace`].
pub type BacktraceGuard = ForkMutexGuard<()>;

/// Hold this across any use of the `backtrace` crate (see [`BACKTRACE`]).
pub fn lock_backtrace() -> BacktraceGuard {
    install_fork_hook();
    BACKTRACE.lock()
}

/// Install the `pthread_atfork` hooks. Idempotent; a no-op off unix.
pub fn install_fork_hook() {
    #[cfg(unix)]
    {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // Register the backtrace lock now, not lazily on its first use: a
            // fork racing that first use would otherwise run `prepare` before the
            // lock is registered while its holder is mid-walk.
            BACKTRACE.ensure_registered();
            extern "C" fn prepare() {
                REGISTRY.lock.lock();
                // SAFETY: registry lock held until `parent`/`child`.
                let (entries, held) =
                    unsafe { (&mut *REGISTRY.entries.get(), &mut *REGISTRY.held.get()) };
                held.clear();
                for (_, entry) in entries.iter() {
                    match entry {
                        Entry::Static(s) => {
                            s.fork_lock();
                            held.push(Held::Static(*s));
                        }
                        Entry::Weak(w) => {
                            if let Some(strong) = w.upgrade() {
                                strong.fork_lock();
                                held.push(Held::Strong(strong));
                            }
                        }
                    }
                }
            }
            fn release() {
                // SAFETY: registry lock still held from `prepare`.
                let held = unsafe { &mut *REGISTRY.held.get() };
                // Reverse order of acquisition.
                while let Some(h) = held.pop() {
                    match h {
                        Held::Static(s) => s.fork_unlock(),
                        Held::Strong(a) => a.fork_unlock(),
                    }
                }
                REGISTRY.lock.unlock();
            }
            extern "C" fn parent() {
                release();
            }
            extern "C" fn child() {
                // Only atomics and lock releases — all that a multithreaded
                // process's fork child may safely do.
                EPOCH.fetch_add(1, Ordering::SeqCst);
                release();
            }
            // SAFETY: registers plain `extern "C"` functions. A failure (ENOMEM)
            // just leaves fork handling off.
            unsafe {
                libc::pthread_atfork(Some(prepare), Some(parent), Some(child));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_and_releases() {
        static L: ForkLock<u32> = ForkLock::new(0);
        {
            let mut g = L.lock();
            *g += 1;
            assert!(L.try_lock().is_none(), "held");
        }
        assert_eq!(*L.lock(), 1);
    }

    #[test]
    fn mutex_initialises_lazily_and_registers_once() {
        static M: ForkMutex<Vec<u8>> = ForkMutex::new(|| vec![1, 2]);
        assert_eq!(*M.lock(), vec![1, 2]);
        M.lock().push(3);
        assert_eq!(*M.lock(), vec![1, 2, 3]);
        assert!(M.try_lock().is_some());
    }

    #[test]
    fn contended_increments_are_not_lost() {
        static L: ForkLock<u64> = ForkLock::new(0);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..2000 {
                        *L.lock() += 1;
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(*L.lock(), 8000);
    }

    #[test]
    fn the_epoch_only_moves_when_forked() {
        install_fork_hook();
        install_fork_hook(); // idempotent
        let before = fork_epoch();
        assert_eq!(fork_epoch(), before);
    }

    /// Regression test for a lock-order inversion: `prepare` used to take locks in
    /// registration order, so an inner lock that registered before its outer lock
    /// deadlocked a fork against a thread nesting them outer -> inner.
    #[cfg(unix)]
    #[test]
    fn prepare_takes_nested_locks_in_rank_order_not_registration_order() {
        static OUTER: ForkMutex<u64> = ForkMutex::ranked(|| 0, 10);
        static INNER: ForkMutex<u64> = ForkMutex::ranked(|| 0, 20);
        install_fork_hook();
        // Register the inner lock FIRST, the outer one second.
        drop(INNER.lock());
        drop(OUTER.lock());

        // A hang here would hold the whole test run, so bound it: the parent is
        // the thread stuck in `fork`, which no per-child deadline can catch.
        let done = Arc::new(AtomicBool::new(false));
        // Disarms the watchdog on every exit path, a failed assertion included.
        struct Disarm(Arc<AtomicBool>);
        impl Drop for Disarm {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let _disarm = Disarm(Arc::clone(&done));
        let watchdog = {
            let done = Arc::clone(&done);
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                while !done.load(Ordering::Relaxed) {
                    if start.elapsed() > std::time::Duration::from_secs(60) {
                        eprintln!("fork() deadlocked against nested locks");
                        std::process::abort();
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
        };
        let stop = Arc::new(AtomicBool::new(false));
        let hammer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let mut outer = OUTER.lock();
                    let mut inner = INNER.lock();
                    *outer += 1;
                    *inner += 1;
                }
            })
        };
        for _ in 0..100 {
            // SAFETY: the child only locks, then `_exit`s.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let ok = OUTER.try_lock().is_some() && INNER.try_lock().is_some();
                unsafe { libc::_exit(if ok { 0 } else { 1 }) };
            }
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
        }
        stop.store(true, Ordering::Relaxed);
        hammer.join().unwrap();
        drop(_disarm);
        watchdog.join().unwrap();
    }

    /// The point of the protocol: fork while another thread holds the lock and
    /// the child must still be able to take it.
    #[cfg(unix)]
    #[test]
    fn a_child_forked_while_another_thread_holds_the_lock_can_still_take_it() {
        static M: ForkMutex<u64> = ForkMutex::new(|| 0);
        install_fork_hook();
        // Prime registration, then hammer the lock from another thread so some
        // forks land while it is held.
        drop(M.lock());
        let stop = Arc::new(AtomicBool::new(false));
        let hammer = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    *M.lock() += 1;
                }
            })
        };
        for _ in 0..40 {
            // SAFETY: the child only locks, then `_exit`s.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let ok = M.try_lock().is_some();
                unsafe { libc::_exit(if ok { 0 } else { 1 }) };
            }
            let start = std::time::Instant::now();
            let mut status = 0;
            loop {
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if r == pid {
                    break;
                }
                assert!(
                    start.elapsed() < std::time::Duration::from_secs(20),
                    "child hung"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(
                libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                "the child inherited the lock held by a thread that did not survive"
            );
        }
        stop.store(true, Ordering::Relaxed);
        hammer.join().unwrap();
    }
}

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
//! any lock another thread held at that instant stays locked forever. Two
//! primitives make that survivable:
//!
//! * [`fork_epoch`] — a counter bumped in the child (by a `pthread_atfork` hook),
//!   so any entry point can cheaply notice "I am a fork child" and rebuild.
//! * [`ForkMutex`] — a mutex whose child-side reset abandons the old lock instead
//!   of waiting on a thread that no longer exists.
//!
//! The hook itself does nothing but an atomic increment — the only thing that is
//! safe to do unconditionally between `fork` and `exec` in a multithreaded
//! process. All real work happens lazily, on the child's next SDK call.
//!
//! Windows has no `fork`, so there everything here is inert.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Number of times this process has been the child of a `fork()` since the hook
/// was installed. Compare against a remembered value to detect "forked since I
/// last looked".
pub fn fork_epoch() -> u64 {
    EPOCH.load(Ordering::SeqCst)
}

/// Install the child-side `pthread_atfork` hook. Idempotent; a no-op off unix.
pub fn install_fork_hook() {
    #[cfg(unix)]
    {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            extern "C" fn child() {
                // An atomic add is async-signal-safe, which is all a fork child of
                // a multithreaded process may rely on.
                EPOCH.fetch_add(1, Ordering::SeqCst);
            }
            // SAFETY: registering a plain `extern "C"` function that only touches
            // an atomic. A failure (ENOMEM) just leaves fork detection off.
            unsafe {
                libc::pthread_atfork(None, None, Some(child));
            }
        });
    }
}

/// A `Mutex` for process-global state that can be reset in a fork child.
///
/// A plain `static Mutex` is a trap across `fork`: if some other thread held it
/// at the instant of the fork, the child inherits it locked with no thread left
/// to release it, and the child's first SDK call deadlocks. [`reset_after_fork`]
/// side-steps that by moving to a fresh mutex and *leaking* the old one rather
/// than ever blocking on it.
///
/// [`reset_after_fork`]: ForkMutex::reset_after_fork
pub struct ForkMutex<T: 'static> {
    slot: AtomicPtr<Mutex<T>>,
    init: fn() -> T,
}

impl<T: Send + 'static> ForkMutex<T> {
    /// A mutex whose initial (and post-reset) value is `init()`.
    pub const fn new(init: fn() -> T) -> Self {
        ForkMutex {
            slot: AtomicPtr::new(std::ptr::null_mut()),
            init,
        }
    }

    fn mutex(&self) -> &'static Mutex<T> {
        let current = self.slot.load(Ordering::Acquire);
        if !current.is_null() {
            // SAFETY: only ever set to a leaked `Box<Mutex<T>>`, never freed.
            return unsafe { &*current };
        }
        let fresh = Box::into_raw(Box::new(Mutex::new((self.init)())));
        match self.slot.compare_exchange(
            std::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: as above.
            Ok(_) => unsafe { &*fresh },
            Err(existing) => {
                // Lost the race: free ours (never shared) and use the winner's.
                // SAFETY: `fresh` came from `Box::into_raw` just above, unshared.
                drop(unsafe { Box::from_raw(fresh) });
                // SAFETY: `existing` is a leaked box, never freed.
                unsafe { &*existing }
            }
        }
    }

    /// Lock, recovering the guard if a panic poisoned it.
    pub fn lock(&self) -> MutexGuard<'static, T> {
        self.mutex().lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lock without blocking; `None` if it is held.
    pub fn try_lock(&self) -> Option<MutexGuard<'static, T>> {
        match self.mutex().try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Call in a fork **child** (single-threaded at that instant). Replaces the
    /// lock with a fresh one and returns the value the old one held — or `None`
    /// if it was locked by a thread that no longer exists, in which case that
    /// value is unrecoverable and the old mutex is simply abandoned.
    pub fn reset_after_fork(&self) -> Option<T> {
        let old = self.mutex();
        let value = match old.try_lock() {
            Ok(mut g) => Some(std::mem::replace(&mut *g, (self.init)())),
            Err(TryLockError::Poisoned(p)) => {
                Some(std::mem::replace(&mut *p.into_inner(), (self.init)()))
            }
            Err(TryLockError::WouldBlock) => None,
        };
        let fresh = Box::into_raw(Box::new(Mutex::new((self.init)())));
        // The old mutex is leaked on purpose: a dead thread may still "hold" it.
        self.slot.store(fresh, Ordering::Release);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_round_trips_and_starts_from_init() {
        static M: ForkMutex<Vec<u8>> = ForkMutex::new(|| vec![1, 2]);
        assert_eq!(*M.lock(), vec![1, 2]);
        M.lock().push(3);
        assert_eq!(*M.lock(), vec![1, 2, 3]);
    }

    #[test]
    fn reset_returns_the_old_value_and_installs_a_fresh_one() {
        static M: ForkMutex<Option<u32>> = ForkMutex::new(|| None);
        *M.lock() = Some(7);
        assert_eq!(M.reset_after_fork(), Some(Some(7)));
        assert_eq!(*M.lock(), None, "the new mutex starts from init");
    }

    #[test]
    fn reset_abandons_a_lock_held_by_a_thread_that_will_never_release_it() {
        static M: ForkMutex<u32> = ForkMutex::new(|| 0);
        // Hold the lock on another thread that never lets go — the situation a
        // fork child is in with respect to a thread that did not survive.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (_hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _g = M.lock();
            locked_tx.send(()).unwrap();
            let _ = hold_rx.recv(); // parks until process exit
        });
        locked_rx.recv().unwrap();
        assert!(M.try_lock().is_none(), "held elsewhere");

        assert_eq!(
            M.reset_after_fork(),
            None,
            "the held value is unrecoverable"
        );
        // The point: the child can lock again instead of deadlocking.
        *M.lock() = 5;
        assert_eq!(*M.lock(), 5);
    }

    #[test]
    fn the_epoch_only_moves_when_forked() {
        install_fork_hook();
        install_fork_hook(); // idempotent
        let before = fork_epoch();
        assert_eq!(fork_epoch(), before);
    }
}

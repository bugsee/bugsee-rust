//
//  claim.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Cross-process exclusive claims on on-disk work items, and session liveness.
//!
//! Several processes can share one data directory — a prefork server's master
//! and its workers, containers sharing a volume — and the work items in it (a
//! crashed session to recover, a queued report to upload) must each be handled
//! by exactly one of them, or the backend receives duplicates.
//!
//! A claim is an **OS byte-range lock** on a small `*.lock` file:
//!
//! * unix: `fcntl(F_SETLK)`, a *process-scoped* lock. The kernel releases it the
//!   instant the process dies, however it dies, and a `fork` child does not
//!   inherit it.
//! * Windows: `LockFileEx` on the handle, released when the handle closes or the
//!   process exits.
//!
//! Because the kernel is the arbiter there is nothing to go stale: no owner pid
//! to probe, no start time, no lease to expire, no takeover protocol. That is
//! also why it is correct across **pid namespaces** (two containers sharing a
//! volume, both with a pid 1) and across pid reuse, where recording a pid in a
//! file is not. The lock file itself may be left behind by a dead holder; it is
//! just an unlocked file, and anyone may lock it.
//!
//! Two costs of process-scoped locks, both handled here:
//!
//! * They do not exclude *threads* of one process, so an in-process registry of
//!   held paths does that.
//! * POSIX drops a process's locks on a file when it closes **any** descriptor
//!   for that file. So a lock file is never opened twice by one process: the
//!   registry is consulted before every open, and a claim closes its descriptor
//!   *before* leaving the registry.
//!
//! Advisory locks on network filesystems are best effort (NFS needs lockd). The
//! SDK relies on no file timestamps for claims; the only ones it reads are the
//! grace windows for half-written files in `recovery` and `queue`, which assume
//! clocks that agree to within seconds.
//!
//! None of this runs on the crash path; it allocates freely.

use std::cell::UnsafeCell;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Holds a claim; dropping it releases the claim.
#[derive(Debug)]
pub struct Claim {
    path: PathBuf,
    file: Option<File>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Close (releasing the OS lock) strictly before leaving the registry, so
        // a thread that acquires next never has its lock dropped by our close.
        drop(self.file.take());
        registry().remove(&self.path);
    }
}

/// Try to claim `path` (created if absent). `None` if another process, or another
/// thread of this one, holds it, or if it cannot be claimed.
pub fn try_claim(path: &Path) -> Option<Claim> {
    match acquire(path, true) {
        Acquire::Got(claim) => Some(claim),
        Acquire::Held | Acquire::Unavailable => None,
    }
}

/// Whether the lock file at `path` is held by a live process.
pub(crate) enum Probe {
    Held,
    Free,
}

/// Is `path` currently claimed? A missing file is free. A file that cannot be
/// inspected is reported held: the conservative answer never disturbs a peer.
pub(crate) fn probe(path: &Path) -> Probe {
    if !path.exists() {
        return Probe::Free;
    }
    match acquire(path, false) {
        Acquire::Got(claim) => {
            drop(claim);
            Probe::Free
        }
        // Vanished since the existence check: nothing holds it.
        Acquire::Unavailable if !path.exists() => Probe::Free,
        Acquire::Held | Acquire::Unavailable => Probe::Held,
    }
}

/// Remove the lock file at `path` if nobody holds it (a dead holder's leftover).
pub(crate) fn remove_if_free(path: &Path) {
    if let Some(claim) = try_claim(path) {
        let _ = std::fs::remove_file(path);
        drop(claim);
    }
}

enum Acquire {
    Got(Claim),
    Held,
    Unavailable,
}

fn acquire(path: &Path, create: bool) -> Acquire {
    if !registry().insert(path) {
        return Acquire::Held; // another thread of this process
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => {
            registry().remove(path);
            return Acquire::Unavailable;
        }
    };
    match os::try_lock(&file) {
        Ok(true) => Acquire::Got(Claim {
            path: path.to_path_buf(),
            file: Some(file),
        }),
        Ok(false) => {
            drop(file);
            registry().remove(path);
            Acquire::Held
        }
        Err(_) => {
            drop(file);
            registry().remove(path);
            Acquire::Unavailable
        }
    }
}

/// The paths this process currently holds, keyed by pid so that a `fork` child,
/// which inherits a copy of this memory, ignores the parent's entries.
///
/// A hand-rolled spin lock rather than a `Mutex`: if another thread held a
/// `Mutex` at the instant of `fork`, the child's copy would stay locked forever
/// and its first claim would deadlock. Here the lock word records the holder's
/// pid, so a child that finds it held by a different pid knows the holder does
/// not exist in this process and breaks it.
struct Registry {
    owner: AtomicU32,
    held: UnsafeCell<Vec<(u32, PathBuf)>>,
}

// SAFETY: `held` is only touched while `owner` is held (see `with`).
unsafe impl Sync for Registry {}

static REGISTRY: Registry = Registry {
    owner: AtomicU32::new(0),
    held: UnsafeCell::new(Vec::new()),
};

fn registry() -> &'static Registry {
    &REGISTRY
}

impl Registry {
    fn with<R>(&self, f: impl FnOnce(&mut Vec<(u32, PathBuf)>) -> R) -> R {
        let pid = std::process::id();
        loop {
            match self
                .owner
                .compare_exchange(0, pid, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(holder) if holder != pid => {
                    // Held by a pid that is not us: a parent's thread at the time
                    // of `fork`. It does not exist here. Take the lock, and
                    // discard (leak, never drop) the vector it may have left
                    // half-modified.
                    if self
                        .owner
                        .compare_exchange(holder, pid, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        // SAFETY: we now hold the lock; the old value is leaked
                        // because its allocator state may be mid-update.
                        unsafe { std::ptr::write(self.held.get(), Vec::new()) };
                        break;
                    }
                }
                Err(_) => std::thread::yield_now(),
            }
        }
        // SAFETY: exclusive while `owner == pid` (held above).
        let out = f(unsafe { &mut *self.held.get() });
        self.owner.store(0, Ordering::Release);
        out
    }

    /// Record `path` as held by this process; `false` if it already is.
    fn insert(&self, path: &Path) -> bool {
        let pid = std::process::id();
        self.with(|held| {
            if held.iter().any(|(p, q)| *p == pid && q == path) {
                return false;
            }
            // Entries of a different pid are a forked-from parent's; drop them.
            held.retain(|(p, _)| *p == pid);
            held.push((pid, path.to_path_buf()));
            true
        })
    }

    fn remove(&self, path: &Path) {
        let pid = std::process::id();
        self.with(|held| held.retain(|(p, q)| !(*p == pid && q == path)));
    }
}

#[cfg(unix)]
mod os {
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;

    /// Take a non-blocking exclusive lock on byte 0. `Ok(false)` if a peer holds it.
    pub fn try_lock(file: &File) -> io::Result<bool> {
        // SAFETY: an all-zero `flock` is a valid plain-data value.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = 0;
        fl.l_len = 1;
        loop {
            // SAFETY: `fd` is a valid open descriptor and `fl` a valid `flock`.
            let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) };
            if ret == 0 {
                return Ok(true);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EACCES) => return Ok(false),
                Some(libc::EINTR) => continue,
                _ => return Err(err),
            }
        }
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 1;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 2;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    extern "system" {
        fn LockFileEx(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            len_low: u32,
            len_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    /// Take a non-blocking exclusive lock on byte 0. `Ok(false)` if a peer holds it.
    pub fn try_lock(file: &File) -> io::Result<bool> {
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        };
        // SAFETY: the handle is valid for the call and `ov` outlives it.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut ov,
            )
        };
        if ok != 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(ERROR_LOCK_VIOLATION) => Ok(false),
            _ => Err(err),
        }
    }
}

/// No file locks: claims exclude threads of this process only.
#[cfg(not(any(unix, windows)))]
mod os {
    pub fn try_lock(_file: &std::fs::File) -> std::io::Result<bool> {
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bugsee-claim-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_claim_is_exclusive_and_released_on_drop() {
        let dir = tmp();
        let path = dir.join("x.lock");
        let first = try_claim(&path).expect("first claim");
        assert!(try_claim(&path).is_none(), "already held");
        assert!(matches!(probe(&path), Probe::Held));
        drop(first);
        assert!(matches!(probe(&path), Probe::Free));
        assert!(try_claim(&path).is_some(), "free again after release");
    }

    #[test]
    fn a_leftover_lock_file_from_a_dead_holder_is_just_free() {
        let dir = tmp();
        let path = dir.join("x.lock");
        // Contents are irrelevant: a dead holder's pid, even one that collides
        // with a live process (here, our own), must not block anything.
        std::fs::write(&path, std::process::id().to_string()).unwrap();
        assert!(matches!(probe(&path), Probe::Free));
        assert!(try_claim(&path).is_some());
    }

    #[test]
    fn a_missing_lock_file_probes_free_and_is_not_created() {
        let dir = tmp();
        let path = dir.join("nope.lock");
        assert!(matches!(probe(&path), Probe::Free));
        assert!(!path.exists());
    }

    #[test]
    fn dropping_one_claim_never_releases_another() {
        // POSIX drops a process's locks on a file when ANY descriptor for it
        // closes; the registry must keep a second open from ever happening.
        let dir = tmp();
        let path = dir.join("x.lock");
        let held = try_claim(&path).unwrap();
        assert!(try_claim(&path).is_none());
        assert!(matches!(probe(&path), Probe::Held));
        assert!(
            try_claim(&path).is_none(),
            "probing must not have closed our lock"
        );
        drop(held);
    }

    #[test]
    fn racing_claimers_of_one_path_yield_exactly_one_holder() {
        for _ in 0..100 {
            let dir = tmp();
            let path = dir.join("x.lock");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        let claim = try_claim(&path);
                        // Hold until every thread has tried.
                        barrier.wait();
                        claim.is_some()
                    })
                })
                .collect();
            let holders = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|held| *held)
                .count();
            assert_eq!(holders, 1, "exactly one claimer may win");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

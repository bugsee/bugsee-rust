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
//!   for that file. So a lock file is never opened twice by one process: every
//!   open (claim or probe) registers the path first, and a claim closes its
//!   descriptor *before* leaving the registry. Registry keys are canonicalised
//!   (`/tmp` and `/private/tmp`, relative and absolute, are one file).
//!
//! **Unsupported locks.** Some filesystems reject byte-range locks outright (NFS
//! without lockd: `ENOLCK`; FUSE/9p: `EOPNOTSUPP`/`ENOSYS`/`EINVAL`). The base
//! scheme (`O_EXCL` files) worked there, so refusing to claim would be a
//! regression: a queue that never drains. Instead such a claim degrades to
//! thread-only exclusion (announced once on stderr), and a probe reports
//! [`Probe::Unknown`] so the caller falls back to its pid check.
//!
//! **Locks that succeed but do not exclude** (Docker Desktop bind mounts, WSL2
//! drvfs/9p, NFS mounted `nolock`) cannot be detected. Processes sharing a
//! directory over such a mount can deliver a report twice; run one SDK host per
//! directory there.
//!
//! The SDK relies on no file timestamps for claims; the only ones it reads are
//! the grace windows for half-written files in `recovery` and `queue`, which
//! assume clocks that agree to within seconds.
//!
//! None of this runs on the crash path; it allocates freely.

use std::cell::UnsafeCell;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Holds a claim; dropping it releases the claim.
#[derive(Debug)]
pub struct Claim {
    key: PathBuf,
    file: Option<File>,
    os_locked: bool,
}

impl Claim {
    /// Whether the kernel enforces this claim against other processes. `false`
    /// means the filesystem rejected locks and only threads are excluded.
    pub(crate) fn is_os_locked(&self) -> bool {
        self.os_locked
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        // Close (releasing the OS lock) strictly before leaving the registry, so
        // a thread that acquires next never has its lock dropped by our close.
        drop(self.file.take());
        registry().remove(&self.key);
    }
}

/// Try to claim `path` (created if absent). `None` if another process, or another
/// thread of this one, holds it, or if it cannot be claimed.
pub fn try_claim(path: &Path) -> Option<Claim> {
    // A dying process must not start work a kill would leave half-claimed.
    if crate::crash_flush::is_dying() {
        return None;
    }
    match acquire(path) {
        Acquire::Got(claim) => Some(claim),
        Acquire::Held | Acquire::Unavailable => None,
    }
}

/// Like [`try_claim`] for a directory that may be full or read-only, where the
/// lock file cannot be created but the work (recover a crash, deliver a queued
/// report) is still worth doing: `Some(None)` means "no claim could be made,
/// proceed unclaimed". The cost is that two processes sharing such a directory
/// may both do the work, and the backend deduplicates. `None` means another
/// process (or thread) holds the claim.
pub fn try_claim_lenient(path: &Path) -> Option<Option<Claim>> {
    if crate::crash_flush::is_dying() {
        return None;
    }
    match acquire(path) {
        Acquire::Got(claim) => Some(Some(claim)),
        Acquire::Unavailable => Some(None),
        Acquire::Held => None,
    }
}

/// Whether the lock file at `path` is held by a live process.
pub(crate) enum Probe {
    Held,
    Free,
    /// The filesystem cannot say (it rejects locks): use another liveness signal.
    Unknown,
}

/// Is `path` currently claimed? A missing file is free. A file that cannot be
/// inspected is reported held: the conservative answer never disturbs a peer.
///
/// On unix this asks (`F_GETLK`) without taking the lock, so a probe can never
/// steal the lock from a creator that has made the file but not yet locked it.
pub(crate) fn probe(path: &Path) -> Probe {
    if !path.exists() {
        return Probe::Free;
    }
    let key = key_of(path);
    if !registry().insert(&key) {
        return Probe::Held; // held by a thread of this process, or being probed
    }
    let result = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => {
            let r = match test_is_locked(&file) {
                Ok(Peek::Free) => Probe::Free,
                Ok(Peek::Held) => Probe::Held,
                Ok(Peek::Unsupported) => Probe::Unknown,
                Err(_) => Probe::Held,
            };
            drop(file); // close before leaving the registry
            r
        }
        // Vanished since the existence check: nothing holds it.
        Err(_) if !path.exists() => Probe::Free,
        Err(_) => Probe::Held,
    };
    registry().remove(&key);
    result
}

/// Remove the lock file at `path` if nobody holds it (a dead holder's leftover).
pub(crate) fn remove_if_free(path: &Path) {
    if let Some(claim) = try_claim(path) {
        let _ = std::fs::remove_file(path);
        drop(claim);
    }
}

/// Forget every registry entry. Called in a `fork` child before it claims
/// anything: the child inherits the parent's registry (entries it does not own,
/// possibly mid-update under the parent's spin lock) and none of its locks.
pub fn reset_after_fork() {
    registry().reset();
}

enum Acquire {
    Got(Claim),
    Held,
    Unavailable,
}

fn acquire(path: &Path) -> Acquire {
    let key = key_of(path);
    if !registry().insert(&key) {
        return Acquire::Held; // another thread of this process
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(_) => {
            registry().remove(&key);
            return Acquire::Unavailable;
        }
    };
    match test_try_lock(&file) {
        Ok(Lock::Got) => Acquire::Got(Claim {
            key,
            file: Some(file),
            os_locked: true,
        }),
        Ok(Lock::Contended) => {
            drop(file);
            registry().remove(&key);
            Acquire::Held
        }
        Ok(Lock::Unsupported) => {
            static ANNOUNCED: AtomicBool = AtomicBool::new(false);
            crate::util::warn_once(
                &ANNOUNCED,
                "this filesystem does not support file locks; processes sharing the data \
                 directory cannot be kept from delivering the same report twice",
            );
            Acquire::Got(Claim {
                key,
                file: Some(file),
                os_locked: false,
            })
        }
        Err(_) => {
            drop(file);
            registry().remove(&key);
            Acquire::Unavailable
        }
    }
}

/// The registry key for `path`: its directory canonicalised, so every spelling of
/// one file maps to one key. Falls back to the path as given.
fn key_of(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            std::fs::canonicalize(parent)
                .map(|p| p.join(name))
                .unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// Outcome of an attempt to take a lock.
enum Lock {
    Got,
    Contended,
    /// Only unix can tell "locks unsupported" apart from other errors.
    #[cfg_attr(not(unix), allow(dead_code))]
    Unsupported,
}

/// Outcome of asking whether a lock is held.
enum Peek {
    Free,
    Held,
    Unsupported,
}

#[cfg(test)]
thread_local! {
    /// Test hook: make this thread's lock calls behave as on a filesystem that
    /// rejects locks.
    pub(crate) static FORCE_UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn test_try_lock(file: &File) -> std::io::Result<Lock> {
    #[cfg(test)]
    if FORCE_UNSUPPORTED.with(|f| f.get()) {
        return Ok(Lock::Unsupported);
    }
    os::try_lock(file)
}

fn test_is_locked(file: &File) -> std::io::Result<Peek> {
    #[cfg(test)]
    if FORCE_UNSUPPORTED.with(|f| f.get()) {
        return Ok(Peek::Unsupported);
    }
    os::is_locked(file)
}

/// The paths this process currently holds, keyed by pid so that a `fork` child,
/// which inherits a copy of this memory, ignores the parent's entries.
///
/// A hand-rolled spin lock rather than a `Mutex`: if another thread held a
/// `Mutex` at the instant of `fork`, the child's copy would stay locked forever
/// and its first claim would deadlock. The fork hook calls [`reset_after_fork`]
/// to clear it; as a second line the lock word records the holder's pid, so a
/// child that finds it held by a different pid breaks it.
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

    /// Discard everything, including a lock word left held by a thread that does
    /// not exist in this process. Only for a freshly forked, single-threaded child.
    fn reset(&self) {
        // SAFETY: the caller is the only thread in the process, so nothing else
        // can be using the registry; the old vector is leaked, not dropped, as its
        // allocator state may be mid-update from the parent's other threads.
        unsafe { std::ptr::write(self.held.get(), Vec::new()) };
        self.owner.store(0, Ordering::Release);
    }
}

#[cfg(unix)]
mod os {
    use super::{Lock, Peek};
    use std::fs::File;
    use std::io;
    use std::os::unix::io::AsRawFd;

    fn whole() -> libc::flock {
        // SAFETY: an all-zero `flock` is a valid plain-data value.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = 0;
        fl.l_len = 1;
        fl
    }

    /// Errors meaning "this filesystem does not do byte-range locks".
    fn is_unsupported(errno: i32) -> bool {
        errno == libc::ENOLCK
            || errno == libc::ENOSYS
            || errno == libc::EINVAL
            || errno == libc::EOPNOTSUPP
            || errno == libc::ENOTSUP
    }

    /// Take a non-blocking exclusive lock on byte 0.
    pub fn try_lock(file: &File) -> io::Result<Lock> {
        let fl = whole();
        loop {
            // SAFETY: `fd` is a valid open descriptor and `fl` a valid `flock`.
            let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) };
            if ret == 0 {
                return Ok(Lock::Got);
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EACCES) => return Ok(Lock::Contended),
                Some(libc::EINTR) => continue,
                Some(e) if is_unsupported(e) => return Ok(Lock::Unsupported),
                _ => return Err(err),
            }
        }
    }

    /// Ask whether a peer holds the lock, without taking it.
    // `l_type` and `F_UNLCK` are different integer types per platform; widening
    // both to i32 is the portable spelling, a no-op cast on some of them.
    #[allow(clippy::unnecessary_cast)]
    pub fn is_locked(file: &File) -> io::Result<Peek> {
        let mut fl = whole();
        loop {
            // SAFETY: as above; `F_GETLK` only writes the conflicting lock into `fl`.
            let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut fl) };
            if ret == 0 {
                return Ok(if i32::from(fl.l_type) == libc::F_UNLCK as i32 {
                    Peek::Free
                } else {
                    Peek::Held
                });
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(e) if is_unsupported(e) => return Ok(Peek::Unsupported),
                _ => return Err(err),
            }
        }
    }
}

#[cfg(windows)]
mod os {
    use super::{Lock, Peek};
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

    /// Take a non-blocking exclusive lock on byte 0.
    pub fn try_lock(file: &File) -> io::Result<Lock> {
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
            return Ok(Lock::Got);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(ERROR_LOCK_VIOLATION) => Ok(Lock::Contended),
            _ => Err(err),
        }
    }

    /// Windows has no query-only call, so this takes the lock; the caller closes
    /// the handle straight after, which releases it.
    pub fn is_locked(file: &File) -> io::Result<Peek> {
        Ok(match try_lock(file)? {
            Lock::Got => Peek::Free,
            Lock::Contended => Peek::Held,
            Lock::Unsupported => Peek::Unsupported,
        })
    }
}

/// No file locks: claims exclude threads of this process only.
#[cfg(not(any(unix, windows)))]
mod os {
    use super::{Lock, Peek};
    pub fn try_lock(_file: &std::fs::File) -> std::io::Result<Lock> {
        Ok(Lock::Unsupported)
    }
    pub fn is_locked(_file: &std::fs::File) -> std::io::Result<Peek> {
        Ok(Peek::Unsupported)
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
        assert!(first.is_os_locked());
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
    fn probing_never_takes_or_drops_a_lock() {
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
        // And a probe of a free file leaves it free for the next claimer.
        assert!(matches!(probe(&path), Probe::Free));
        assert!(try_claim(&path).is_some());
    }

    #[test]
    fn one_file_under_two_spellings_is_one_claim() {
        let dir = tmp();
        let path = dir.join("x.lock");
        let held = try_claim(&path).unwrap();
        // The same file through a `..` detour.
        let detour = dir.join("sub").join("..").join("x.lock");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(try_claim(&detour).is_none());
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

    #[test]
    fn a_filesystem_that_rejects_locks_degrades_to_thread_only_claims() {
        let dir = tmp();
        let path = dir.join("x.lock");
        FORCE_UNSUPPORTED.with(|f| f.set(true));
        let first = try_claim(&path).expect("must still claim");
        assert!(!first.is_os_locked());
        assert!(try_claim(&path).is_none(), "threads are still excluded");
        drop(first);
        assert!(try_claim(&path).is_some(), "and it is released on drop");
        // A probe cannot say: the caller must use another signal.
        assert!(matches!(probe(&path), Probe::Unknown));
        FORCE_UNSUPPORTED.with(|f| f.set(false));
    }
}

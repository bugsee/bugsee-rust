//
//  claim.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Cross-process exclusive claims on on-disk work items.
//!
//! Several processes can share one data directory — a prefork server's master
//! and its workers above all — and the work items in it (a crashed session to
//! recover, a queued report to upload) must each be handled by exactly one of
//! them, or the backend receives duplicates. A claim is a small file created
//! with `O_EXCL` (`CREATE_NEW` on Windows) that records its owner as
//! `"<pid> <start-time> <nonce>"`.
//!
//! A claim must never block the work forever if its owner dies, so a stale claim
//! is taken over. It is stale when
//!
//! * its owner process is gone, or the pid now belongs to a *different* process
//!   (the recorded start time no longer matches: pid reuse, or a container's
//!   restarted app being pid 1 again),
//! * it has no readable owner and is older than [`UNOWNED_GRACE`] (its creator
//!   died between creating the file and writing it), or
//! * it is older than [`MAX_LEASE`], a backstop for a holder that is alive but
//!   wedged, or on a platform that cannot tell processes apart.
//!
//! Taking over is serialised by a short-lived guard file, so two processes that
//! both find the same claim stale cannot each remove the other's fresh claim and
//! both end up holding it. (An OS file lock would release on death automatically,
//! but it is per-open-file-description, not per-process, which makes it unreliable
//! across `fork`.)
//!
//! None of this runs on the crash path; it allocates freely.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::process;

/// A claim that has no readable owner is given this long before it is treated as
/// stale: its creator writes the owner a moment after creating the file.
const UNOWNED_GRACE: Duration = Duration::from_secs(5);

/// No claim is honoured for longer than this, whatever its owner's state. Work
/// items are short (one upload, one recovery), so a claim this old is wedged.
const MAX_LEASE: Duration = Duration::from_secs(30 * 60);

/// The takeover guard is held for a few syscalls; one older than this was left
/// by a process that died inside the critical section.
const GUARD_TTL: Duration = Duration::from_secs(10);

/// Holds a claim; dropping it releases the claim.
#[derive(Debug)]
pub struct Claim {
    path: PathBuf,
    /// Exactly what this claim wrote, so release never removes a successor's claim.
    content: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).is_ok_and(|c| c == self.content) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Try to claim `path`. `None` if another live process (or another thread of this
/// one) holds it, or if it cannot be claimed.
pub fn try_claim(path: &Path) -> Option<Claim> {
    let content = owner_record();
    match create(path, &content) {
        Create::Done => Some(Claim {
            path: path.to_path_buf(),
            content,
        }),
        Create::Exists => take_over(path, content),
        // Directory missing / unwritable: nothing can be claimed, and the
        // caller's own I/O will fail more usefully than we can here.
        Create::Failed => None,
    }
}

enum Create {
    Done,
    Exists,
    Failed,
}

/// Atomically create `path` holding `content` (one `write`, so the owner is
/// visible as soon as the file is).
fn create(path: &Path, content: &str) -> Create {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            let _ = file.write_all(content.as_bytes());
            Create::Done
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Create::Exists,
        Err(_) => Create::Failed,
    }
}

/// Replace a stale claim with ours. Only one process at a time may do this: the
/// guard file excludes a second taker between our staleness check and our create,
/// which is exactly the window in which it could otherwise remove the claim we
/// just made.
fn take_over(path: &Path, content: String) -> Option<Claim> {
    let observed = std::fs::read_to_string(path).ok()?;
    if !is_stale(path, &observed) {
        return None;
    }
    let guard_path = sibling(path, "takeover");
    match create(&guard_path, "") {
        Create::Done => {}
        Create::Exists => {
            // Held by a peer mid-takeover (it finishes in microseconds, and the
            // work is retried on the next drain), or left by one that died.
            if age(&guard_path).is_some_and(|a| a > GUARD_TTL) {
                let _ = std::fs::remove_file(&guard_path);
            }
            return None;
        }
        Create::Failed => return None,
    }
    let _guard = Guard(guard_path);
    // Re-check under the guard: a peer may have completed its own takeover since
    // we looked, and what is there now is a live claim, not the stale one.
    if std::fs::read_to_string(path).ok().as_deref() != Some(observed.as_str()) {
        return None;
    }
    let _ = std::fs::remove_file(path);
    match create(path, &content) {
        Create::Done => Some(Claim {
            path: path.to_path_buf(),
            content,
        }),
        // A fresh creator slipped in between our remove and create: it holds it.
        Create::Exists | Create::Failed => None,
    }
}

struct Guard(PathBuf);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn sibling(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// `"<pid> <start|-> <nonce>"` for this process; the nonce makes every claim's
/// content unique, so "is this still my claim" is a content comparison.
fn owner_record() -> String {
    let (pid, start) =
        process::parse_token(&process::self_token()).unwrap_or((std::process::id(), None));
    let start = start.map_or_else(|| "-".to_string(), |s| s.to_string());
    format!("{pid} {start} {}", crate::util::random_hex(8))
}

fn age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn is_stale(path: &Path, content: &str) -> bool {
    let age = age(path);
    if age.is_some_and(|a| a > MAX_LEASE) {
        return true;
    }
    match process::parse_token(content) {
        // A live owner with the recorded identity (including another thread of
        // this very process) holds it.
        Some((pid, start)) => !process::owner_is_alive(pid, start),
        None => age.is_some_and(|a| a > UNOWNED_GRACE),
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

    fn backdate(path: &Path, by: Duration) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - by).unwrap();
    }

    /// This process's own live owner record, as a peer would have written it.
    fn live_owner() -> String {
        format!("{} nonce", process::self_token())
    }

    #[test]
    fn a_claim_is_exclusive_and_released_on_drop() {
        let dir = tmp();
        let path = dir.join("x.claim");
        let first = try_claim(&path).expect("first claim");
        assert!(try_claim(&path).is_none(), "already held");
        drop(first);
        assert!(try_claim(&path).is_some(), "free again after release");
    }

    #[test]
    fn a_claim_whose_owner_is_dead_is_taken_over() {
        let dir = tmp();
        let path = dir.join("x.claim");
        // A pid that cannot be alive: far above any real pid range.
        std::fs::write(&path, "4294967290").unwrap();
        assert!(try_claim(&path).is_some(), "stale claim must be reclaimed");
        assert!(!sibling(&path, "takeover").exists(), "guard released");
    }

    #[test]
    fn a_claim_held_by_a_live_process_is_respected() {
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, live_owner()).unwrap();
        assert!(try_claim(&path).is_none());
        // Old-format owner (pid only), pid 1: alive on unix.
        #[cfg(unix)]
        {
            std::fs::write(&path, "1").unwrap();
            assert!(try_claim(&path).is_none());
        }
    }

    #[test]
    fn a_recycled_pid_does_not_keep_a_dead_owners_claim_alive() {
        let Some(start) = process::start_time(std::process::id()) else {
            return; // platform cannot tell processes apart
        };
        let dir = tmp();
        let path = dir.join("x.claim");
        // Our own (live) pid, but with a start time that is not ours: the claim
        // was written by an earlier process that had this pid.
        std::fs::write(
            &path,
            format!("{} {} stale", std::process::id(), start.wrapping_add(1)),
        )
        .unwrap();
        assert!(try_claim(&path).is_some(), "recycled pid must not block");
    }

    #[test]
    fn a_freshly_created_unowned_claim_is_not_stolen_but_an_old_one_is() {
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, "").unwrap(); // creator has not written its owner yet
        assert!(try_claim(&path).is_none());
        backdate(&path, UNOWNED_GRACE + Duration::from_secs(5));
        assert!(try_claim(&path).is_some(), "creator died before writing");
    }

    #[test]
    fn a_wedged_holder_cannot_hold_a_claim_past_the_lease() {
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, live_owner()).unwrap();
        assert!(try_claim(&path).is_none());
        backdate(&path, MAX_LEASE + Duration::from_secs(60));
        assert!(try_claim(&path).is_some());
    }

    #[test]
    fn releasing_a_claim_never_removes_its_successors() {
        let dir = tmp();
        let path = dir.join("x.claim");
        let first = try_claim(&path).unwrap();
        // First is judged wedged and replaced behind its back.
        std::fs::write(&path, live_owner()).unwrap();
        drop(first);
        assert!(path.exists(), "the successor's claim must survive");
    }

    #[test]
    fn an_abandoned_takeover_guard_does_not_block_forever() {
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, "4294967290").unwrap();
        let guard = sibling(&path, "takeover");
        std::fs::write(&guard, "").unwrap();
        // A taker is (apparently) mid-takeover: back off.
        assert!(try_claim(&path).is_none());
        // It died there long ago: clear the guard, then succeed.
        backdate(&guard, GUARD_TTL + Duration::from_secs(5));
        assert!(
            try_claim(&path).is_none(),
            "clearing the guard is its own step"
        );
        assert!(try_claim(&path).is_some());
    }

    #[test]
    fn racing_takers_of_one_stale_claim_yield_exactly_one_holder() {
        for _ in 0..200 {
            let dir = tmp();
            let path = dir.join("x.claim");
            std::fs::write(&path, "4294967290").unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        let claim = try_claim(&path);
                        // Keep the claim held until every thread has tried.
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
            assert_eq!(holders, 1, "exactly one taker may win");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

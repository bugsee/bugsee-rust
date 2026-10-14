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
//! with `O_EXCL` that records its owner's pid.
//!
//! A claim must never block the work forever if its owner dies, so a claim whose
//! owner is no longer alive is *stale* and is taken over. (An OS file lock would
//! be released on death automatically, but it is per-open-file-description, not
//! per-process, which makes it unreliable across `fork`.)

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// A claim that has no readable owner is given this long before it is treated as
/// stale: its creator writes the pid a moment after creating the file.
const UNOWNED_GRACE: Duration = Duration::from_secs(5);

/// Holds a claim; dropping it releases the claim.
#[derive(Debug)]
pub struct Claim {
    path: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Try to claim `path`. `None` if another live process (or another thread of this
/// one) holds it, or if the claim file cannot be created at all.
pub fn try_claim(path: &Path) -> Option<Claim> {
    // A dying process must not start work a kill would leave half-claimed.
    if crate::crash_flush::is_dying() {
        return None;
    }
    match claim_state(path) {
        ClaimState::Held(claim) => Some(claim),
        ClaimState::Taken | ClaimState::Unavailable => None,
    }
}

/// Like [`try_claim`] for a directory that may be full or read-only, where the
/// claim file cannot be created but the work (recover a crash, deliver a queued
/// report) is still worth doing: `Some(None)` means "no claim could be made,
/// proceed unclaimed". The cost is that two processes sharing such a directory
/// may both do the work, and the backend deduplicates. `None` means another
/// process holds the claim.
pub fn try_claim_lenient(path: &Path) -> Option<Option<Claim>> {
    if crate::crash_flush::is_dying() {
        return None;
    }
    match claim_state(path) {
        ClaimState::Held(claim) => Some(Some(claim)),
        ClaimState::Unavailable => Some(None),
        ClaimState::Taken => None,
    }
}

enum ClaimState {
    Held(Claim),
    Taken,
    Unavailable,
}

fn claim_state(path: &Path) -> ClaimState {
    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
                return ClaimState::Held(Claim {
                    path: path.to_path_buf(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !is_stale(path) {
                    return ClaimState::Taken;
                }
                // Take over a claim whose owner is gone, then retry the create.
                // Losing the race to another taker just means `create_new` fails
                // again and we report the claim as held.
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    // Someone else took it over first: retry the create.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    // A read-only or full volume will not let us clear it, so no
                    // claim can ever be made here: not "held by someone else".
                    Err(_) => return ClaimState::Unavailable,
                }
            }
            // Directory missing / unwritable: nothing can be claimed, and the
            // caller's own I/O will fail more usefully than we can here.
            Err(_) => return ClaimState::Unavailable,
        }
    }
    ClaimState::Taken
}

fn is_stale(path: &Path) -> bool {
    let owner = std::fs::read_to_string(path).unwrap_or_default();
    match owner.trim().parse::<u32>() {
        // Our own pid is a live owner: another thread of this process holds it.
        Ok(pid) => pid != std::process::id() && !crate::recovery::process_is_alive(pid),
        Err(_) => std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > UNOWNED_GRACE),
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
    }

    #[test]
    fn a_claim_held_by_a_live_process_is_respected() {
        let dir = tmp();
        let path = dir.join("x.claim");
        // pid 1 is always alive on unix; on other platforms liveness is "dead".
        std::fs::write(&path, "1").unwrap();
        #[cfg(unix)]
        assert!(try_claim(&path).is_none());
        #[cfg(not(unix))]
        assert!(try_claim(&path).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_claim_that_cannot_be_removed_is_unavailable_not_taken() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, "4294967290").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::remove_file(dir.join("nonexistent-probe")).is_ok()
            || std::fs::write(dir.join("probe"), b"x").is_ok()
        {
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
            return; // permission bits not enforced (root)
        }
        // Proceed unclaimed instead of skipping the work as "held elsewhere".
        assert!(matches!(try_claim_lenient(&path), Some(None)));
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755));
    }

    #[test]
    fn a_freshly_created_unowned_claim_is_not_stolen() {
        let dir = tmp();
        let path = dir.join("x.claim");
        std::fs::write(&path, "").unwrap(); // creator has not written its pid yet
        assert!(try_claim(&path).is_none());
    }
}

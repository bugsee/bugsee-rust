//
//  session.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session generations and liveness markers.
//!
//! Each launch claims a fresh generation by **atomically** creating its
//! `<gen>.alive` marker with `O_EXCL`, so two processes sharing a data dir never
//! collide on a generation (which would corrupt the append streams). The
//! generation floor is derived from the on-disk state (existing markers/parts +
//! the `gen` hint), so a torn/lost `gen` file after power loss cannot make a new
//! session reuse a lower number and hide prior crashes. A clean shutdown removes
//! the marker; a marker surviving into the next launch means an abnormal exit.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The name of the generation counter hint file under the data dir.
const GEN_FILE: &str = "gen";
/// Sub-directory holding per-generation liveness markers.
const SESSIONS_DIR: &str = "sessions";

/// A launched session: owns its generation and liveness marker.
pub struct Session {
    data_dir: PathBuf,
    generation: u64,
    /// The OS lock on `<gen>.lock`, held for the session's life. Peers sharing the
    /// data dir (including from another pid namespace) probe it to tell a live
    /// session from a crashed one; the kernel drops it when this process dies.
    /// `None` when the session has no lock file (the filesystem rejects locks, or
    /// the file could not be locked), in which case the pid in the marker is all
    /// a peer has and `<gen>.lock` is deliberately absent.
    _lock: Option<crate::claim::Claim>,
}

impl Session {
    /// Begin a session: atomically claim the next free generation and write its
    /// liveness marker.
    pub fn begin(data_dir: &Path) -> std::io::Result<Session> {
        std::fs::create_dir_all(data_dir)?;
        let markers = data_dir.join(SESSIONS_DIR);
        std::fs::create_dir_all(&markers)?;

        let mut candidate = floor_generation(data_dir) + 1;
        let (generation, lock) = loop {
            let marker = markers.join(format!("{candidate}.alive"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
            {
                Ok(mut f) => {
                    // Lock BEFORE writing the pid: the marker is empty (and fresh,
                    // so peers skip it) until then.
                    let lock = Session::take_lock(data_dir, candidate);
                    if lock.is_none() {
                        // No kernel lock to tell peers we are alive: a heartbeat
                        // file, touched by the worker, is the substitute.
                        let _ = std::fs::write(
                            crate::recovery::heartbeat_path(&Session::marker_path(
                                data_dir, candidate,
                            )),
                            b"",
                        );
                    }
                    // The marker stays a bare pid, readable by older builds.
                    let _ = write!(f, "{}", std::process::id());
                    break (candidate, lock);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    candidate += 1;
                }
                Err(e) => return Err(e),
            }
        };

        // Advance the hint (best-effort; the floor scan is the source of truth).
        let _ = std::fs::write(data_dir.join(GEN_FILE), generation.to_string());

        Ok(Session {
            data_dir: data_dir.to_path_buf(),
            generation,
            _lock: lock,
        })
    }

    /// Take `<gen>.lock`, or leave no lock file at all.
    ///
    /// A peer's probe never takes the lock (unix), but on Windows it momentarily
    /// does, so a contended claim is retried briefly. If it still cannot be held
    /// with kernel enforcement the file is removed: a lock file that exists but
    /// is not held reads as "that session died" to every peer, which would have
    /// them recover (and delete) this live session. Without the file peers fall
    /// back to the pid in the marker.
    fn take_lock(data_dir: &Path, generation: u64) -> Option<crate::claim::Claim> {
        let path = Session::lock_path(data_dir, generation);
        for attempt in 0..20 {
            match crate::claim::try_claim(&path) {
                Some(claim) if claim.is_os_locked() => return Some(claim),
                // Thread-only (the filesystem rejects locks): useless to peers.
                Some(_) => break,
                None if attempt < 19 => std::thread::sleep(std::time::Duration::from_millis(2)),
                None => break,
            }
        }
        let _ = std::fs::remove_file(&path);
        None
    }

    /// Keep this session's lock until the process exits instead of releasing it
    /// when the session is dropped. For an unclean shutdown, where a detached
    /// worker or uploader may still be writing this session's data: releasing the
    /// lock would let a peer recover the session as a crash and delete its parts
    /// underneath them. The kernel drops the lock when the process ends.
    pub(crate) fn keep_lock_until_exit(&mut self) {
        std::mem::forget(self._lock.take());
    }

    /// This session's generation number.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The liveness-marker path for `generation` under `data_dir`.
    pub fn marker_path(data_dir: &Path, generation: u64) -> PathBuf {
        data_dir
            .join(SESSIONS_DIR)
            .join(format!("{generation}.alive"))
    }

    /// The liveness lock-file path for `generation`. Its presence marks a session
    /// whose liveness is arbitrated by the lock rather than the marker's pid.
    pub(crate) fn lock_path(data_dir: &Path, generation: u64) -> PathBuf {
        data_dir
            .join(SESSIONS_DIR)
            .join(format!("{generation}.lock"))
    }

    /// Mark a clean shutdown: remove this session's liveness marker.
    pub fn end(&self) {
        let _ = std::fs::remove_file(Session::marker_path(&self.data_dir, self.generation));
        let _ = std::fs::remove_file(Session::lock_path(&self.data_dir, self.generation));
        let _ = std::fs::remove_file(crate::recovery::heartbeat_path(&Session::marker_path(
            &self.data_dir,
            self.generation,
        )));
    }
}

/// The highest generation number implied by on-disk state: the `gen` hint, any
/// `sessions/<n>.alive` marker, and any `parts/<n>` directory. Deriving the
/// floor from reality (not just the hint) prevents a lost `gen` file from
/// resetting to a lower generation and hiding un-recovered crashes.
fn floor_generation(data_dir: &Path) -> u64 {
    let mut floor = std::fs::read_to_string(data_dir.join(GEN_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0u64);

    floor = floor.max(max_numeric_entry(
        &data_dir.join(SESSIONS_DIR),
        Some(".alive"),
    ));
    floor = floor.max(max_numeric_entry(&data_dir.join("parts"), None));
    // Session-health records name generations too (a finished or acknowledged
    // one must never be shadowed by a reused number).
    floor = floor.max(crate::health::max_generation(data_dir));
    floor
}

/// The largest `u64` parsed from directory-entry names (optionally stripping a
/// suffix like `.alive`), or 0 if none.
fn max_numeric_entry(dir: &Path, strip_suffix: Option<&str>) -> u64 {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut max = 0;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = match strip_suffix {
            Some(sfx) => name.strip_suffix(sfx),
            None => Some(name.as_str()),
        };
        if let Some(n) = stem.and_then(|s| s.parse::<u64>().ok()) {
            max = max.max(n);
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{probe, Probe};

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bugsee-sess-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_live_session_holds_its_lock_and_a_clean_end_removes_it() {
        let dir = tmp();
        let session = Session::begin(&dir).unwrap();
        let lock = Session::lock_path(&dir, session.generation());
        assert!(matches!(probe(&lock), Probe::Held));
        session.end();
        assert!(!lock.exists());
    }

    #[test]
    fn a_session_that_cannot_hold_its_lock_leaves_no_lock_file_behind() {
        // A lock file that exists but is not held reads as "that session died".
        let dir = tmp();
        std::fs::create_dir_all(dir.join(SESSIONS_DIR)).unwrap();
        let peer = crate::claim::try_claim(&Session::lock_path(&dir, 1)).unwrap();
        let session = Session::begin(&dir).unwrap();
        assert_eq!(session.generation(), 1);
        assert!(session._lock.is_none());
        drop(peer);
        assert!(!Session::lock_path(&dir, 1).exists());
    }

    #[test]
    fn a_session_on_a_filesystem_without_locks_leaves_no_lock_file() {
        let dir = tmp();
        crate::claim::FORCE_UNSUPPORTED.with(|f| f.set(true));
        let session = Session::begin(&dir).unwrap();
        crate::claim::FORCE_UNSUPPORTED.with(|f| f.set(false));
        assert!(session._lock.is_none());
        assert!(!Session::lock_path(&dir, session.generation()).exists());
        // Instead it keeps a heartbeat file, removed on a clean end.
        let beat =
            crate::recovery::heartbeat_path(&Session::marker_path(&dir, session.generation()));
        assert!(beat.exists());
        session.end();
        assert!(!beat.exists());
    }

    #[test]
    fn keeping_the_lock_until_exit_survives_dropping_the_session() {
        let dir = tmp();
        let mut session = Session::begin(&dir).unwrap();
        let lock = Session::lock_path(&dir, session.generation());
        session.keep_lock_until_exit();
        drop(session);
        assert!(
            matches!(probe(&lock), Probe::Held),
            "a detached thread may still be writing: peers must not recover it"
        );
    }
}

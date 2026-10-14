//
//  health.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session health: a durable record of how each run of the application ended
//! (issue #12), the SDK half of release health / crash-free sessions.
//!
//! **Experimental and unstable; there is no backend contract for it.** The
//! backend derives crash-free sessions itself: it counts one session per
//! `POST /v2/sessions` and one crash per crash report received (`appserver`
//! `shared/statistics`: `sessions`, `issues.crashes`; crash-free =
//! `(sessions - crashes) / sessions`), and it has no endpoint that accepts a
//! session's end, status, duration or error count. These records are therefore
//! kept locally and never sent; what release health does send is the session
//! registration itself, made eagerly at launch (see `runtime`), because a
//! session the backend never hears of cannot be counted. Off by default.
//! DESIGN.md (release health) lists what the backend still has to define. This
//! module is `doc(hidden)` and may change without notice.
//!
//! # Lifecycle
//!
//! One **session** is one SDK launch (one generation), whatever the process
//! does: a desktop app, a long-running server and a short-lived CLI all start it
//! at launch and end it at a clean stop. There is no idle timeout; a server's
//! session is the process's life.
//!
//! ```text
//! launch        health/<gen>.live   written (status ok, 0 errors)
//! handled error health/<gen>.live   error count rewritten (rate limited)
//! clean stop    health/<gen>.json   written (ok | errored), then .live removed
//! crash / kill  .live stays; the NEXT launch's recovery finalizes it as
//!               crashed (crash evidence) or abnormal (none), then removes it
//! ```
//!
//! A non-main-thread panic is an **error**, not a crash: the process survives,
//! and counting it as a crash would corrupt crash-free rates.
//!
//! # Exactly once
//!
//! A finished record is named by its generation and written by whoever holds the
//! session (the live process at a clean stop, or the process holding
//! `sessions/<gen>.recover.lock` at recovery), via a temp file and a rename. A
//! finalizer that finds the record already there (or its `<gen>.acked`
//! tombstone, left by [`acknowledge`]) only removes the `.live` file, so a retry
//! after a crash between the two steps cannot flip a status or count a session
//! twice. Generations are not reused: the generation floor also reads this
//! directory, so even a wiped `gen` hint and marker tree cannot hand out a
//! generation that has a record.
//!
//! Limits: records of the legacy and extra recovery directories are not
//! surfaced (only the primary data directory's are); [`MAX_FINISHED`] prunes the
//! oldest records even if never acknowledged; an unacknowledged record of a
//! session recovered right at launch may not be listed until that recovery has
//! run (it is asynchronous past the launch-path budget); and a server that is
//! stopped by SIGTERM, `exec`s or calls `_exit` without dropping its guard ends
//! its session `abnormal`, not `ok`: drop the guard from the termination
//! handler (see the shutdown contract, #8).
//!
//! None of this runs on the crash path: the signal handler and panic hook write
//! nothing here. The crash is inferred at the next launch from the evidence the
//! handlers already leave.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::model::environment::Environment;

const HEALTH_DIR: &str = "health";
/// Newest finished records kept; older ones are pruned when a session finishes.
pub const MAX_FINISHED: usize = 256;

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionStatus {
    /// Ended cleanly with no handled errors.
    Ok,
    /// Ended cleanly but reported at least one handled error (including a
    /// panic on a non-main thread).
    Errored,
    /// The process died of a fatal signal or an uncaught panic.
    Crashed,
    /// The process vanished without evidence of a crash (SIGKILL, OOM kill,
    /// power loss).
    Abnormal,
}

/// The durable record of one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SessionRecord {
    /// Schema version of this record.
    pub version: u32,
    /// Random id, stable across the session's records.
    pub session_id: String,
    /// The generation in the data directory (monotonic, never reused).
    pub generation: u64,
    /// Epoch ms.
    pub started_at: i64,
    /// Epoch ms; for a crashed or abnormal session, the last activity seen.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ended_at: Option<i64>,
    pub status: SessionStatus,
    /// Handled errors counted in the session. Approximate for a session that
    /// died: counts after the last rewrite are lost.
    pub errors: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub package_id: Option<String>,
    /// The release: the application version and build.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub app_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub app_build: Option<String>,
    pub platform: String,
    pub sdk_version: String,
}

fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join(HEALTH_DIR)
}

fn live_path(data_dir: &Path, generation: u64) -> PathBuf {
    dir(data_dir).join(format!("{generation}.live"))
}

fn finished_path(data_dir: &Path, generation: u64) -> PathBuf {
    dir(data_dir).join(format!("{generation}.json"))
}

fn acked_path(data_dir: &Path, generation: u64) -> PathBuf {
    dir(data_dir).join(format!("{generation}.acked"))
}

/// The temp file for `path`: its own name plus `.tmp`, so `<gen>.live` and
/// `<gen>.json` never share one.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Write `record` atomically (temp file, then rename).
fn write_record(path: &Path, record: &SessionRecord) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let tmp = tmp_path(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

fn read_record(path: &Path) -> Option<SessionRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The live session's record, owned by the running process.
pub struct HealthTracker {
    data_dir: PathBuf,
    record: SessionRecord,
    /// Monotonic start, so a clean session's duration survives clock changes.
    started: Instant,
}

impl HealthTracker {
    /// Start tracking the session `generation` in `data_dir`. `None` when the
    /// record cannot be written (the session then simply is not tracked).
    pub fn start(data_dir: &Path, generation: u64, env: &Environment) -> Option<HealthTracker> {
        // An empty path would resolve against the working directory.
        if data_dir.as_os_str().is_empty() {
            return None;
        }
        std::fs::create_dir_all(dir(data_dir)).ok()?;
        let record = SessionRecord {
            version: 1,
            session_id: crate::util::random_hex(8),
            generation,
            started_at: crate::util::epoch_ms(),
            ended_at: None,
            status: SessionStatus::Ok,
            errors: 0,
            duration_ms: None,
            package_id: env.app.package_id.clone(),
            app_version: env.app.version.clone(),
            app_build: env.app.build.clone(),
            platform: env.platform.os_type.clone(),
            sdk_version: env.sdk.version.clone(),
        };
        write_record(&live_path(data_dir, generation), &record).ok()?;
        Some(HealthTracker {
            data_dir: data_dir.to_path_buf(),
            record,
            started: Instant::now(),
        })
    }

    /// Bring the record up to `errors` handled errors (the process-wide counter,
    /// which the reporting path bumps without touching the disk) and persist it
    /// if that changed anything. Called from the worker's tick, never from a
    /// caller's thread.
    pub fn sync_errors(&mut self, errors: u64) {
        if errors == self.record.errors {
            return;
        }
        self.record.errors = errors;
        if errors > 0 {
            self.record.status = SessionStatus::Errored;
        }
        let _ = write_record(
            &live_path(&self.data_dir, self.record.generation),
            &self.record,
        );
    }

    /// The directory this session's records live in.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The record as it stands.
    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    /// End the session cleanly: publish the finished record, then drop the live
    /// one.
    pub fn finish_clean(&self, errors: u64) {
        let duration = self.started.elapsed().as_millis() as i64;
        let mut record = self.record.clone();
        record.errors = errors;
        record.status = if errors > 0 {
            SessionStatus::Errored
        } else {
            SessionStatus::Ok
        };
        record.ended_at = Some(record.started_at + duration);
        record.duration_ms = Some(duration);
        publish(&self.data_dir, record);
    }
}

/// Finish a session that died, from the process that recovered it. Marks it
/// `status` (`Crashed` or `Abnormal`) unless a finished record already exists,
/// which is left alone. Idempotent; a missing live record (release health was
/// off for that run) does nothing.
pub fn finalize_dead(
    data_dir: &Path,
    generation: u64,
    status: SessionStatus,
    last_activity: Option<i64>,
) {
    let live = live_path(data_dir, generation);
    let Some(mut record) = read_record(&live) else {
        // Unreadable or absent: nothing to report, but do not leave junk.
        let _ = std::fs::remove_file(&live);
        let _ = std::fs::remove_file(tmp_path(&live));
        return;
    };
    record.status = status;
    // The last sign of life: the newest capture part, or the live record's own
    // last write (an error count), whichever is later.
    let live_written = std::fs::metadata(&live)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64);
    let end = last_activity
        .into_iter()
        .chain(live_written)
        .fold(record.started_at, i64::max);
    record.ended_at = Some(end);
    record.duration_ms = Some(end - record.started_at);
    publish(data_dir, record);
}

/// Write the finished record unless one exists, then remove the live one.
fn publish(data_dir: &Path, record: SessionRecord) {
    let done = finished_path(data_dir, record.generation);
    // Settled already: the host took it (tombstone) or an intact record exists.
    // An unreadable one (zero-length after a power loss) is rewritten.
    let settled = acked_path(data_dir, record.generation).exists() || read_record(&done).is_some();
    if !settled {
        let _ = write_record(&done, &record);
    }
    // Only drop the live record once the finished one is durable: a failure
    // leaves it for the next recovery, which finds the finished one and stops.
    if settled || read_record(&done).is_some() {
        let _ = std::fs::remove_file(live_path(data_dir, record.generation));
    }
    prune(data_dir);
}

fn prune(data_dir: &Path) {
    let mut gens = finished_generations(data_dir);
    if gens.len() > MAX_FINISHED {
        gens.sort_unstable();
        for g in &gens[..gens.len() - MAX_FINISHED] {
            let _ = std::fs::remove_file(finished_path(data_dir, *g));
        }
    }
}

fn finished_generations(data_dir: &Path) -> Vec<u64> {
    let Ok(read) = std::fs::read_dir(dir(data_dir)) else {
        return Vec::new();
    };
    read.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json")?.parse().ok()
        })
        .collect()
}

/// Finished sessions in `data_dir`, oldest first, that have not been
/// [`acknowledge`]d.
pub fn finished(data_dir: &Path) -> Vec<SessionRecord> {
    if data_dir.as_os_str().is_empty() {
        return Vec::new();
    }
    let mut gens = finished_generations(data_dir);
    gens.sort_unstable();
    gens.into_iter()
        .filter_map(|g| read_record(&finished_path(data_dir, g)))
        .collect()
}

/// Forget a finished session once the host (or a future transport) has taken it.
/// Leaves a `<gen>.acked` tombstone, so a finalizer that still finds the
/// session's marker (a recovery racing the acknowledgement) cannot write the
/// record again and have it counted twice. [`gc`] drops the tombstone once the
/// marker is gone.
pub fn acknowledge(data_dir: &Path, generation: u64) {
    if data_dir.as_os_str().is_empty() {
        return;
    }
    let _ = std::fs::write(acked_path(data_dir, generation), b"");
    let _ = std::fs::remove_file(finished_path(data_dir, generation));
}

/// Sweep leftovers of generations below `current` that no longer have a liveness
/// marker (`alive` says whether one does): tombstones, whose job is done, and
/// `.live` records no recovery will ever finish. Called from
/// [`crate::recovery::gc_orphans`].
pub(crate) fn gc(data_dir: &Path, current: u64, alive: impl Fn(u64) -> bool) {
    let Ok(read) = std::fs::read_dir(dir(data_dir)) else {
        return;
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((stem, ext)) = name.split_once('.') else {
            continue;
        };
        let Ok(generation) = stem.parse::<u64>() else {
            continue;
        };
        if generation >= current || alive(generation) {
            continue;
        }
        if matches!(ext, "acked" | "live" | "live.tmp" | "json.tmp") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The highest generation any record here names, for the generation floor.
pub(crate) fn max_generation(data_dir: &Path) -> u64 {
    std::fs::read_dir(dir(data_dir))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.split('.').next()?.parse::<u64>().ok()
        })
        .max()
        .unwrap_or(0)
}

/// The newest modification time (epoch ms) under `dir`, to `depth` levels down.
/// A dead session's last-activity stamp: the capture parts are written as it
/// runs.
pub(crate) fn newest_mtime(dir: &Path, depth: u32) -> Option<i64> {
    let mut newest = None::<i64>;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        // Crash evidence is written at the moment of death, not by capture.
        let name = entry.file_name();
        if matches!(
            name.to_str(),
            Some("panic.info" | "panic.unwinding" | "crash.info" | "crash.modules")
        ) {
            continue;
        }
        // Rotation creates empty part directories whether or not anything was
        // captured; only files are capture activity.
        if meta.is_dir() && depth == 0 {
            continue;
        }
        let mut t = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);
        if meta.is_dir() && depth > 0 {
            t = t.max(newest_mtime(&entry.path(), depth - 1));
        }
        newest = newest.max(t);
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bugsee-health-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn env() -> Environment {
        Environment::detect_with(
            "9.9.9",
            &Default::default(),
            &crate::model::environment::AppIdentity {
                package_id: Some("app".into()),
                version: Some("1.2.3".into()),
                build: Some("45".into()),
            },
        )
    }

    #[test]
    fn a_clean_session_is_ok_and_carries_the_release() {
        let d = tmp();
        let t = HealthTracker::start(&d, 1, &env()).unwrap();
        assert!(live_path(&d, 1).exists());
        t.finish_clean(0);
        assert!(!live_path(&d, 1).exists());
        let all = finished(&d);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, SessionStatus::Ok);
        assert_eq!(all[0].errors, 0);
        assert_eq!(all[0].app_version.as_deref(), Some("1.2.3"));
        assert_eq!(all[0].app_build.as_deref(), Some("45"));
        assert!(all[0].duration_ms.unwrap() >= 0);
    }

    #[test]
    fn handled_errors_make_the_session_errored_not_crashed() {
        let d = tmp();
        let mut t = HealthTracker::start(&d, 1, &env()).unwrap();
        t.sync_errors(2);
        t.finish_clean(2);
        let r = &finished(&d)[0];
        assert_eq!((r.status, r.errors), (SessionStatus::Errored, 2));
    }

    #[test]
    fn a_dead_session_is_finalized_once_whoever_asks() {
        let d = tmp();
        let _t = HealthTracker::start(&d, 3, &env()).unwrap();
        finalize_dead(&d, 3, SessionStatus::Crashed, None);
        // A second finalizer (a retry after a crash between the two steps, with
        // the live record put back) must not flip or duplicate the status.
        let _t2 = HealthTracker::start(&d, 3, &env());
        finalize_dead(&d, 3, SessionStatus::Abnormal, None);
        let all = finished(&d);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, SessionStatus::Crashed);
        assert!(!live_path(&d, 3).exists());
    }

    #[test]
    fn finalizing_a_session_that_was_not_tracked_does_nothing() {
        let d = tmp();
        finalize_dead(&d, 5, SessionStatus::Crashed, None);
        assert!(finished(&d).is_empty());
    }

    #[test]
    fn acknowledged_and_old_records_are_dropped() {
        let d = tmp();
        for g in 1..=(MAX_FINISHED as u64 + 3) {
            HealthTracker::start(&d, g, &env()).unwrap().finish_clean(0);
        }
        let all = finished(&d);
        assert_eq!(all.len(), MAX_FINISHED);
        assert_eq!(all[0].generation, 4);
        acknowledge(&d, 4);
        assert_eq!(finished(&d).len(), MAX_FINISHED - 1);
    }

    #[test]
    fn an_acknowledged_session_is_not_counted_again_by_a_late_finalizer() {
        let d = tmp();
        let t = HealthTracker::start(&d, 2, &env()).unwrap();
        t.finish_clean(0);
        acknowledge(&d, 2);
        assert!(finished(&d).is_empty());
        // The marker survived (a recovery racing the acknowledgement): the
        // finalizer finds the tombstone and writes nothing.
        let _again = HealthTracker::start(&d, 2, &env());
        finalize_dead(&d, 2, SessionStatus::Crashed, None);
        assert!(finished(&d).is_empty());
        assert!(!live_path(&d, 2).exists());
        // Once the marker is gone the tombstone is swept.
        gc(&d, 10, |_| false);
        assert!(!acked_path(&d, 2).exists());
    }

    #[test]
    fn gc_keeps_what_a_live_marker_or_current_generation_still_needs() {
        let d = tmp();
        HealthTracker::start(&d, 1, &env()).unwrap();
        HealthTracker::start(&d, 2, &env()).unwrap();
        HealthTracker::start(&d, 3, &env()).unwrap();
        // 1: marker present (a peer); 2: no marker, below current (junk); 3: current.
        gc(&d, 3, |g| g == 1);
        assert!(live_path(&d, 1).exists());
        assert!(!live_path(&d, 2).exists());
        assert!(live_path(&d, 3).exists());
    }

    #[test]
    fn a_zero_length_finished_record_is_rewritten() {
        let d = tmp();
        std::fs::create_dir_all(dir(&d)).unwrap();
        std::fs::write(finished_path(&d, 4), b"").unwrap();
        HealthTracker::start(&d, 4, &env()).unwrap().finish_clean(0);
        assert_eq!(finished(&d).len(), 1);
    }

    #[test]
    fn records_raise_the_generation_floor_and_temp_files_do_not_collide() {
        let d = tmp();
        HealthTracker::start(&d, 7, &env()).unwrap();
        assert_eq!(max_generation(&d), 7);
        assert_ne!(tmp_path(&live_path(&d, 7)), tmp_path(&finished_path(&d, 7)));
        // A wiped hint and marker tree still cannot reuse generation 7.
        let session = crate::session::Session::begin(&d).unwrap();
        assert_eq!(session.generation(), 8);
    }
}

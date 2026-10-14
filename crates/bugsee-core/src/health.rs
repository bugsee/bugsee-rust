//
//  health.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session health: a durable record of how each run of the application ended
//! (issue #12), the SDK half of release health / crash-free sessions.
//!
//! **There is no backend half yet.** The Bugsee backend derives crash-free
//! sessions itself: it counts one session per `POST /v2/sessions` and one crash
//! per crash issue (`appserver` `shared/statistics`: `sessions`,
//! `issues.crashes`; crash-free = `(sessions - crashes) / sessions`), and it has
//! no endpoint that accepts a session's end, status, duration or error count.
//! Nothing here is sent anywhere. The records are kept locally, off by default
//! ([`HealthTracker`] exists only when the host asked for release health), and
//! are exposed through [`finished`] / [`acknowledge`] so a host can use them
//! today and a transport can ship them once a contract exists. DESIGN.md
//! (release health) lists what the backend still has to define.
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
//! finalizer that finds the record already there only removes the `.live` file,
//! so a retry after a crash between the two steps cannot flip a status or count
//! a session twice. Generations are never reused in a data directory.
//!
//! None of this runs on the crash path: the signal handler and panic hook write
//! nothing here. The crash is inferred at the next launch from the evidence the
//! handlers already leave.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::environment::Environment;

const HEALTH_DIR: &str = "health";
/// Newest finished records kept; older ones are pruned when a session finishes.
pub const MAX_FINISHED: usize = 256;
/// Error counts are rewritten to disk for the first few errors and then at each
/// power of two, so an error storm costs a handful of small writes, not one each.
const EAGER_WRITES: u64 = 8;

/// How a session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// Write `record` atomically (temp file, then rename).
fn write_record(path: &Path, record: &SessionRecord) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
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
}

impl HealthTracker {
    /// Start tracking the session `generation` in `data_dir`. `None` when the
    /// record cannot be written (the session then simply is not tracked).
    pub fn start(data_dir: &Path, generation: u64, env: &Environment) -> Option<HealthTracker> {
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
        })
    }

    /// Count one handled error.
    pub fn note_error(&mut self) {
        self.record.errors = self.record.errors.saturating_add(1);
        self.record.status = SessionStatus::Errored;
        let n = self.record.errors;
        if n <= EAGER_WRITES || n.is_power_of_two() {
            let _ = write_record(
                &live_path(&self.data_dir, self.record.generation),
                &self.record,
            );
        }
    }

    /// The record as it stands.
    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    /// End the session cleanly: publish the finished record, then drop the live
    /// one.
    pub fn finish_clean(&self) {
        let now = crate::util::epoch_ms();
        let mut record = self.record.clone();
        record.ended_at = Some(now);
        record.duration_ms = Some((now - record.started_at).max(0));
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
        let _ = std::fs::remove_file(live.with_extension("tmp"));
        return;
    };
    record.status = status;
    let end = last_activity
        .unwrap_or(record.started_at)
        .max(record.started_at);
    record.ended_at = Some(end);
    record.duration_ms = Some(end - record.started_at);
    publish(data_dir, record);
}

/// Write the finished record unless one exists, then remove the live one.
fn publish(data_dir: &Path, record: SessionRecord) {
    let done = finished_path(data_dir, record.generation);
    if !done.exists() {
        let _ = write_record(&done, &record);
    }
    // Only drop the live record once the finished one is durable: a failure
    // leaves it for the next recovery, which finds the finished one and stops.
    if done.exists() {
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
    let mut gens = finished_generations(data_dir);
    gens.sort_unstable();
    gens.into_iter()
        .filter_map(|g| read_record(&finished_path(data_dir, g)))
        .collect()
}

/// Forget a finished session once the host (or a future transport) has taken it.
pub fn acknowledge(data_dir: &Path, generation: u64) {
    let _ = std::fs::remove_file(finished_path(data_dir, generation));
}

/// The newest modification time (epoch ms) under `dir`, to `depth` levels down.
/// A dead session's last-activity stamp: the capture parts are written as it
/// runs.
pub(crate) fn newest_mtime(dir: &Path, depth: u32) -> Option<i64> {
    let mut newest = None::<i64>;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
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
        t.finish_clean();
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
        t.note_error();
        t.note_error();
        t.finish_clean();
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
            HealthTracker::start(&d, g, &env()).unwrap().finish_clean();
        }
        let all = finished(&d);
        assert_eq!(all.len(), MAX_FINISHED);
        assert_eq!(all[0].generation, 4);
        acknowledge(&d, 4);
        assert_eq!(finished(&d).len(), MAX_FINISHED - 1);
    }
}

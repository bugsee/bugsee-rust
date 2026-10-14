//
//  storage_failures.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! An unusable data directory must degrade, never fail the host.
//!
//! Each test drives a real `Recorder` through one failure mode and asserts the
//! documented outcome: no panic, no hang, and the report still reaches the
//! transport (from memory where the disk refuses it).
//!
//! Failures are injected by making a path component unusable (a regular file
//! where a directory is needed, a read-only directory, a directory removed
//! mid-run). A true ENOSPC needs a size-capped filesystem and root to mount one;
//! writing under a path blocked by a regular file fails the same way at the
//! same call sites (`create_dir_all` / `write` returning an `io::Error`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::model::entry::{CaptureEntry, EventEntry};
use bugsee_core::reporting::manual_upload_meta;
use bugsee_core::runtime::StorageMode;
use bugsee_core::util::epoch_ms;
use bugsee_core::{MockTransport, Recorder, RecorderConfig};
use serde_json::Map;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bugsee-storage-{}",
            bugsee_core::util::random_hex(8)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        restore_permissions(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(unix)]
fn restore_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn restore_permissions(_path: &Path) {}

fn event(name: &str) -> CaptureEntry {
    CaptureEntry::UserEvent(EventEntry {
        timestamp: epoch_ms(),
        name: Some(name.into()),
        params: Map::new(),
        custom: Map::new(),
    })
}

/// Report once and wait for delivery; returns how many bundles arrived.
fn report_and_flush(recorder: &Recorder, transport: &MockTransport) -> usize {
    recorder.capture(event("before"));
    recorder.report(manual_upload_meta(), None);
    assert!(
        recorder.flush(Duration::from_secs(10)),
        "flush must complete, not hang"
    );
    let n = transport.uploaded_bundles.lock().unwrap().len();
    n
}

/// A path that can never be created: a child of a regular file.
fn blocked_path(dir: &Path) -> PathBuf {
    let file = dir.join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    file.join("data")
}

#[test]
fn missing_uncreatable_directory_runs_in_memory_and_still_reports() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());

    let recorder = Recorder::launch(
        RecorderConfig::new(blocked_path(&dir.path), "T"),
        transport.clone(),
    )
    .expect("an unusable directory must not fail launch");

    assert_eq!(recorder.storage_mode(), StorageMode::Memory);
    assert!(recorder.storage_reason().is_some());
    assert_eq!(report_and_flush(&recorder, &transport), 1);
}

#[test]
fn memory_only_config_skips_the_disk_entirely() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let data_dir = dir.path.join("must-stay-absent");
    let mut config = RecorderConfig::new(&data_dir, "T");
    config.memory_only = true;

    let recorder = Recorder::launch(config, transport.clone()).expect("launch");

    assert_eq!(recorder.storage_mode(), StorageMode::Memory);
    assert_eq!(report_and_flush(&recorder, &transport), 1);
    drop(recorder);
    assert!(!data_dir.exists(), "memory mode must not touch the disk");
}

#[cfg(unix)]
#[test]
fn read_only_directory_runs_in_memory_and_still_reports() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let ro = dir.path.join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    // root ignores permission bits, which would make this test vacuous.
    if std::fs::write(ro.join("probe"), b"x").is_ok() {
        eprintln!("skipped: permission bits are not enforced for this user");
        return;
    }
    let transport = Arc::new(MockTransport::default());

    let recorder = Recorder::launch(RecorderConfig::new(&ro, "T"), transport.clone())
        .expect("a read-only directory must not fail launch");

    assert_eq!(recorder.storage_mode(), StorageMode::Memory);
    assert_eq!(report_and_flush(&recorder, &transport), 1);
}

#[test]
fn directory_removed_mid_run_still_reports() {
    let dir = TempDir::new();
    let data_dir = dir.path.join("data");
    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&data_dir, "T"), transport.clone()).expect("launch");
    assert_eq!(recorder.storage_mode(), StorageMode::Disk);
    recorder.capture(event("early"));
    assert!(recorder.flush(Duration::from_secs(10)));

    // The volume vanishes under a running process.
    std::fs::remove_dir_all(&data_dir).unwrap();

    assert_eq!(report_and_flush(&recorder, &transport), 1);
    // Capture and shutdown after the loss must not panic either.
    recorder.capture(event("late"));
    drop(recorder);
}

#[test]
fn directory_replaced_by_a_file_mid_run_falls_back_to_memory_delivery() {
    let dir = TempDir::new();
    let data_dir = dir.path.join("data");
    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&data_dir, "T"), transport.clone()).expect("launch");

    // Nothing under the data dir can be created any more: every disk write site
    // (parts, report snapshot, queue) fails.
    std::fs::remove_dir_all(&data_dir).unwrap();
    std::fs::write(&data_dir, b"a file now").unwrap();

    assert_eq!(report_and_flush(&recorder, &transport), 1);
    drop(recorder);
}

#[test]
fn queue_write_failure_holds_the_report_in_memory() {
    let dir = TempDir::new();
    let data_dir = dir.path.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    // The queue directory cannot be created: the enqueue step fails while the
    // capture parts and the liveness marker work. This is what a full disk or a
    // quota looks like at the queue.
    std::fs::write(data_dir.join("queue"), b"blocker").unwrap();
    let transport = Arc::new(MockTransport::default());

    let recorder =
        Recorder::launch(RecorderConfig::new(&data_dir, "T"), transport.clone()).expect("launch");
    assert_eq!(recorder.storage_mode(), StorageMode::Disk);

    assert_eq!(report_and_flush(&recorder, &transport), 1);
}

#[test]
fn memory_backlog_is_bounded_and_overflow_is_reported() {
    use bugsee_core::runtime::DropReason;
    use bugsee_core::transport::{Transport, TransportError};
    use std::sync::Mutex;

    // Never delivers, so everything stays in the memory backlog.
    struct Offline;
    impl Transport for Offline {
        fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
            Err(TransportError::Transient("offline".into()))
        }
        fn create_issue(
            &self,
            _: &str,
            _: Option<&str>,
            _: &[u8],
        ) -> Result<String, TransportError> {
            Err(TransportError::Transient("offline".into()))
        }
        fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
            Err(TransportError::Transient("offline".into()))
        }
    }

    let dir = TempDir::new();
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let sink = dropped.clone();
    let mut config = RecorderConfig::new(blocked_path(&dir.path), "T");
    config.on_report_dropped = Some(Box::new(move |r| sink.lock().unwrap().push(r)));
    let recorder = Recorder::launch(config, Arc::new(Offline)).expect("launch");

    for _ in 0..40 {
        recorder.report(manual_upload_meta(), None);
    }
    // Flush cannot succeed (offline) but must return promptly, not hang.
    assert!(!recorder.flush(Duration::from_millis(500)));

    let seen = dropped.lock().unwrap().clone();
    assert!(
        !seen.is_empty() && seen.iter().all(|r| *r == DropReason::StorageUnavailable),
        "overflow must be surfaced, got {seen:?}"
    );
}

//
//  launch_recovery.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Early-crash recovery finishes on the launch path (issue #10): by the time
//! `launch` returns, the previous run's crash is a durable queue entry, so a host
//! that crashes again right away (a crash loop) cannot outrun its report.
//!
//! The crash loop is real: this test binary re-executes itself as children that
//! launch the SDK and `abort()` immediately.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bugsee_core::transport::{Transport, TransportError};
use bugsee_core::{queue, MockTransport, Recorder, RecorderConfig};

const ROLE: &str = "BUGSEE_LR_ROLE";
const DIR: &str = "BUGSEE_LR_DIR";
const OUT: &str = "BUGSEE_LR_OUT";

/// A transport that never delivers anything, so the queue keeps its entries.
struct Offline;
impl Transport for Offline {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
}

/// Child entry point. A no-op in a normal test run.
#[test]
fn child_entry() {
    if std::env::var(ROLE).as_deref() != Ok("crash_after_launch") {
        return;
    }
    let dir = PathBuf::from(std::env::var(DIR).unwrap());
    let out = PathBuf::from(std::env::var(OUT).unwrap());
    let _rec = Recorder::launch(RecorderConfig::new(&dir, "tok"), Arc::new(Offline)).unwrap();
    // The instant `launch` returns: record what is durably queued, then die the
    // way a crash does, with no destructor and no flush.
    std::fs::write(out, queue::list_pending(&dir).len().to_string()).unwrap();
    std::process::abort();
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p =
            std::env::temp_dir().join(format!("bugsee-lr-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seed_crashed(dir: &Path, generations: std::ops::RangeInclusive<u64>) {
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for g in generations.clone() {
        // "0" is the portable "owner gone" sentinel.
        std::fs::write(sessions.join(format!("{g}.alive")), "0").unwrap();
        std::fs::create_dir_all(dir.join("parts").join(g.to_string())).unwrap();
    }
    std::fs::write(dir.join("gen"), generations.end().to_string()).unwrap();
}

#[test]
fn the_previous_crash_is_queued_before_launch_returns() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=1);

    let rec = Recorder::launch(RecorderConfig::new(&dir, "tok"), Arc::new(Offline)).unwrap();
    // No flush, no waiting: it is on disk the moment launch returns.
    assert_eq!(queue::list_pending(&dir).len(), 1);
    drop(rec);
}

/// Several pending crashes, a time cap, and newest-first: the hook observes which
/// thread recovers each. With a budget shorter than one session only the newest
/// is recovered on the launch path; the rest follow on the worker.
#[test]
fn the_launch_path_is_capped_by_time_and_the_rest_follows_in_the_background() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=5);

    let threads: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&threads);
    let mut config = RecorderConfig::new(&dir, "tok");
    config.launch_recovery_budget = Duration::from_millis(150);
    config.before_send = Some(Box::new(move |_meta| {
        let t = std::thread::current();
        seen.lock()
            .unwrap()
            .push(t.name().unwrap_or("launch").to_string());
        std::thread::sleep(Duration::from_millis(100));
        true
    }));
    let transport = Arc::new(MockTransport::default());
    let me = std::thread::current().name().map(String::from);
    let rec = Recorder::launch(config, transport.clone()).unwrap();

    let after_launch = threads.lock().unwrap().clone();
    // 100 ms per session against a 150 ms budget: two start, a third does not.
    let on_launch_path = after_launch
        .iter()
        .filter(|n| Some(n.as_str()) != Some("bugsee-capture"))
        .count();
    assert_eq!(
        on_launch_path, 2,
        "hook threads: {after_launch:?}, test thread {me:?}"
    );

    assert!(rec.flush(Duration::from_secs(10)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 5);
    let all = threads.lock().unwrap().clone();
    assert_eq!(
        all.len(),
        5,
        "each crash was processed exactly once: {all:?}"
    );
}

#[test]
fn a_zero_budget_leaves_recovery_to_the_worker() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=2);

    let threads: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&threads);
    let mut config = RecorderConfig::new(&dir, "tok");
    config.launch_recovery_budget = Duration::ZERO;
    config.before_send = Some(Box::new(move |_meta| {
        let t = std::thread::current();
        seen.lock()
            .unwrap()
            .push(t.name().unwrap_or("launch").to_string());
        true
    }));
    let transport = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config, transport.clone()).unwrap();
    assert!(rec.flush(Duration::from_secs(10)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 2);
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .all(|n| n == "bugsee-capture"));
}

/// The product scenario: the host crashes right after every launch. Each launch
/// must already hold the previous crash in the queue, and a later healthy run
/// delivers every one of them exactly once.
#[test]
fn a_crash_loop_loses_no_crash_and_reports_each_exactly_once() {
    const CRASHES: usize = 5;
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    std::fs::create_dir_all(&dir).unwrap();

    for i in 0..CRASHES {
        let out = tmp.0.join(format!("queued-{i}"));
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_entry", "--nocapture"])
            .env(ROLE, "crash_after_launch")
            .env(DIR, &dir)
            .env(OUT, &out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "child {i} should have crashed");
        let queued: usize = std::fs::read_to_string(&out)
            .unwrap_or_else(|_| panic!("child {i} never got past launch"))
            .parse()
            .unwrap();
        // Child i found the crashes of children 0..i already queued, with
        // the previous child's among them: none waited for an async worker.
        assert_eq!(queued, i, "child {i} saw {queued} queued crashes");
    }

    // A healthy run: everything is delivered, once.
    let transport = Arc::new(MockTransport::default());
    let rec = Recorder::launch(RecorderConfig::new(&dir, "tok"), transport.clone()).unwrap();
    assert!(rec.flush(Duration::from_secs(20)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), CRASHES);
    assert!(queue::list_pending(&dir).is_empty());
    drop(rec);

    // And not again.
    let again = Arc::new(MockTransport::default());
    let rec = Recorder::launch(RecorderConfig::new(&dir, "tok"), again.clone()).unwrap();
    assert!(rec.flush(Duration::from_secs(5)));
    assert!(again.uploaded_bundles.lock().unwrap().is_empty());
}

//
//  release_health.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Issue #12 at the recorder level: eager backend-session registration, health
//! records on a slow stop, and memory mode.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bugsee_core::health::{self, SessionStatus};
use bugsee_core::model::enums::{IssueType, Severity, TriggerType};
use bugsee_core::model::report::Source;
use bugsee_core::transport::{Transport, TransportError};
use bugsee_core::{MockTransport, Recorder, RecorderConfig, ReportMeta};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "bugsee-health-core-{name}-{}",
        bugsee_core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn config(dir: &std::path::Path, health: bool) -> RecorderConfig {
    let mut c = RecorderConfig::new(dir, "tok");
    c.release_health = health;
    c
}

fn error_meta() -> ReportMeta {
    ReportMeta {
        issue_type: IssueType::Error,
        summary: Some("e".into()),
        description: None,
        labels: Vec::new(),
        severity: Severity::default(),
        email: None,
        signatures: vec!["sig-health".into()],
        source: Source {
            trigger: TriggerType::Error,
            origin: None,
        },
        attrs: Default::default(),
        attachments: Vec::new(),
    }
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn release_health_registers_the_backend_session_once_at_launch() {
    let d = dir("eager");
    let mock = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config(&d, true), mock.clone()).unwrap();
    // Nothing was reported: only the eager registration can have run.
    wait_until("the eager registration", || {
        *mock.sessions.lock().unwrap() == 1
    });

    // A later delivery reuses that session instead of registering another.
    rec.report(error_meta(), None);
    assert!(rec.flush(Duration::from_secs(10)));
    assert_eq!(mock.created_requests.lock().unwrap().len(), 1);
    assert_eq!(*mock.sessions.lock().unwrap(), 1, "one session per launch");
}

#[test]
fn without_release_health_registration_stays_lazy() {
    let d = dir("lazy");
    let mock = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config(&d, false), mock.clone()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(*mock.sessions.lock().unwrap(), 0);
    drop(rec);
}

/// Offline at launch, then online: launch is unaffected and the session is still
/// registered exactly once, by the lazy path of the first delivery.
struct FlakyRegister {
    inner: MockTransport,
    attempts: AtomicU32,
    fail_first: u32,
}
impl Transport for FlakyRegister {
    fn register_session(&self, t: &str, e: &[u8]) -> Result<String, TransportError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) < self.fail_first {
            return Err(TransportError::Transient("offline".into()));
        }
        self.inner.register_session(t, e)
    }
    fn create_issue(&self, t: &str, a: Option<&str>, r: &[u8]) -> Result<String, TransportError> {
        self.inner.create_issue(t, a, r)
    }
    fn upload_bundle(&self, u: &str, z: &[u8]) -> Result<(), TransportError> {
        self.inner.upload_bundle(u, z)
    }
}

#[test]
fn a_failed_eager_registration_never_fails_launch_and_is_not_repeated_by_delivery() {
    let d = dir("offline");
    let flaky = Arc::new(FlakyRegister {
        inner: MockTransport::default(),
        attempts: AtomicU32::new(0),
        fail_first: 1,
    });
    let started = Instant::now();
    let rec = Recorder::launch(config(&d, true), flaky.clone()).expect("launch must not fail");
    assert!(started.elapsed() < Duration::from_secs(5));
    wait_until("the failed eager attempt", || {
        flaky.attempts.load(Ordering::SeqCst) >= 1
    });
    rec.report(error_meta(), None);
    assert!(rec.flush(Duration::from_secs(10)));
    assert_eq!(flaky.inner.created_requests.lock().unwrap().len(), 1);
    assert_eq!(*flaky.inner.sessions.lock().unwrap(), 1);
}

/// A transport whose delivery hangs longer than a stop waits.
struct Hung;
impl Transport for Hung {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        std::thread::sleep(Duration::from_millis(3500));
        Err(TransportError::Transient("hung".into()))
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("hung".into()))
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Transient("hung".into()))
    }
}

#[test]
fn a_stop_whose_uploader_is_hung_still_finishes_the_health_record() {
    let d = dir("hung");
    let rec = Recorder::launch(config(&d, true), Arc::new(Hung)).unwrap();
    rec.report(error_meta(), None);
    let started = Instant::now();
    drop(rec); // the uploader is stuck in the transport: the stop gives up on it
    assert!(started.elapsed() < Duration::from_secs(10));
    let all = health::finished(&d);
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].status, SessionStatus::Errored);
    assert_eq!(all[0].errors, 1);
}

#[test]
fn memory_mode_tracks_nothing_and_lists_nothing() {
    let d = dir("memory");
    // A finished record from a disk run must not be listed by a memory-mode run.
    let first = Recorder::launch(config(&d, true), Arc::new(MockTransport::default())).unwrap();
    drop(first);
    assert_eq!(health::finished(&d).len(), 1);

    let mut cfg = config(&d, true);
    cfg.memory_only = Some("test".into());
    let rec = Recorder::launch(cfg, Arc::new(MockTransport::default())).unwrap();
    assert!(rec.session_health().is_none());
    assert!(rec.finished_sessions().is_empty());
    rec.handle().acknowledge_session(1);
    drop(rec);
    assert_eq!(
        health::finished(&d).len(),
        1,
        "memory mode must not touch records"
    );
}

#[test]
fn an_empty_data_dir_is_rejected() {
    let env = bugsee_core::Environment::detect_with("0", &Default::default(), &Default::default());
    assert!(health::HealthTracker::start(std::path::Path::new(""), 1, &env).is_none());
    assert!(health::finished(std::path::Path::new("")).is_empty());
}

#[test]
fn a_memory_mode_launch_is_not_registered_eagerly() {
    // It could never contribute a recovered crash, so counting it as a session
    // would read healthy.
    let d = dir("memory-eager");
    let mock = Arc::new(MockTransport::default());
    let mut cfg = config(&d, true);
    cfg.memory_only = Some("test".into());
    let rec = Recorder::launch(cfg, mock.clone()).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(*mock.sessions.lock().unwrap(), 0);
    drop(rec);
}

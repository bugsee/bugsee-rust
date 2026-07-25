//
//  durable_queue.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 5 hardening: the durable outbound queue retries transient failures and
//! survives process restarts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bugsee_core::transport::{Transport, TransportError};
use bugsee_core::{queue, MockTransport, Recorder, RecorderConfig};

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-queue-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn log_entry() -> bugsee_core::CaptureEntry {
    use bugsee_core::model::entry::LogEntry;
    use bugsee_core::{LogLevel, LogSource};
    bugsee_core::CaptureEntry::Log(LogEntry {
        timestamp: bugsee_core::util::epoch_ms(),
        level: LogLevel::Info,
        source: LogSource::StdOut,
        tag: None,
        message: Some("x".into()),
        custom: Default::default(),
    })
}

/// Fails the first `fail_uploads` upload attempts (transiently), then succeeds.
#[derive(Default)]
struct FlakyTransport {
    remaining_failures: AtomicU32,
    uploaded: Mutex<u32>,
}
impl FlakyTransport {
    fn new(fail: u32) -> Self {
        FlakyTransport {
            remaining_failures: AtomicU32::new(fail),
            uploaded: Mutex::new(0),
        }
    }
}
impl Transport for FlakyTransport {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("t".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Ok("endpoint".into())
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        if self.remaining_failures.load(Ordering::SeqCst) > 0 {
            self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
            return Err(TransportError::Transient("flaky".into()));
        }
        *self.uploaded.lock().unwrap() += 1;
        Ok(())
    }
}

/// Always fails uploads transiently (create_issue succeeds).
struct AlwaysFailUpload;
impl Transport for AlwaysFailUpload {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("t".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Ok("endpoint".into())
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
}

fn config(dir: &std::path::Path) -> RecorderConfig {
    let mut c = RecorderConfig::new(dir, "TOKEN");
    // Fast backoff so retries resolve within the test.
    c.upload_backoff_base = Duration::from_millis(20);
    c
}

#[test]
fn worker_survives_a_panicking_before_send() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let calls = Arc::new(AtomicU32::new(0));
    let seen = calls.clone();

    let mut config = config(&dir.path);
    config.before_send = Some(Box::new(move |_meta| {
        // Panic on the first report; succeed afterward.
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("boom inside a host before_send callback");
        }
        true
    }));

    let recorder = Recorder::launch(config, mock.clone()).unwrap();
    recorder.report(bugsee_core::reporting::manual_upload_meta(), None); // panics in before_send
    recorder.capture(log_entry());
    recorder.report(bugsee_core::reporting::manual_upload_meta(), None); // must still be delivered
    assert!(recorder.flush(Duration::from_secs(5)));

    assert!(calls.load(Ordering::SeqCst) >= 2, "before_send ran for both reports");
    assert_eq!(
        mock.uploaded_bundles.lock().unwrap().len(),
        1,
        "the worker survived the panic and delivered the second report"
    );
}

#[test]
fn transient_failures_are_retried_until_delivered() {
    let dir = TempDir::new();
    let flaky = Arc::new(FlakyTransport::new(2)); // fail twice, then succeed
    let recorder = Recorder::launch(config(&dir.path), flaky.clone()).unwrap();

    recorder.capture(log_entry());
    recorder.report(bugsee_core::reporting::manual_upload_meta(), None);
    assert!(recorder.flush(Duration::from_secs(5)));

    assert_eq!(*flaky.uploaded.lock().unwrap(), 1, "delivered after retries");
    assert!(queue::list_pending(&dir.path).is_empty(), "queue drained");
}

#[test]
fn queued_report_survives_restart_and_delivers_next_launch() {
    let dir = TempDir::new();

    // Session 1: delivery always fails → the bundle stays queued. Dropping the
    // recorder flushes the capture worker (enqueueing the report) on shutdown.
    {
        let recorder =
            Recorder::launch(config(&dir.path), Arc::new(AlwaysFailUpload)).unwrap();
        recorder.capture(log_entry());
        recorder.report(bugsee_core::reporting::manual_upload_meta(), None);
    } // clean drop enqueues then stops
    assert!(
        !queue::list_pending(&dir.path).is_empty(),
        "undelivered bundle persisted on disk"
    );

    // Session 2: a healthy transport delivers the queued bundle on launch.
    let good = Arc::new(MockTransport::default());
    let recorder2 = Recorder::launch(config(&dir.path), good.clone()).unwrap();
    assert!(recorder2.flush(Duration::from_secs(5)));

    assert_eq!(
        good.uploaded_bundles.lock().unwrap().len(),
        1,
        "queued bundle delivered on the next launch"
    );
    assert!(queue::list_pending(&dir.path).is_empty(), "queue drained after relaunch");
}

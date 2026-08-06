//
//  report_dropped.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Delivery failure has to be observable.
//!
//! The durable queue is drained on success AND on permanent failure — see
//! `runtime.rs`, "Duplicate / permanent — abandon the report" — so
//! `Bugsee::flush` returning `true` means "the queue is empty", not "the server
//! got it". Combined with an SDK that does no logging, a host had no way to
//! learn its reports were being rejected. That is not hypothetical: a
//! deployment rejected every Rust report with a validation error for days while
//! the SDK reported success, because the server answers `ok:false` with HTTP
//! 200 and nothing surfaced it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bugsee::core::transport::{Transport, TransportError};
use bugsee::{Bugsee, DropReason, LaunchOptions, LogLevel};

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bugsee-dropped-{}",
            bugsee::core::util::random_hex(8)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Rejects every issue the way a real deployment does: permanently, with the
/// server's own explanation attached.
struct RejectingTransport {
    message: String,
}

impl Transport for RejectingTransport {
    fn register_session(
        &self,
        _app_token: &str,
        _environment_json: &[u8],
    ) -> Result<String, TransportError> {
        Ok("token".into())
    }

    fn create_issue(
        &self,
        _app_token: &str,
        _access_token: Option<&str>,
        _request_json: &[u8],
    ) -> Result<String, TransportError> {
        Err(TransportError::Permanent(self.message.clone()))
    }

    fn upload_bundle(&self, _endpoint: &str, _zip: &[u8]) -> Result<(), TransportError> {
        Ok(())
    }
}

#[test]
fn a_rejected_report_notifies_the_host_with_the_server_reason() {
    let dir = TempDir::new();
    let dropped: Arc<Mutex<Vec<DropReason>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = dropped.clone();

    let transport = Arc::new(RejectingTransport {
        // The shape a real rejection takes; the text is the only account of why
        // the report vanished, so it must survive to the host.
        message: "server error 99006: Cast to Number failed for value \"NaN\"".to_string(),
    });

    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&dir.path)
            .with_transport(transport)
            .on_report_dropped(move |reason| sink.lock().unwrap().push(reason)),
    )
    .expect("launch");

    Bugsee::capture_message(LogLevel::Error, "this report will be rejected");

    // `true` here is precisely the trap: the queue drained because the report
    // was THROWN AWAY, not because it was delivered.
    let drained = Bugsee::flush(Duration::from_secs(5));

    let seen = dropped.lock().unwrap().clone();
    drop(guard);

    assert!(
        drained,
        "the queue drains either way — this assertion documents the trap"
    );
    assert_eq!(seen.len(), 1, "expected exactly one drop, got {seen:?}");

    match &seen[0] {
        DropReason::Rejected(detail) => {
            assert!(
                detail.contains("99006"),
                "the server's reason must reach the host: {detail}"
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

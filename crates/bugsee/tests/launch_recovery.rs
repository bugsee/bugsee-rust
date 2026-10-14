//
//  launch_recovery.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Issue #10 through the facade: the previous run's crash is on disk in the queue
//! by the time `launch_with` returns, even if the transport never delivers.

use std::path::PathBuf;
use std::sync::Arc;

use bugsee::core::transport::{Transport, TransportError};
use bugsee::{Bugsee, LaunchOptions};

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

#[test]
fn launch_with_queues_the_previous_crash_before_returning() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("bugsee-fl-{}", bugsee::core::util::random_hex(8)));
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // A crashed earlier session: marker naming no live owner, and its parts.
    std::fs::write(sessions.join("1.alive"), "0").unwrap();
    std::fs::create_dir_all(dir.join("parts").join("1")).unwrap();
    std::fs::write(dir.join("gen"), "1").unwrap();

    let guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir)
            .with_transport(Arc::new(Offline)),
    )
    .unwrap();
    let queued = std::fs::read_dir(dir.join("queue"))
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().to_string_lossy().ends_with(".bundle.zip"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(queued, 1, "no flush, no waiting");
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
}

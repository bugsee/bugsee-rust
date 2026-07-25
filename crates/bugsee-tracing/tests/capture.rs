//
//  capture.rs
//  bugsee-tracing
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! tracing events flow into Bugsee log entries.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};
use bugsee_tracing::BugseeLayer;
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-tracing-{}", bugsee::core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[test]
fn tracing_events_become_log_entries() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T").data_dir(&dir.path).with_transport(mock.clone()),
    )
    .unwrap();

    let subscriber = tracing_subscriber::registry()
        .with(BugseeLayer::new().with_min_level(tracing::Level::DEBUG));
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(target: "myapp", user = "alice", "user logged in");
        tracing::error!("boom happened");
        tracing::debug!("verbose detail");
        tracing::trace!("too verbose to capture");
    });

    Bugsee::upload();
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let log_name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".log.json"))
        .expect("log.json present");
    let mut bytes = Vec::new();
    zip.by_name(&log_name).unwrap().read_to_end(&mut bytes).unwrap();
    let log: Value = serde_json::from_slice(&bytes).unwrap();
    let events = log["events"].as_array().unwrap();

    let messages: Vec<&str> = events.iter().map(|e| e["message"].as_str().unwrap()).collect();
    assert!(messages.contains(&"user logged in"));
    assert!(messages.contains(&"boom happened"));
    assert!(messages.contains(&"verbose detail"));
    assert!(!messages.contains(&"too verbose to capture"), "TRACE below min level skipped");

    // The info event mapped to level 3 (Info), tag = target, field captured.
    let info = events.iter().find(|e| e["message"] == "user logged in").unwrap();
    assert_eq!(info["level"], 3);
    assert_eq!(info["tag"], "myapp");
    assert_eq!(info["user"], "alice", "structured field flattened onto the entry");

    // The error event mapped to level 1 (Error).
    let err = events.iter().find(|e| e["message"] == "boom happened").unwrap();
    assert_eq!(err["level"], 1);
}

//
//  capture.rs
//  bugsee-log
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! log records flow into Bugsee log entries.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-log-{}", bugsee::core::util::random_hex(8)));
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
fn log_records_become_log_entries() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T").data_dir(&dir.path).with_transport(mock.clone()),
    )
    .unwrap();

    // Install the global logger (this test binary is its own process).
    bugsee_log::init().unwrap();
    log::info!(target: "svc", "hello from log");
    log::error!("bad thing happened");
    log::trace!("too verbose"); // below Info, skipped

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
    assert!(messages.contains(&"hello from log"));
    assert!(messages.contains(&"bad thing happened"));
    assert!(!messages.contains(&"too verbose"));

    let info = events.iter().find(|e| e["message"] == "hello from log").unwrap();
    assert_eq!(info["level"], 3);
    assert_eq!(info["tag"], "svc");
}

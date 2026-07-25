//
//  sampling.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 5: sample_rate drops a fraction of non-fatal reports but never crashes.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-sample-{}", bugsee::core::util::random_hex(8)));
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
fn zero_sample_rate_drops_errors_but_keeps_manual_reports() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let options = LaunchOptions::new("T")
        .data_dir(&dir.path)
        .with_transport(mock.clone())
        .sample_rate(0.0); // drop all non-fatal errors
    let _guard = Bugsee::launch_with(options).unwrap();

    // Non-fatal error reports — all sampled out.
    Bugsee::capture_message(LogLevel::Error, "err one");
    Bugsee::capture_message(LogLevel::Error, "err two");
    Bugsee::capture_message(LogLevel::Error, "err three");

    // A manual upload is a `bug` issue, not sampled.
    Bugsee::upload();

    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "only the non-sampled manual report survived");

    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut rbytes = Vec::new();
    zip.by_name("request.json").unwrap().read_to_end(&mut rbytes).unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["type"], "bug", "the surviving report is the manual upload");
}

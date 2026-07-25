//
//  apm.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 4 APM: a transaction with a child span is captured and exported to
//! performance.json with the `{"transactions":[…]}` envelope.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, Status};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-apm-{}", bugsee::core::util::random_hex(8)));
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
fn transaction_with_span_exports_to_performance_json() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    let tx = Bugsee::start_transaction("MainScreen", "ui.load");
    let mut span = tx.start_span("db.query");
    span.set_description("SELECT * FROM users")
        .set_status(Status::Ok);
    span.finish();
    tx.set_status(Status::Ok);
    tx.finish();

    Bugsee::upload();
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();

    // Find the performance file.
    let perf_name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".performance.json"))
        .expect("performance.json present");

    let mut pbytes = Vec::new();
    zip.by_name(&perf_name)
        .unwrap()
        .read_to_end(&mut pbytes)
        .unwrap();
    let perf: Value = serde_json::from_slice(&pbytes).unwrap();

    // APM uses the transactions envelope, not the events envelope.
    assert!(perf.get("transactions").is_some(), "transactions envelope");
    assert!(perf.get("events").is_none(), "not the events envelope");

    let tx0 = &perf["transactions"][0];
    assert_eq!(tx0["name"], "MainScreen");
    assert_eq!(tx0["operation"], "ui.load");
    assert_eq!(tx0["status"], "OK");
    assert!(tx0["durationNanos"].as_i64().is_some());
    assert_eq!(tx0["spans"][0]["operation"], "db.query");
    assert_eq!(tx0["spans"][0]["description"], "SELECT * FROM users");
    assert_eq!(tx0["spans"][0]["finished"], true);

    // The manifest lists it with type `performance`.
    let mut mbytes = Vec::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_end(&mut mbytes)
        .unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    let types: Vec<&str> = man["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"performance"), "types: {types:?}");
}

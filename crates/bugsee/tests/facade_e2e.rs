//
//  facade_e2e.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! End-to-end through the public `Bugsee` facade with an injected mock
//! transport. The SDK is a process-global singleton, so this is a single
//! sequential test.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel, Severity};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bugsee-facade-{}",
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

#[test]
fn full_public_api_flow() {
    // Safe no-op before launch.
    assert!(!Bugsee::is_active());
    Bugsee::event("before_launch_is_ignored");

    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let options = LaunchOptions::new("APP_TOKEN_XYZ")
        .data_dir(&dir.path)
        .with_transport(mock.clone());

    let guard = Bugsee::launch_with(options).expect("launch");
    assert!(Bugsee::is_active());

    Bugsee::set_email("user@example.com");
    Bugsee::set_attribute("tier", "premium");
    Bugsee::event("app_started");
    Bugsee::log(LogLevel::Info, "hello from rust");
    Bugsee::trace("temperature", 21.5);

    // Deferred report: create → populate → upload.
    let mut report = Bugsee::create_report();
    report
        .set_summary("Manual checkpoint")
        .set_severity(Severity::Critical)
        .add_label("beta")
        .add_attachment(b"server-response-body".to_vec(), "resp.txt", "text/plain");
    report.upload();

    assert!(Bugsee::flush(Duration::from_secs(5)), "flush drained");

    // Pause suppresses capture.
    Bugsee::pause();
    assert!(!Bugsee::is_active());
    Bugsee::resume();

    // Inspect what the transport received.
    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "one report uploaded");
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).expect("valid zip");

    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with(".attachment.bgsfile")),
        "attachment present: {names:?}"
    );

    let mut rbytes = Vec::new();
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut rbytes)
        .unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["summary"], "Manual checkpoint");
    assert_eq!(req["severity"], 4, "Critical == 4");
    assert_eq!(req["labels"], serde_json::json!(["beta"]));
    assert_eq!(req["email"], "user@example.com");
    assert_eq!(req["source"]["type"], "code_upload");
    assert_eq!(
        req["environment"]["sdk"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    let mut mbytes = Vec::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_end(&mut mbytes)
        .unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    assert_eq!(man["attrs"]["tier"], "premium");
    let types: Vec<&str> = man["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"log"));
    // Manifest `type` is the shared base per contract (events.user → events,
    // traces.user → traces); the user/system split lives in the filename only.
    assert!(types.contains(&"events"));
    assert!(types.contains(&"traces"));
    assert!(
        !types.contains(&"events.user") && !types.contains(&"traces.user"),
        "compound manifest type must not leak: {types:?}"
    );
    assert!(types.contains(&"attachment"));

    drop(bundles);
    drop(guard);
    assert!(!Bugsee::is_active(), "guard drop stopped the SDK");
}

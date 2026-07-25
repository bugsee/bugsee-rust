//
//  runtime_e2e.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 1 end-to-end: launch the recorder, capture entries, trigger an upload,
//! and assert the transport received a valid, parseable report bundle.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::model::entry::{CaptureEntry, EventEntry, LogEntry};
use bugsee_core::model::enums::{LogLevel, LogSource};
use bugsee_core::reporting::manual_upload_meta;
use bugsee_core::util::epoch_ms;
use bugsee_core::{MockTransport, Recorder, RecorderConfig};
use serde_json::{Map, Value};
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-e2e-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn log(msg: &str) -> CaptureEntry {
    CaptureEntry::Log(LogEntry {
        timestamp: epoch_ms(),
        level: LogLevel::Info,
        source: LogSource::StdOut,
        tag: None,
        message: Some(msg.into()),
        custom: Map::new(),
    })
}

fn event(name: &str) -> CaptureEntry {
    CaptureEntry::UserEvent(EventEntry {
        timestamp: epoch_ms(),
        name: Some(name.into()),
        params: Map::new(),
        custom: Map::new(),
    })
}

#[test]
fn launch_capture_upload_produces_valid_bundle() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());

    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "APP_TOKEN_123"),
        transport.clone(),
    )
    .expect("launch");

    recorder.with_scope(|s| {
        s.set_email(Some("user@example.com".into()));
        s.set_attribute("userTier", Value::from("premium"));
    });

    recorder.capture(log("app started"));
    recorder.capture(event("checkout_started"));
    recorder.capture(log("checkout done"));

    recorder.report(manual_upload_meta(), None);
    assert!(recorder.flush(Duration::from_secs(5)), "flush completed");

    // The transport received exactly one created issue and one uploaded bundle.
    assert_eq!(transport.created_requests.lock().unwrap().len(), 1);
    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);

    // The bundle is a valid ZIP with the required documents.
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).expect("valid zip");
    let names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
    assert!(names.iter().any(|n| n == "manifest.json"));
    assert!(names.iter().any(|n| n == "request.json"));
    assert!(names.iter().any(|n| n == ".apptoken"));
    assert!(names.iter().any(|n| n.ends_with(".log.json")), "log capture file present: {names:?}");
    assert!(names.iter().any(|n| n.ends_with(".events.user.json")), "event capture file present: {names:?}");

    // request.json reflects the scope (email) and the code_upload trigger.
    let mut rbytes = Vec::new();
    zip.by_name("request.json").unwrap().read_to_end(&mut rbytes).unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["type"], "bug");
    assert_eq!(req["source"]["type"], "code_upload");
    assert_eq!(req["email"], "user@example.com");

    // manifest.json lists the capture files and carries the scope attribute.
    let mut mbytes = Vec::new();
    zip.by_name("manifest.json").unwrap().read_to_end(&mut mbytes).unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    assert_eq!(man["version"], 1);
    assert_eq!(man["attrs"]["userTier"], "premium");
    let types: Vec<&str> = man["files"].as_array().unwrap().iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"log"));
    assert!(types.contains(&"events.user"));

    // The .apptoken carries the raw token.
    let mut token = String::new();
    zip.by_name(".apptoken").unwrap().read_to_string(&mut token).unwrap();
    assert_eq!(token, "APP_TOKEN_123");
}

#[test]
fn capture_after_flush_still_uploads_second_report() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "T"), transport.clone()).unwrap();

    recorder.capture(log("one"));
    recorder.report(manual_upload_meta(), None);
    recorder.capture(log("two"));
    recorder.report(manual_upload_meta(), None);
    assert!(recorder.flush(Duration::from_secs(5)));

    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 2);
    // A single session registration is reused across both uploads.
    assert_eq!(*transport.sessions.lock().unwrap(), 1);
}

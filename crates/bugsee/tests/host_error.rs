//
//  host_error.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! `Bugsee::capture_host_error`: a binding reports an error caught in its host
//! language, with the host's frames and cause chain instead of a Rust backtrace.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::errors::Cause;
use bugsee::core::model::crash::{Frame, FrameData};
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
        path.push(format!("bugsee-host-{}", bugsee::core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn frame(function: &str, hidden: bool, line: i64) -> Frame {
    Frame {
        trace: format!("{function} (app.php:{line})"),
        hidden,
        data: FrameData {
            source: Some("app.php".into()),
            member_class: None,
            member: Some(function.into()),
            line,
        },
    }
}

#[test]
fn host_frames_and_causes_reach_the_bundle_without_rust_frames() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    Bugsee::capture_host_error(
        "RuntimeError",
        "boom",
        &[Cause {
            name: "IOError".into(),
            reason: "disk full".into(),
        }],
        vec![frame("vendor_call", true, 7), frame("run_job", false, 42)],
    );
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut bytes = Vec::new();
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(crash["handled"], true);
    let exc = &crash["exception"];
    assert_eq!(exc["name"], "RuntimeError");
    assert_eq!(exc["reason"], "boom");
    assert_eq!(exc["cause"]["name"], "IOError");
    assert_eq!(exc["cause"]["reason"], "disk full");

    let text = crash.to_string();
    assert!(text.contains("run_job"), "host frame present");
    assert!(
        text.contains("vendor_call"),
        "hidden host frame still recorded"
    );
    assert!(
        !text.contains("capture_host_error") && !text.contains("bugsee_ffi"),
        "no Rust call-site frames are attached: {text}"
    );
}

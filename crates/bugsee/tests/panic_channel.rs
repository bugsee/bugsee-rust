//
//  panic_channel.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 2 caught-panic path: a panic inside `bugsee::guard` is contained and
//! delivered as a handled panic report through the full pipeline.

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
        path.push(format!(
            "bugsee-panic-{}",
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
fn caught_panic_is_contained_and_reported() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    // A panic inside the guard must not propagate.
    let result = bugsee::guard(|| {
        let v: Vec<u8> = vec![1, 2, 3];
        // Deliberate out-of-bounds panic.
        let _ = v[10];
    });
    assert!(result.is_err(), "panic contained by guard");

    // The host is still alive and functional afterward.
    Bugsee::event("survived_the_panic");

    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "one panic report delivered");

    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut cbytes = Vec::new();
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut cbytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();

    assert_eq!(crash["exception"]["name"], "panic");
    assert_eq!(crash["handled"], true, "caught panic is handled");
    assert_eq!(crash["ndkCrash"], false);
    let reason = crash["exception"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("index out of bounds") || reason.contains("10"),
        "panic reason captured: {reason}"
    );

    let mut rbytes = Vec::new();
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut rbytes)
        .unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(
        req["type"], "error",
        "caught panic is a non-fatal error issue"
    );
    assert_eq!(req["signatures"].as_array().unwrap().len(), 1);
}

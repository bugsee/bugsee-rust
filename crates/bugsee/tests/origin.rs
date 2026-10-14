//
//  origin.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! `source.origin` through the public `Bugsee::upload()` path: it names the
//! calling function, not an SDK frame. One sequential test (global singleton).

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
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[inline(never)]
fn report_from_a_named_call_site() {
    Bugsee::upload();
}

#[test]
fn upload_origin_names_the_calling_function() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "bugsee-origin-{}",
        bugsee::core::util::random_hex(8)
    ));
    std::fs::create_dir_all(&path).unwrap();
    let dir = TempDir { path };

    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    report_from_a_named_call_site();
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut bytes = Vec::new();
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let req: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(req["source"]["type"], "code_upload");
    let origin = req["source"]["origin"].as_str().expect("origin populated");
    assert!(origin.contains("report_from_a_named_call_site"), "{origin}");
}

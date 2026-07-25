//
//  before_send.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 5: the before_send hook can mutate or drop reports before delivery.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::reporting::manual_upload_meta;
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, Severity};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-bs-{}", bugsee::core::util::random_hex(8)));
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
fn before_send_mutates_and_drops_reports() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let options = LaunchOptions::new("T")
        .data_dir(&dir.path)
        .with_transport(mock.clone())
        .before_send(|meta| {
            // Drop anything flagged secret.
            if meta.summary.as_deref() == Some("drop-me") {
                return false;
            }
            // Otherwise bump severity and tag it.
            meta.severity = Severity::Critical;
            meta.attrs.insert("scrubbed".into(), Value::from(true));
            true
        });
    let _guard = Bugsee::launch_with(options).unwrap();

    let mut keep = manual_upload_meta();
    keep.summary = Some("keep-me".into());
    Bugsee::upload_with(keep);

    let mut drop = manual_upload_meta();
    drop.summary = Some("drop-me".into());
    Bugsee::upload_with(drop);

    assert!(Bugsee::flush(Duration::from_secs(5)));

    // Only the non-dropped report was delivered.
    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "the drop-me report was suppressed");

    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut rbytes = Vec::new();
    zip.by_name("request.json").unwrap().read_to_end(&mut rbytes).unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["summary"], "keep-me");
    assert_eq!(req["severity"], 4, "before_send bumped to Critical");

    let mut mbytes = Vec::new();
    zip.by_name("manifest.json").unwrap().read_to_end(&mut mbytes).unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    assert_eq!(man["attrs"]["scrubbed"], true, "before_send mutation applied");
}

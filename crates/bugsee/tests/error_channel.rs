//
//  error_channel.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 2 handled-error channel: capture_error (with source chain),
//! capture_message, and the ResultExt `.capture()` ergonomic.

use std::error::Error;
use std::fmt;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel, ResultExt};
use serde_json::Value;
use zip::ZipArchive;

#[derive(Debug)]
struct InnerError;
impl fmt::Display for InnerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "inner boom")
    }
}
impl Error for InnerError {}

#[derive(Debug)]
struct CheckoutError {
    inner: InnerError,
}
impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "checkout failed")
    }
}
impl Error for CheckoutError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.inner)
    }
}

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-err-{}", bugsee::core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn crash_json_in(bundle: &[u8]) -> Value {
    let mut zip = ZipArchive::new(Cursor::new(bundle.to_vec())).unwrap();
    let mut bytes = Vec::new();
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn request_json_in(bundle: &[u8]) -> Value {
    let mut zip = ZipArchive::new(Cursor::new(bundle.to_vec())).unwrap();
    let mut bytes = Vec::new();
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn capture_error_message_and_result_ext() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    // 1) capture_error with a source chain.
    Bugsee::capture_error(&CheckoutError { inner: InnerError });

    // 2) capture_message.
    Bugsee::capture_message(LogLevel::Error, "something looked off");

    // 3) ResultExt::capture on an Err.
    let result: Result<(), CheckoutError> = Err(CheckoutError { inner: InnerError });
    let _ = result.capture();

    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 3, "three error reports delivered");

    // The first is the capture_error report: error issue + crash.json chain.
    let req = request_json_in(&bundles[0]);
    assert_eq!(req["type"], "error");
    assert_eq!(req["source"]["type"], "error");
    assert_eq!(req["signatures"].as_array().unwrap().len(), 1);

    let crash = crash_json_in(&bundles[0]);
    assert_eq!(crash["handled"], true);
    assert_eq!(crash["ndkCrash"], false);
    assert_eq!(crash["exception_type"], "error");
    assert_eq!(
        crash["exception"]["name"], "CheckoutError",
        "short type name"
    );
    assert_eq!(crash["exception"]["reason"], "checkout failed");
    assert_eq!(
        crash["exception"]["cause"]["reason"], "inner boom",
        "source() chain captured"
    );
    // Signature is shared between request.json and crash.json.
    assert_eq!(req["signatures"][0], crash["signatures"][0]);

    // The message report.
    let msg_crash = crash_json_in(&bundles[1]);
    assert_eq!(msg_crash["exception"]["name"], "Message");
    assert_eq!(msg_crash["exception"]["reason"], "something looked off");
}

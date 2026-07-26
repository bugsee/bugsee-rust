//
//  error_chain_cap.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! F5 regression: capture_error must cap its source() walk so a very deep (or
//! cyclic) error chain can't hang the caller or overflow the recursive
//! crash.json serialization. Its own integration binary — the `Bugsee` facade is
//! a process-global singleton, so this must not share a process with other
//! facade-launching tests.

use std::error::Error;
use std::fmt;
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
            "bugsee-chaincap-{}",
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

#[derive(Debug)]
struct Deep {
    next: Option<Box<Deep>>,
}
impl fmt::Display for Deep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "deep")
    }
}
impl Error for Deep {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.next.as_deref().map(|e| e as &(dyn Error + 'static))
    }
}

#[test]
fn capture_error_caps_deep_source_chain() {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone()),
    )
    .unwrap();

    // A 1000-link source() chain: without a cap the walk folds 1000 recursively
    // nested `cause` boxes into crash.json (and a real cycle would loop forever).
    // The cap bounds both (F5).
    let mut head = Deep { next: None };
    for _ in 0..1000 {
        head = Deep {
            next: Some(Box::new(head)),
        };
    }
    Bugsee::capture_error(&head);
    assert!(Bugsee::flush(Duration::from_secs(5)));

    let bundles = mock.uploaded_bundles.lock().unwrap();
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut bytes = Vec::new();
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&bytes).unwrap();

    // Count the nested `cause` depth — it must be capped well below 1000.
    let mut depth = 0;
    let mut node = &crash["exception"]["cause"];
    while node.is_object() {
        depth += 1;
        node = &node["cause"];
    }
    assert!(
        depth > 0 && depth <= 32,
        "source chain must be capped at 32, got {depth}"
    );
}

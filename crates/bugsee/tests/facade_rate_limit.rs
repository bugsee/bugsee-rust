//
//  facade_rate_limit.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! A handled-error storm is rate limited by default through the facade; `disable_rate_limiting` lifts it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-fac-{}", bugsee::core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn storm(options: LaunchOptions, n: usize) -> usize {
    let dir = TempDir::new();
    let mock = Arc::new(MockTransport::default());
    let guard =
        Bugsee::launch_with(options.data_dir(&dir.path).with_transport(mock.clone())).unwrap();
    for _ in 0..n {
        Bugsee::capture_message(LogLevel::Error, "the same failure, again");
    }
    assert!(Bugsee::flush(Duration::from_secs(30)));
    drop(guard);
    let n = mock.uploaded_bundles.lock().unwrap().len();
    n
}

#[test]
fn storm_is_bounded_by_default_and_unbounded_when_disabled() {
    let limited = storm(LaunchOptions::new("T"), 40);
    assert!((1..=7).contains(&limited), "delivered {limited}");
    let unlimited = storm(LaunchOptions::new("T").disable_rate_limiting(), 40);
    assert_eq!(unlimited, 40);
}

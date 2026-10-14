//
//  facade_scrub.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The default scrubber is on through the public facade, and `disable_scrubbing` turns it off.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use zip::ZipArchive;

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

fn all_text(zip: &[u8]) -> String {
    let mut z = ZipArchive::new(Cursor::new(zip.to_vec())).unwrap();
    let mut out = String::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        if f.name() == ".apptoken" {
            continue;
        }
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.push_str(&String::from_utf8_lossy(&b));
    }
    out
}

#[test]
fn default_scrubs_and_opt_out_does_not() {
    for (disable, expect_leak) in [(false, false), (true, true)] {
        let dir = TempDir::new();
        let mock = Arc::new(MockTransport::default());
        let mut options = LaunchOptions::new("T")
            .data_dir(&dir.path)
            .with_transport(mock.clone());
        if disable {
            options = options.disable_scrubbing();
        }
        let guard = Bugsee::launch_with(options).unwrap();
        Bugsee::capture_message(
            LogLevel::Error,
            "login failed for carol@corp-example.com password=hunter2 in /home/carol/app",
        );
        assert!(Bugsee::flush(Duration::from_secs(5)));
        drop(guard);

        let bundles = mock.uploaded_bundles.lock().unwrap();
        assert_eq!(bundles.len(), 1);
        let text = all_text(&bundles[0]);
        for secret in ["carol@corp-example.com", "hunter2", "/home/carol/"] {
            assert_eq!(
                text.contains(secret),
                expect_leak,
                "{secret} (disable={disable})"
            );
        }
    }
}

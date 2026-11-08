//
//  shutdown_windows.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The Windows end of the shutdown contract (`shutdown.rs` is unix-only).
//!
//! `LaunchOptions::flush_at_exit` is a no-op on Windows by design:
//! `std::process::exit` calls `ExitProcess` directly and CRT `atexit` handlers
//! never run, so a plain `exit` stays an abnormal exit even with the option set,
//! and the queued report is recovered on the next launch. A host that wants a
//! clean end calls `shutdown` (or drops the launch guard).

#![cfg(windows)]

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bugsee::core::transport::{Transport, TransportError};
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::Value;
use zip::ZipArchive;

const KIND_ENV: &str = "BUGSEE_SHUTDOWN_WIN_KIND";
const DIR_ENV: &str = "BUGSEE_SHUTDOWN_WIN_DIR";
const PENDING: &str = "pending-report-marker";
const ABNORMAL: &str = "terminated abnormally";

/// An unreachable backend: every report stays queued on disk.
struct Unreachable;

impl Transport for Unreachable {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("token".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("backend unreachable".into()))
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Transient("backend unreachable".into()))
    }
}

/// Not a test by itself: the body of the child. A plain run does nothing.
#[test]
fn child_entry() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap());
    let mut options = LaunchOptions::new("APP_TOKEN")
        .data_dir(dir.join("data"))
        .native_crash_capture(false)
        .with_transport(Arc::new(Unreachable));
    if kind == "exit_flush" {
        options = options.flush_at_exit(Duration::from_secs(2));
    }
    let guard = Bugsee::launch_with(options).expect("launch");
    Bugsee::capture_message(LogLevel::Error, PENDING);
    // Durably queued before the process ends, as in the unix suite.
    Bugsee::flush(Duration::from_secs(1));
    match kind.as_str() {
        "exit_plain" | "exit_flush" => std::process::exit(0),
        "return" => {
            drop(guard);
            std::process::exit(0);
        }
        other => panic!("unknown kind {other}"),
    }
}

fn crash_json(zip: &[u8]) -> Value {
    let mut archive = ZipArchive::new(Cursor::new(zip)).expect("bundle is a zip");
    let mut entry = archive.by_name("crash.json").expect("crash.json present");
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

/// Launch over the child's data directory and collect what recovery delivers.
fn relaunch(dir: &Path) -> Vec<Value> {
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir.join("data"))
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");
    assert!(Bugsee::flush(Duration::from_secs(10)), "queue drains");
    let out = mock
        .uploaded_bundles
        .lock()
        .unwrap()
        .iter()
        .map(|b| crash_json(b))
        .collect();
    drop(guard);
    out
}

fn count(reports: &[Value], needle: &str) -> usize {
    reports
        .iter()
        .filter(|r| r.to_string().contains(needle))
        .count()
}

/// Run the child to its end and return `(exit code, pending, abnormal)` over
/// what the following launch recovers.
fn run_case(kind: &str) -> (Option<i32>, usize, usize) {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-shutdown-win-{kind}-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env(KIND_ENV, kind)
        .env(DIR_ENV, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            panic!("[{kind}] child did not end");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let recovered = relaunch(&dir);
    let again = relaunch(&dir);
    assert!(
        again.is_empty(),
        "[{kind}] reported again on the following launch: {again:?}"
    );
    let counts = (
        status.code(),
        count(&recovered, PENDING),
        count(&recovered, ABNORMAL),
    );
    let _ = std::fs::remove_dir_all(&dir);
    counts
}

#[test]
fn returning_from_main_is_a_clean_stop() {
    let (code, pending, abnormal) = run_case("return");
    assert_eq!(code, Some(0));
    assert_eq!(pending, 1, "the queued report is recovered once");
    assert_eq!(abnormal, 0, "a deliberate stop is not a crash");
}

#[test]
fn flush_at_exit_is_a_no_op_so_exit_stays_an_abnormal_exit() {
    // Without the option and with it, `exit` ends the same way: the exit is
    // reported as abnormal and the queued report is still not lost.
    for kind in ["exit_plain", "exit_flush"] {
        let (code, pending, abnormal) = run_case(kind);
        assert_eq!(code, Some(0), "[{kind}]");
        assert_eq!(pending, 1, "[{kind}]");
        assert_eq!(abnormal, 1, "[{kind}]");
    }
}

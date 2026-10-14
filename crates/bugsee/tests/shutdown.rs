//
//  shutdown.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The shutdown contract, per exit path (see `Bugsee::stop`): what is delivered
//! by the dying process, what is recovered on the next launch, and that nothing
//! is reported twice or lost.
//!
//! Each case runs a real child process (this same test binary, re-entered
//! through `child_entry`), ends it the way the case says, then relaunches the SDK
//! in this process over the same data directory.

#![cfg(unix)]

use std::io::{Cursor, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee::core::transport::{Transport, TransportError};
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::Value;
use zip::ZipArchive;

const KIND_ENV: &str = "BUGSEE_SHUTDOWN_KIND";
const DIR_ENV: &str = "BUGSEE_SHUTDOWN_DIR";
const FAIL_ENV: &str = "BUGSEE_SHUTDOWN_FAIL_UPLOADS";
const PENDING: &str = "pending-report-marker";

static SERIAL: Mutex<()> = Mutex::new(());

/// Writes every uploaded bundle to a file so the parent can see what a child
/// delivered; optionally fails every upload (an unreachable backend).
struct FileTransport {
    dir: PathBuf,
    fail: bool,
    n: AtomicUsize,
}

impl Transport for FileTransport {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("token".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        if self.fail {
            return Err(TransportError::Transient("backend unreachable".into()));
        }
        Ok("https://uploads.example/x".into())
    }
    fn upload_bundle(&self, _: &str, zip: &[u8]) -> Result<(), TransportError> {
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        std::fs::write(
            self.dir
                .join("out")
                .join(format!("{}-{n}.zip", std::process::id())),
            zip,
        )
        .map_err(|e| TransportError::Transient(e.to_string()))
    }
}

fn crash_json(zip: &[u8]) -> Value {
    let mut archive = ZipArchive::new(Cursor::new(zip)).expect("bundle is a zip");
    let mut entry = archive.by_name("crash.json").expect("crash.json present");
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn delivered_by_child(dir: &Path) -> Vec<Value> {
    std::fs::read_dir(dir.join("out"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| crash_json(&std::fs::read(e.path()).unwrap()))
        .collect()
}

// ---------------------------------------------------------------------------
// The child.
// ---------------------------------------------------------------------------

static HOST_HANDLER_RAN: AtomicBool = AtomicBool::new(false);

extern "C" fn host_handler(_: libc::c_int) {
    HOST_HANDLER_RAN.store(true, Ordering::SeqCst);
}

fn install_host_handler(sig: libc::c_int) {
    // SAFETY: a zeroed sigaction filled in with a plain handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = host_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        assert_eq!(libc::sigaction(sig, &sa, std::ptr::null_mut()), 0);
    }
}

/// Not a test by itself: the body of every child. A plain run does nothing.
#[test]
fn child_entry() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap());
    let fail = std::env::var(FAIL_ENV).is_ok();
    let ready = || std::fs::write(dir.join("ready"), b"1").unwrap();

    if kind == "sigterm_host_handler" {
        install_host_handler(libc::SIGTERM);
    }
    let mut options = LaunchOptions::new("APP_TOKEN")
        .data_dir(dir.join("data"))
        .native_crash_capture(false)
        .with_transport(Arc::new(FileTransport {
            dir: dir.clone(),
            fail,
            n: AtomicUsize::new(0),
        }));
    if kind == "exit_flush" {
        options = options.flush_at_exit(Duration::from_secs(2));
    }
    let guard = Bugsee::launch_with(options).expect("launch");
    if kind.starts_with("sigterm") && kind != "sigterm_no_hook" {
        Bugsee::install_termination_flush(Duration::from_secs(2)).expect("install");
        // Idempotent.
        Bugsee::install_termination_flush(Duration::from_secs(2)).expect("install twice");
    }
    Bugsee::capture_message(LogLevel::Error, PENDING);

    match kind.as_str() {
        // The signal must be what persists the report: no flush beforehand.
        "sigterm" | "sigterm_host_handler" => {
            ready();
            if kind == "sigterm" {
                std::thread::sleep(Duration::from_secs(60));
                std::process::exit(3); // the signal should have ended us
            }
            let start = Instant::now();
            while !HOST_HANDLER_RAN.load(Ordering::SeqCst) {
                assert!(start.elapsed() < Duration::from_secs(60), "no signal");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(dir.join("host-handler-ran"), b"1").unwrap();
            // The host decides to exit: a normal return path.
            drop(guard);
            std::process::exit(0);
        }
        // Reports are durably queued before the process ends the given way.
        "sigterm_no_hook" | "kill" | "exit_plain" | "exit_flush" | "return" => {
            Bugsee::flush(Duration::from_secs(1));
            ready();
            match kind.as_str() {
                "exit_plain" | "exit_flush" => std::process::exit(0),
                "return" => {
                    drop(guard);
                    std::process::exit(0);
                }
                _ => std::thread::sleep(Duration::from_secs(60)),
            }
        }
        other => panic!("unknown kind {other}"),
    }
}

// ---------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------

struct Outcome {
    status: std::process::ExitStatus,
    stderr: String,
    dir: PathBuf,
    child_delivered: Vec<Value>,
}

fn run_child(kind: &str, fail_uploads: bool, signal: Option<libc::c_int>) -> Outcome {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-shutdown-{kind}-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(dir.join("out")).unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env(KIND_ENV, kind)
        .env(DIR_ENV, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if fail_uploads {
        cmd.env(FAIL_ENV, "1");
    }
    let mut child = cmd.spawn().expect("spawn");

    let start = Instant::now();
    while !dir.join("ready").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child died before it was ready"
        );
        assert!(start.elapsed() < Duration::from_secs(30), "child not ready");
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(sig) = signal {
        // SAFETY: signalling a child this test spawned.
        unsafe { libc::kill(child.id() as libc::pid_t, sig) };
    }
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if start.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("child did not end after the signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let child_delivered = delivered_by_child(&dir);
    Outcome {
        status,
        stderr,
        dir,
        child_delivered,
    }
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
    std::thread::sleep(Duration::from_millis(1500));
    Bugsee::flush(Duration::from_secs(10));
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

const ABNORMAL: &str = "terminated abnormally";

/// Run one case and return `(pending, abnormal)` counts over everything
/// delivered by the child and by the first relaunch, after checking that a
/// second relaunch delivers nothing more.
fn run_case(
    kind: &str,
    fail_uploads: bool,
    signal: Option<libc::c_int>,
) -> (Outcome, usize, usize) {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = run_child(kind, fail_uploads, signal);
    let recovered = relaunch(&out.dir);
    let again = relaunch(&out.dir);
    assert!(
        again.is_empty(),
        "[{kind}] reported again on the following launch: {again:?}"
    );
    let mut all = out.child_delivered.clone();
    all.extend(recovered);
    let pending = count(&all, PENDING);
    let abnormal = count(&all, ABNORMAL);
    (out, pending, abnormal)
}

#[test]
fn returning_from_main_is_a_clean_stop() {
    let (out, pending, abnormal) = run_case("return", true, None);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1, "the queued report is delivered exactly once");
    assert_eq!(abnormal, 0, "a deliberate stop is not a crash");
}

#[test]
fn exit_without_stop_is_an_abnormal_exit_unless_opted_in() {
    // Default: nothing in the process can tell `exit` from a death, so it is
    // reported as abnormal. The queued report is still not lost.
    let (out, pending, abnormal) = run_case("exit_plain", true, None);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);

    // `flush_at_exit`: the same exit is deliberate.
    let (out, pending, abnormal) = run_case("exit_flush", true, None);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn sigkill_loses_neither_the_queue_nor_the_abnormal_exit() {
    let (out, pending, abnormal) = run_case("kill", true, Some(libc::SIGKILL));
    assert_eq!(out.status.signal(), Some(libc::SIGKILL));
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);
}

#[test]
fn sigterm_without_the_hook_is_untouched() {
    let (out, pending, abnormal) = run_case("sigterm_no_hook", true, Some(libc::SIGTERM));
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);
}

#[test]
fn sigterm_with_the_hook_flushes_then_dies_of_the_signal() {
    // The backend is unreachable, so the report can only survive in the queue,
    // and only if the signal made the SDK persist it before the process ended.
    let (out, pending, abnormal) = run_case("sigterm", true, Some(libc::SIGTERM));
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGTERM),
        "the process must still die of the signal, not exit: {:?} {}",
        out.status,
        out.stderr
    );
    assert_eq!(pending, 1, "delivered or queued, exactly once");
    assert_eq!(abnormal, 0, "a handled SIGTERM is a deliberate stop");
}

#[test]
fn sigterm_with_the_hook_delivers_when_the_backend_is_up() {
    let (out, pending, abnormal) = run_case("sigterm", false, Some(libc::SIGTERM));
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert_eq!(
        count(&out.child_delivered, PENDING),
        1,
        "delivered pre-exit"
    );
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn the_hosts_own_sigterm_handler_still_runs_and_decides() {
    let (out, pending, abnormal) = run_case("sigterm_host_handler", true, Some(libc::SIGTERM));
    assert!(
        out.dir.join("host-handler-ran").exists(),
        "the previous handler was swallowed: {}",
        out.stderr
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "the host chose to exit normally; the SDK must not kill it: {:?} {}",
        out.status,
        out.stderr
    );
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn shutdown_is_idempotent() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!(
        "bugsee-shutdown-idem-{}",
        bugsee::core::util::random_hex(6)
    ));
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&dir)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("launch");
    Bugsee::capture_message(LogLevel::Error, PENDING);
    assert!(Bugsee::shutdown(Duration::from_secs(10)), "drained");
    assert!(!Bugsee::is_launched());
    // Everything after the first call is a no-op, including the guard's Drop.
    assert!(!Bugsee::shutdown(Duration::from_secs(1)));
    Bugsee::stop();
    drop(guard);
    assert_eq!(mock.uploaded_bundles.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

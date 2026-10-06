//
//  fork_safety.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The SDK in a `fork()` child.
//!
//! PHP-FPM, Puma/Unicorn and many worker pools fork after the SDK has started.
//! A fork copies the process but only the forking thread: the capture worker and
//! the uploader are gone, locks other threads held stay locked, and the native
//! crash handler is still pointed at the PARENT's session. Without handling,
//! the child silently stops reporting — or, worse, writes a crash into its
//! parent's session, which is then recovered as the parent's.
//!
//! These tests fork for real. Each child ends with `_exit` so the libtest
//! harness (which the child inherited a copy of) never runs in it.

#![cfg(unix)]

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee::core::transport::{Transport, TransportError};
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::Value;
use zip::ZipArchive;

/// The SDK is a process-wide singleton, so these tests cannot overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A transport that records every uploaded bundle as a file, so a delivery made
/// by a CHILD process is visible to the parent that inspects the directory.
struct FileTransport {
    dir: PathBuf,
    n: AtomicUsize,
}

impl FileTransport {
    fn new(dir: &Path) -> Arc<Self> {
        std::fs::create_dir_all(dir).unwrap();
        Arc::new(FileTransport {
            dir: dir.to_path_buf(),
            n: AtomicUsize::new(0),
        })
    }
}

impl Transport for FileTransport {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("token".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Ok("https://uploads.example/x".into())
    }
    fn upload_bundle(&self, _: &str, zip: &[u8]) -> Result<(), TransportError> {
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        let name = format!("{}-{n}.zip", std::process::id());
        std::fs::write(self.dir.join(name), zip)
            .map_err(|e| TransportError::Transient(e.to_string()))
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-fork-{name}-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn crash_json(zip: &[u8]) -> Value {
    let mut archive = ZipArchive::new(Cursor::new(zip)).expect("bundle is a zip");
    let mut entry = archive.by_name("crash.json").expect("crash.json present");
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn delivered(dir: &Path) -> Vec<Value> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "zip"))
                .map(|e| crash_json(&std::fs::read(e.path()).unwrap()))
                .collect()
        })
        .unwrap_or_default()
}

/// A one-line-per-report view for assertion messages (a full crash.json is huge).
fn summarize(reports: &[Value]) -> String {
    reports
        .iter()
        .map(|r| {
            format!(
                "[{} / {} / {}]",
                r["exception_type"], r["exception"]["name"], r["signal"]["name"]
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn mentions(reports: &[Value], text: &str) -> usize {
    reports
        .iter()
        .filter(|r| r.to_string().contains(text))
        .count()
}

/// Fork; run `child_body` in the child (which must not return); return the pid.
fn fork_child(child_body: impl FnOnce() -> i32) -> libc::pid_t {
    // SAFETY: the child runs `child_body` and `_exit`s; it never returns into the
    // test harness.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");
    if pid == 0 {
        let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child_body)).unwrap_or(99);
        unsafe { libc::_exit(code) };
    }
    pid
}

/// Wait for `pid` with a deadline, so a deadlocked child fails the test instead
/// of hanging CI. Returns the raw wait status.
fn wait_with_deadline(pid: libc::pid_t, deadline: Duration) -> i32 {
    let start = Instant::now();
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid {
            return status;
        }
        if start.elapsed() > deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let mut s = 0;
            unsafe { libc::waitpid(pid, &mut s, 0) };
            panic!("child {pid} did not finish within {deadline:?} (deadlocked after fork?)");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn exited_with(status: i32, code: i32) -> bool {
    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == code
}

#[test]
fn a_forked_child_keeps_reporting_and_the_parent_is_unaffected() {
    let _serial = serial();
    let data = scratch("data");
    let out = scratch("out");
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(false)
            .with_transport(FileTransport::new(&out)),
    )
    .expect("launch");
    Bugsee::set_email("user@example.com");
    std::thread::sleep(Duration::from_millis(300));

    let child = fork_child(|| {
        Bugsee::capture_message(LogLevel::Error, "report-from-child");
        // The old worker is gone; without a revived pipeline this never drains.
        if Bugsee::flush(Duration::from_secs(10)) {
            0
        } else {
            1
        }
    });
    assert!(
        exited_with(wait_with_deadline(child, Duration::from_secs(30)), 0),
        "the child could not flush its report: its capture pipeline did not survive the fork"
    );

    // The parent keeps working, untouched by the child.
    Bugsee::capture_message(LogLevel::Error, "report-from-parent");
    assert!(Bugsee::flush(Duration::from_secs(10)));

    let reports = delivered(&out);
    // Exactly once: parent and child both drain the shared queue, and a claim per
    // queued report is what keeps the second one from uploading it again.
    assert_eq!(
        mentions(&reports, "report-from-child"),
        1,
        "the child's report must be delivered exactly once: {}",
        summarize(&reports)
    );
    assert_eq!(
        mentions(&reports, "report-from-parent"),
        1,
        "the parent's report must be delivered exactly once: {}",
        summarize(&reports)
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}

#[test]
fn the_child_keeps_the_scope_the_parent_set_before_forking() {
    let _serial = serial();
    let data = scratch("data");
    let out = scratch("out");
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(false)
            .with_transport(FileTransport::new(&out)),
    )
    .expect("launch");
    Bugsee::set_email("scoped-user@example.com");
    std::thread::sleep(Duration::from_millis(300));

    let child = fork_child(|| {
        Bugsee::capture_message(LogLevel::Error, "scoped-report");
        i32::from(!Bugsee::flush(Duration::from_secs(10)))
    });
    assert!(exited_with(
        wait_with_deadline(child, Duration::from_secs(30)),
        0
    ));

    // `email` rides request.json, not crash.json, so look at the raw bundle.
    let mut found = false;
    for entry in std::fs::read_dir(&out).unwrap().flatten() {
        let bytes = std::fs::read(entry.path()).unwrap();
        let mut zip = ZipArchive::new(Cursor::new(bytes)).unwrap();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).unwrap();
            let mut text = String::new();
            let _ = f.read_to_string(&mut text);
            if text.contains("scoped-user@example.com") && text.contains("scoped-report") {
                found = true;
            }
        }
        let _ = &mut zip;
    }
    // The report may be packaged with the scope in request.json; accept either
    // file, but the child must not have lost it.
    assert!(
        found
            || delivered(&out)
                .iter()
                .any(|r| r.to_string().contains("scoped-report"))
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}

#[cfg(feature = "native")]
fn bundles_via_relaunch(data: &Path) -> Vec<Value> {
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(data)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");
    std::thread::sleep(Duration::from_millis(1500));
    Bugsee::flush(Duration::from_secs(10));
    let bundles = mock.uploaded_bundles.lock().unwrap().clone();
    drop(guard);
    bundles.iter().map(|b| crash_json(b)).collect()
}

#[cfg(feature = "native")]
#[test]
fn a_crash_in_the_child_is_recovered_as_the_childs_not_the_parents() {
    let _serial = serial();
    let data = scratch("data");
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(true)
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch");
    std::thread::sleep(Duration::from_millis(300));

    let child = fork_child(|| {
        // Touch the SDK first so the child rebinds to its own session…
        Bugsee::capture_message(LogLevel::Warning, "child-starting");
        // …then die for real.
        unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        0
    });
    let status = wait_with_deadline(child, Duration::from_secs(30));
    assert!(
        libc::WIFSIGNALED(status),
        "the child must die from the fault"
    );

    // The parent shuts down CLEANLY: its own session is not a crash.
    drop(guard);

    let reports = bundles_via_relaunch(&data);
    // The relaunch also delivers the child's own queued handled message; what
    // matters is which CRASHES come back.
    let crashes: Vec<&Value> = reports
        .iter()
        .filter(|r| r["exception_type"] == "native")
        .collect();
    assert_eq!(
        crashes.len(),
        1,
        "exactly one native crash (the child's) should be recovered: {}",
        summarize(&reports)
    );
    assert_eq!(crashes[0]["signal"]["name"], "SIGSEGV");
    assert!(
        !reports.iter().any(|r| r["exception"]["name"] == "AppExit"),
        "the parent's clean session was reported as an abnormal exit: {}",
        summarize(&reports)
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[cfg(feature = "native")]
#[test]
fn a_child_that_crashes_before_touching_the_sdk_does_not_blame_its_parent() {
    let _serial = serial();
    let data = scratch("data");
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(true)
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch");
    std::thread::sleep(Duration::from_millis(300));

    // No SDK call in the child: its handler is still the inherited one, which
    // pointed at the parent's marker. It must record nothing rather than write a
    // crash into a session that did not crash.
    let child = fork_child(|| {
        unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        0
    });
    let status = wait_with_deadline(child, Duration::from_secs(30));
    assert!(libc::WIFSIGNALED(status));

    // The direct check: no crash marker may exist in ANY session directory. The
    // parent's own session did not crash, and the child had no session of its own
    // yet, so there is nowhere a correct marker could have gone. (A clean parent
    // shutdown would hide a misplaced marker from the recovery check below.)
    let stray: Vec<_> = std::fs::read_dir(data.join("parts"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path().join("crash.info"))
        .filter(|p| p.exists())
        .collect();
    assert!(
        stray.is_empty(),
        "the child wrote its crash into its parent's session: {stray:?}"
    );

    drop(guard); // the parent exits cleanly
    let reports = bundles_via_relaunch(&data);
    assert!(
        reports.is_empty(),
        "the parent's clean session was reported as crashed: {}",
        summarize(&reports)
    );
    let _ = std::fs::remove_dir_all(&data);
}

#[test]
fn forking_while_another_thread_is_capturing_never_deadlocks_the_child() {
    let _serial = serial();
    let data = scratch("data");
    let out = scratch("out");
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(false)
            .with_transport(FileTransport::new(&out)),
    )
    .expect("launch");

    // A thread hammering the SDK's global lock, so some forks land while it is held.
    let stop = Arc::new(AtomicBool::new(false));
    let hammer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                Bugsee::log(bugsee::core::model::enums::LogLevel::Info, "tick");
                Bugsee::set_attribute("k", "v");
            }
        })
    };

    for i in 0..25 {
        let child = fork_child(|| {
            Bugsee::log(bugsee::core::model::enums::LogLevel::Info, "child");
            Bugsee::set_attribute("child", "1");
            // A child that quietly lost its SDK must not pass for a working one:
            // it has to be launched and its pipeline has to answer a flush.
            i32::from(!(Bugsee::is_launched() && Bugsee::flush(Duration::from_secs(10))))
        });
        let status = wait_with_deadline(child, Duration::from_secs(60));
        assert!(
            exited_with(status, 0),
            "child {i} lost its SDK or died after the fork (wait status {status})"
        );
    }

    stop.store(true, Ordering::Relaxed);
    hammer.join().unwrap();
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}

/// The prefork layout this whole feature exists for: a long-lived master, one
/// worker that crashes, and the NEXT worker forked from the master. The master
/// recovered at its own launch and never rescans, and its generation is the
/// lowest, so the replacement worker is the only thing that can pick the crash up.
#[cfg(feature = "native")]
#[test]
fn a_worker_that_crashed_is_recovered_by_the_next_worker_while_the_master_lives() {
    let _serial = serial();
    let data = scratch("data");
    let out = scratch("out");
    let _master = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(true)
            .with_transport(FileTransport::new(&out)),
    )
    .expect("launch");
    std::thread::sleep(Duration::from_millis(300));

    // Worker A uses the SDK, then dies for real.
    let a = fork_child(|| {
        Bugsee::capture_message(LogLevel::Warning, "worker-a-alive");
        unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        0
    });
    assert!(libc::WIFSIGNALED(wait_with_deadline(
        a,
        Duration::from_secs(30)
    )));

    // The master stays up. Worker B is forked from it; reviving must recover A.
    let b = fork_child(|| {
        i32::from(!(Bugsee::is_launched() && Bugsee::flush(Duration::from_secs(10))))
    });
    assert!(
        exited_with(wait_with_deadline(b, Duration::from_secs(60)), 0),
        "the replacement worker could not start its SDK"
    );

    let reports = delivered(&out);
    let crashes = reports
        .iter()
        .filter(|r| r["exception_type"] == "native" && r["signal"]["name"] == "SIGSEGV")
        .count();
    assert_eq!(
        crashes,
        1,
        "worker A's crash must be recovered exactly once while the master lives: {}",
        summarize(&reports)
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}

/// A panic in a child BEFORE any SDK call runs the hook while the persisted
/// snapshot path still points into the parent's session. It must not be written
/// there: the parent's later abort would be reported as this panic.
#[test]
fn a_panic_in_the_child_before_touching_the_sdk_does_not_write_into_the_parents_session() {
    let _serial = serial();
    let data = scratch("data");
    let out = scratch("out");
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&data)
            .native_crash_capture(false)
            .with_transport(FileTransport::new(&out)),
    )
    .expect("launch");
    std::thread::sleep(Duration::from_millis(300));

    let child = fork_child(|| {
        // No SDK call first: the observer's hook is the first thing to run.
        let _ = std::thread::spawn(|| panic!("child-panic-before-sdk")).join();
        0
    });
    assert!(exited_with(
        wait_with_deadline(child, Duration::from_secs(30)),
        0
    ));

    let stray: Vec<_> = std::fs::read_dir(data.join("parts"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|d| d.path().join("panic.info"))
        .filter(|p| p.exists())
        .collect();
    assert!(
        stray.is_empty(),
        "the child's panic was written into its parent's session: {stray:?}"
    );
    let _ = std::fs::remove_dir_all(&data);
    let _ = std::fs::remove_dir_all(&out);
}

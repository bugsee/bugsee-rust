//
//  crash_flush_e2e.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The crash-time flush handshake against a real capture pipeline (issue #13).
//! The crash handler's side is exactly `flush_in_flight`; the subprocess crash
//! tests in the `bugsee` crate prove it from a real fault.
//!
//! The registration is process-global, so these tests run one at a time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee_core::capture::record::RecordIter;
use bugsee_core::crash_flush::{flush_in_flight, CrashFlush};
use bugsee_core::model::entry::{CaptureEntry, EventEntry};
use bugsee_core::runtime::TelemetrySampler;
use bugsee_core::{MockTransport, Recorder, RecorderConfig};
use serde_json::{Map, Value};

static SERIAL: Mutex<()> = Mutex::new(());

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "bugsee-cflush-{}",
            bugsee_core::util::random_hex(8)
        ));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn event(i: usize) -> CaptureEntry {
    let mut params = Map::new();
    params.insert("pad".into(), Value::String("x".repeat(300)));
    CaptureEntry::UserEvent(EventEntry {
        timestamp: 1_000 + i as i64,
        name: Some(format!("e{i}")),
        params,
        custom: Map::new(),
    })
}

/// Count the records the part files hold RIGHT NOW (no flush of ours involved).
fn records_on_disk(data: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![data.join("parts")];
    while let Some(dir) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in read.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.to_string_lossy().ends_with("events.user.part") {
                n += RecordIter::new(&std::fs::read(&p).unwrap()).count();
            }
        }
    }
    n
}

#[test]
fn everything_queued_before_the_request_is_on_disk_when_it_confirms() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.0, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();

    const N: usize = 1500;
    for i in 0..N {
        recorder.capture(event(i));
    }
    // The producer only queued; the worker is still behind.
    let behind = records_on_disk(&dir.0);

    assert!(
        flush_in_flight(Duration::from_secs(5)),
        "a healthy worker confirms"
    );
    assert_eq!(records_on_disk(&dir.0), N, "all {N} are durable on confirm");
    eprintln!("on disk before the request: {behind}/{N}");
    drop(recorder);
}

struct Blocker(Arc<AtomicBool>);
impl TelemetrySampler for Blocker {
    fn sample(&mut self) -> Vec<(String, Value)> {
        while !self.0.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Vec::new()
    }
}

#[test]
fn a_stuck_worker_costs_only_the_budget() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let release = Arc::new(AtomicBool::new(false));
    let mut cfg = RecorderConfig::new(&dir.0, "TOKEN");
    cfg.rotate_interval = Duration::from_millis(20);
    cfg.sampler = Some(Box::new(Blocker(Arc::clone(&release))));
    let recorder = Recorder::launch(cfg, Arc::new(MockTransport::default())).unwrap();
    // Let a tick start and wedge the worker inside the sampler.
    std::thread::sleep(Duration::from_millis(150));
    for i in 0..100 {
        recorder.capture(event(i));
    }

    let t = Instant::now();
    assert!(!flush_in_flight(Duration::from_millis(50)));
    let took = t.elapsed();
    assert!(took < Duration::from_millis(500), "bounded, took {took:?}");

    // Un-wedge: the same request now succeeds (the helper is not poisoned).
    release.store(true, Ordering::SeqCst);
    assert!(flush_in_flight(Duration::from_secs(5)));
    assert_eq!(records_on_disk(&dir.0), 100);
    drop(recorder);
}

/// A flush target that only counts its calls.
fn counter() -> (Arc<AtomicUsize>, impl Fn() -> bool + Send + Sync + 'static) {
    let n = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&n);
    (n, move || {
        c.fetch_add(1, Ordering::SeqCst);
        true
    })
}

#[test]
fn dropping_the_recorder_stops_flushing_into_it() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let (calls, f) = counter();
    let _sentinel = CrashFlush::install(f).unwrap();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.0, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    assert!(flush_in_flight(Duration::from_secs(2)));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the recorder is the target"
    );
    drop(recorder);
    // Nothing is registered now, so a request is answered at once rather than
    // waited out, and the replaced sentinel is not called.
    let t = Instant::now();
    assert!(flush_in_flight(Duration::from_secs(2)));
    assert!(t.elapsed() < Duration::from_millis(200), "no wait at all");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn memory_mode_does_not_take_over_the_target() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let (calls, f) = counter();
    let _sentinel = CrashFlush::install(f).unwrap();
    let mut cfg = RecorderConfig::new(&dir.0, "TOKEN");
    cfg.memory_only = Some("test".into());
    let recorder = Recorder::launch(cfg, Arc::new(MockTransport::default())).unwrap();
    assert!(flush_in_flight(Duration::from_secs(2)));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "still the sentinel");
    drop(recorder);
}

// The handshake itself, against stand-in flushers (no pipeline).

#[test]
fn a_responsive_flusher_is_confirmed_promptly() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (calls, f) = counter();
    let cf = CrashFlush::install(f).unwrap();
    let t = Instant::now();
    assert!(flush_in_flight(Duration::from_millis(500)));
    assert!(t.elapsed() < Duration::from_millis(250));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(cf);
    assert!(flush_in_flight(Duration::from_millis(500)));
    assert_eq!(calls.load(Ordering::SeqCst), 1, "deregistered: not called");
}

#[test]
fn the_last_registered_target_wins() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (a_calls, a) = counter();
    let (b_calls, b) = counter();
    let a = CrashFlush::install(a).unwrap();
    let b = CrashFlush::install(b).unwrap();
    assert!(flush_in_flight(Duration::from_millis(500)));
    assert_eq!(
        (
            a_calls.load(Ordering::SeqCst),
            b_calls.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
    // Dropping the one that is not current must not deregister the current one.
    drop(a);
    assert!(flush_in_flight(Duration::from_millis(500)));
    assert_eq!(b_calls.load(Ordering::SeqCst), 2);
    drop(b);
}

#[test]
fn a_stuck_flusher_costs_the_budget_and_no_more() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let release = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&release);
    let cf = CrashFlush::install(move || {
        while !r.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    })
    .unwrap();
    let t = Instant::now();
    assert!(!flush_in_flight(Duration::from_millis(50)));
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(45), "{took:?}");
    assert!(took < Duration::from_millis(400), "{took:?}");
    release.store(true, Ordering::SeqCst);
    drop(cf);
    // Let the helper finish the stuck call before the next test.
    std::thread::sleep(Duration::from_millis(50));
}

#[test]
fn a_panicking_flusher_does_not_kill_the_helper() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&n);
    let cf = CrashFlush::install(move || {
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            panic!("boom");
        }
        true
    })
    .unwrap();
    assert!(!flush_in_flight(Duration::from_millis(50)));
    // Let the helper finish the first request: two wakeups that land before it
    // reads are served by ONE flush call (which here panics).
    while n.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(flush_in_flight(Duration::from_millis(500)));
    drop(cf);
}

#[test]
fn a_pipeline_thread_does_not_wait_for_itself() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (calls, f) = counter();
    let cf = CrashFlush::install(f).unwrap();
    let t = Instant::now();
    let r = std::thread::spawn(|| {
        let _p = bugsee_core::crash_flush::register_pipeline_thread();
        flush_in_flight(Duration::from_secs(2))
    })
    .join()
    .unwrap();
    assert!(!r, "skipped");
    assert!(t.elapsed() < Duration::from_millis(200));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(cf);
}

#[test]
fn a_crash_flush_raises_the_dying_flag_and_blocks_new_claims() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let claim = dir.0.join("x.claim");
    bugsee_core::crash_flush::reset_dying_for_test();
    assert!(bugsee_core::claim::try_claim(&claim).is_some());
    let (_calls, f) = counter();
    let cf = CrashFlush::install(f).unwrap();
    // The plain flush (a panic hook's) does not declare the process dying.
    flush_in_flight(Duration::from_millis(100));
    assert!(!bugsee_core::crash_flush::is_dying());
    bugsee_core::crash_flush::flush_for_crash(Duration::from_millis(100));
    assert!(bugsee_core::crash_flush::is_dying());
    assert!(
        bugsee_core::claim::try_claim(&claim).is_none(),
        "a dying process starts no claim a kill would leave half-made"
    );
    assert!(bugsee_core::claim::try_claim_lenient(&claim).is_none());
    bugsee_core::crash_flush::reset_dying_for_test();
    assert!(bugsee_core::claim::try_claim(&claim).is_some());
    drop(cf);
}

/// A crash while the launch pass / worker recovery runs: a dying process must
/// recover nothing (a kill would strand the claim), and the session stays pending
/// for the next launch.
#[test]
fn a_dying_process_recovers_nothing_and_leaves_the_session_pending() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    std::fs::write(dir.0.join("gen"), "1").unwrap();
    let part = dir.0.join("parts/1/0");
    std::fs::create_dir_all(&part).unwrap();
    let payload = br#"{"timestamp":100,"level":1,"source":2,"message":"x"}"#;
    let mut bytes = Vec::new();
    bugsee_core::capture::record::frame(100, payload, &mut bytes);
    std::fs::write(part.join("log.part"), &bytes).unwrap();
    std::fs::create_dir_all(dir.0.join("sessions")).unwrap();
    std::fs::write(dir.0.join("sessions/1.alive"), "0").unwrap();

    bugsee_core::crash_flush::flush_for_crash(Duration::from_millis(10));
    assert!(bugsee_core::crash_flush::is_dying());
    let transport = Arc::new(MockTransport::default());
    let r1 = Recorder::launch(RecorderConfig::new(&dir.0, "TOKEN"), transport.clone()).unwrap();
    r1.recover_on_launch_path();
    assert!(r1.flush(Duration::from_secs(5)));
    assert!(transport.uploaded_bundles.lock().unwrap().is_empty());
    assert!(dir.0.join("sessions/1.alive").exists(), "still pending");
    drop(r1);

    // The worker reached its loop at once (no scan/GC ahead of it), so a flush
    // request is answered promptly and what was queued is on disk.
    let r1 = Recorder::launch(RecorderConfig::new(&dir.0, "TOKEN"), transport.clone()).unwrap();
    for i in 0..200 {
        r1.capture(event(i));
    }
    let t = Instant::now();
    assert!(flush_in_flight(Duration::from_secs(2)));
    assert!(t.elapsed() < Duration::from_millis(500));
    assert_eq!(records_on_disk(&dir.0), 200);
    drop(r1);

    bugsee_core::crash_flush::reset_dying_for_test();
    let r2 = Recorder::launch(RecorderConfig::new(&dir.0, "TOKEN"), transport.clone()).unwrap();
    r2.recover_on_launch_path();
    assert!(r2.flush(Duration::from_secs(5)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 1);
    drop(r2);
}

/// A `fork()` child has none of the parent's threads: it must not wait on the
/// parent's helper, and must get its own once it registers.
#[cfg(unix)]
#[test]
fn a_fork_child_gets_its_own_helper() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (_calls, f) = counter();
    let cf = CrashFlush::install(f).unwrap();
    assert!(flush_in_flight(Duration::from_millis(500)));
    // SAFETY: the child only calls the code under test and `_exit`s.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        let before = flush_in_flight(Duration::from_millis(500));
        let (_c, g) = counter();
        let mine = CrashFlush::install(g).unwrap();
        let after = flush_in_flight(Duration::from_millis(500));
        std::mem::forget(mine);
        let code = if !before && after { 0 } else { 1 };
        // SAFETY: leaving a forked child.
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    // SAFETY: waiting for our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "child status {status}"
    );
    assert!(
        flush_in_flight(Duration::from_millis(500)),
        "parent unaffected"
    );
    drop(cf);
}

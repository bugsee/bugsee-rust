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

#[test]
fn dropping_the_recorder_deregisters() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.0, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    assert!(flush_in_flight(Duration::from_secs(2)));
    drop(recorder);
    let t = Instant::now();
    assert!(!flush_in_flight(Duration::from_secs(2)));
    assert!(t.elapsed() < Duration::from_millis(200), "no wait at all");
}

// The handshake itself, against stand-in flushers (no pipeline).

#[test]
fn unregistered_returns_false_immediately() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let t = Instant::now();
    assert!(!flush_in_flight(Duration::from_millis(500)));
    assert!(t.elapsed() < Duration::from_millis(100));
}

#[test]
fn a_responsive_flusher_is_confirmed_promptly() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let calls = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&calls);
    let cf = CrashFlush::install(move || {
        c.fetch_add(1, Ordering::SeqCst);
        true
    })
    .unwrap();
    let t = Instant::now();
    assert!(flush_in_flight(Duration::from_millis(500)));
    assert!(t.elapsed() < Duration::from_millis(250));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(cf);
    assert!(!flush_in_flight(Duration::from_millis(50)), "deregistered");
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
fn memory_mode_has_nothing_to_flush() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = TempDir::new();
    let mut cfg = RecorderConfig::new(&dir.0, "TOKEN");
    cfg.memory_only = Some("test".into());
    let recorder = Recorder::launch(cfg, Arc::new(MockTransport::default())).unwrap();
    assert!(!flush_in_flight(Duration::from_millis(50)));
    drop(recorder);
}

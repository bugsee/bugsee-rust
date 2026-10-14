//
//  launch_recovery.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Early-crash recovery finishes on the launch path (issue #10): by the time
//! `recover_on_launch_path` returns, the previous run's crash is a queue entry on
//! disk, so a host that crashes again right away (a crash loop) cannot outrun its
//! report.
//!
//! The crash loop is real: this test binary re-executes itself as children that
//! launch the SDK and `abort()` immediately.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee_core::capture::record;
use bugsee_core::transport::{Transport, TransportError};
use bugsee_core::{queue, MockTransport, Recorder, RecorderConfig};

const ROLE: &str = "BUGSEE_LR_ROLE";
const DIR: &str = "BUGSEE_LR_DIR";
const OUT: &str = "BUGSEE_LR_OUT";
const LAUNCH_THREAD: &str = "bugsee-launch-recovery";

/// A transport that never delivers anything, so the queue keeps its entries.
struct Offline;
impl Transport for Offline {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
    fn upload_bundle(&self, _: &str, _: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::Transient("offline".into()))
    }
}

fn config(dir: &Path, budget: Duration) -> RecorderConfig {
    let mut config = RecorderConfig::new(dir, "tok");
    config.launch_recovery_budget = budget;
    config
}

/// Child entry point. A no-op in a normal test run.
#[test]
fn child_entry() {
    if std::env::var(ROLE).as_deref() != Ok("crash_after_launch") {
        return;
    }
    let dir = PathBuf::from(std::env::var(DIR).unwrap());
    let out = PathBuf::from(std::env::var(OUT).unwrap());
    let rec =
        Recorder::launch(config(&dir, Duration::from_millis(500)), Arc::new(Offline)).unwrap();
    rec.recover_on_launch_path();
    // The instant the launch path is done: record what is queued, then die the
    // way a crash does, with no destructor and no flush.
    std::fs::write(out, queue::list_pending(&dir).len().to_string()).unwrap();
    std::process::abort();
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p =
            std::env::temp_dir().join(format!("bugsee-lr-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn seed_crashed(dir: &Path, generations: std::ops::RangeInclusive<u64>) {
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for g in generations.clone() {
        // "0" is the portable "owner gone" sentinel.
        std::fs::write(sessions.join(format!("{g}.alive")), "0").unwrap();
        std::fs::create_dir_all(dir.join("parts").join(g.to_string())).unwrap();
    }
    std::fs::write(dir.join("gen"), generations.end().to_string()).unwrap();
}

/// A `before_send` that records the name of the thread it runs on and sleeps.
fn recording_hook(
    threads: &Arc<Mutex<Vec<String>>>,
    sleep: Duration,
) -> bugsee_core::runtime::BeforeSend {
    let seen = Arc::clone(threads);
    Box::new(move |_meta| {
        let t = std::thread::current();
        seen.lock()
            .unwrap()
            .push(t.name().unwrap_or("launch").to_string());
        std::thread::sleep(sleep);
        true
    })
}

fn on_launch_thread(threads: &Arc<Mutex<Vec<String>>>) -> usize {
    threads
        .lock()
        .unwrap()
        .iter()
        .filter(|n| *n == LAUNCH_THREAD)
        .count()
}

#[test]
fn the_previous_crash_is_queued_before_the_launch_path_returns() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=1);

    let rec =
        Recorder::launch(config(&dir, Duration::from_millis(500)), Arc::new(Offline)).unwrap();
    rec.recover_on_launch_path();
    // No flush, no waiting: it is on disk the moment the launch path returns.
    assert_eq!(queue::list_pending(&dir).len(), 1);
    drop(rec);
}

/// Time cap: a budget shorter than one session lets exactly the newest session
/// through on the launch path (it is always attempted); the rest follow on the
/// worker. And the launch path returns at the budget, not when the hook does.
#[test]
fn a_short_budget_recovers_one_session_on_the_launch_path() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=5);

    let threads: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut config = config(&dir, Duration::from_millis(50));
    config.before_send = Some(recording_hook(&threads, Duration::from_millis(400)));
    let transport = Arc::new(MockTransport::default());
    let started = Instant::now();
    let rec = Recorder::launch(config, transport.clone()).unwrap();
    rec.recover_on_launch_path();
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "the launch path waited for the hook: {:?}",
        started.elapsed()
    );

    assert!(rec.flush(Duration::from_secs(20)));
    assert_eq!(
        on_launch_thread(&threads),
        1,
        "{:?}",
        threads.lock().unwrap()
    );
    assert_eq!(threads.lock().unwrap().len(), 5, "each session once");
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 5);
}

/// Count cap: with time to spare, `launch_recovery_max_sessions` limits the pass.
#[test]
fn the_session_cap_limits_the_launch_path() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=5);

    let threads: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut config = config(&dir, Duration::from_secs(10));
    config.launch_recovery_max_sessions = 2;
    config.before_send = Some(recording_hook(&threads, Duration::ZERO));
    let transport = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config, transport.clone()).unwrap();
    rec.recover_on_launch_path();
    // The pass was quick, so it is over: exactly the cap ran on it.
    assert_eq!(on_launch_thread(&threads), 2);

    assert!(rec.flush(Duration::from_secs(20)));
    assert_eq!(threads.lock().unwrap().len(), 5, "each session once");
    // The pass's second session repeats the first one's crash and is dropped.
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 4);
}

/// A unique crash the launch pass has no queue room for is not destroyed: the
/// worker's (uncapped) pass queues it.
#[test]
fn a_full_queue_defers_a_unique_crash_to_the_worker_instead_of_discarding_it() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=1);
    for i in 0..queue::MAX_QUEUED_REPORTS {
        queue::enqueue(
            &dir,
            &bugsee_core::AssembledReport {
                bundle_name: format!("{}.bundle.zip", bugsee_core::util::random_hex(10)),
                zip: b"z".to_vec(),
                request_json: format!("{{\"signatures\":[\"other-{i}\"]}}").into_bytes(),
            },
        )
        .unwrap();
    }

    let rec =
        Recorder::launch(config(&dir, Duration::from_millis(500)), Arc::new(Offline)).unwrap();
    rec.recover_on_launch_path();
    // The launch pass refused it (the worker may already be queueing it, so
    // only the outcome is asserted): it ends up queued, not discarded, and
    // without having used up its recovery attempts.
    // Observable condition, generous timeout: a loaded CI machine can delay the
    // helper thread, the worker's wait (budget plus a 5 s grace) and its pass.
    let deadline = Instant::now() + Duration::from_secs(60);
    while queue::list_pending(&dir).len() <= queue::MAX_QUEUED_REPORTS {
        assert!(
            Instant::now() < deadline,
            "the worker never recovered it: {} queued, session marker present: {}",
            queue::list_pending(&dir).len(),
            dir.join("sessions").join("1.alive").exists()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(rec);
}

/// A hook that never returns must not hold `launch` past the budget.
#[test]
fn a_blocked_before_send_cannot_stall_the_launch_path() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=1);

    let release = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&release);
    let mut config = config(&dir, Duration::from_millis(100));
    config.before_send = Some(Box::new(move |_meta| {
        while !gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }));
    let transport = Arc::new(MockTransport::default());
    let started = Instant::now();
    let rec = Recorder::launch(config, transport.clone()).unwrap();
    rec.recover_on_launch_path();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "launch waited on a blocked hook: {:?}",
        started.elapsed()
    );

    // Unblocked, the pass finishes in the background and the report goes out.
    release.store(true, Ordering::SeqCst);
    assert!(rec.flush(Duration::from_secs(20)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 1);
}

/// Without a launch-path pass (the core default), the worker does it all.
#[test]
fn a_zero_budget_leaves_recovery_to_the_worker() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    seed_crashed(&dir, 1..=2);

    let threads: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut config = config(&dir, Duration::ZERO);
    config.before_send = Some(recording_hook(&threads, Duration::ZERO));
    let transport = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config, transport.clone()).unwrap();
    rec.recover_on_launch_path();
    assert!(rec.flush(Duration::from_secs(10)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 2);
    assert!(threads
        .lock()
        .unwrap()
        .iter()
        .all(|n| n == "bugsee-capture"));
}

/// The product scenario: the host crashes right after every launch. A crash loop
/// reports the same crash each time, so the first copy is queued and later copies
/// are not stacked behind it; and each launch already holds the previous crash in
/// the queue on return, with no async worker involved.
#[test]
fn a_crash_loop_queues_the_first_crash_before_each_relaunch_and_collapses_repeats() {
    const CRASHES: usize = 5;
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    std::fs::create_dir_all(&dir).unwrap();

    for i in 0..CRASHES {
        let out = tmp.0.join(format!("queued-{i}"));
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_entry", "--nocapture"])
            .env(ROLE, "crash_after_launch")
            .env(DIR, &dir)
            .env(OUT, &out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "child {i} should have crashed");
        let queued: usize = std::fs::read_to_string(&out)
            .unwrap_or_else(|_| panic!("child {i} never got past launch"))
            .parse()
            .unwrap();
        // Child 0 has nothing to recover. Every later child found the crash
        // before it already queued, and identical repeats are not queued again.
        assert_eq!(queued, usize::from(i > 0), "child {i} saw {queued} queued");
    }

    // Another launch, offline so nothing is delivered behind the test's back,
    // recovers the last child's crash: a repeat of the queued one, so dropped.
    let rec =
        Recorder::launch(config(&dir, Duration::from_millis(500)), Arc::new(Offline)).unwrap();
    rec.recover_on_launch_path();
    assert_eq!(queue::list_pending(&dir).len(), 1);
    drop(rec);

    // A healthy run delivers the queued crash, once.
    let transport = Arc::new(MockTransport::default());
    let rec =
        Recorder::launch(config(&dir, Duration::from_millis(500)), transport.clone()).unwrap();
    rec.recover_on_launch_path();
    assert!(rec.flush(Duration::from_secs(20)));
    assert_eq!(transport.uploaded_bundles.lock().unwrap().len(), 1);
    assert!(queue::list_pending(&dir).is_empty());
    drop(rec);

    // And not again.
    let again = Arc::new(MockTransport::default());
    let rec = Recorder::launch(config(&dir, Duration::from_millis(500)), again.clone()).unwrap();
    rec.recover_on_launch_path();
    assert!(rec.flush(Duration::from_secs(5)));
    assert!(again.uploaded_bundles.lock().unwrap().is_empty());
}

/// Launch latency of the pass, for DESIGN.md: run with
/// `cargo test -p bugsee-core --release --test launch_recovery -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure_launch_latency() {
    fn seed_with_window(dir: &Path, generations: std::ops::RangeInclusive<u64>, bytes: usize) {
        seed_crashed(dir, generations.clone());
        for g in generations {
            let part = dir.join("parts").join(g.to_string()).join("0");
            std::fs::create_dir_all(&part).unwrap();
            let mut buf = Vec::new();
            let mut ts = 1_000i64;
            while buf.len() < bytes {
                let payload = format!(
                    r#"{{"timestamp":{ts},"level":1,"source":2,"message":"{}"}}"#,
                    "x".repeat(200)
                );
                record::frame(ts, payload.as_bytes(), &mut buf);
                ts += 1;
            }
            std::fs::write(part.join("log.part"), &buf).unwrap();
        }
    }
    for (label, sessions, bytes) in [
        ("empty directory", 0u64, 0usize),
        ("1 session, 8 MiB window", 1, 8 << 20),
        ("8 sessions, 8 MiB windows", 8, 8 << 20),
    ] {
        let tmp = TempDir::new();
        let dir = tmp.0.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        if sessions > 0 {
            seed_with_window(&dir, 1..=sessions, bytes);
        }
        let mut config = config(&dir, Duration::from_secs(30));
        config.caps = bugsee_core::capture::WindowCaps {
            max_window_ms: 60_000,
            max_bytes: 8 << 20,
            max_events: 100_000,
        };
        let t0 = Instant::now();
        let rec = Recorder::launch(config, Arc::new(Offline)).unwrap();
        let launched = t0.elapsed();
        let t1 = Instant::now();
        rec.recover_on_launch_path();
        println!(
            "{label}: Recorder::launch {launched:?}, recover_on_launch_path {:?}, queued {}",
            t1.elapsed(),
            queue::list_pending(&dir).len()
        );
    }
}

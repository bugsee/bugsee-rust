//
//  release_health.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Issue #12 end to end: how a session ended, recorded locally and settled
//! exactly once, across real process deaths.
//!
//! A child process launches the SDK with release health on and ends in the
//! requested way; the parent inspects what is on disk, relaunches (which
//! recovers the dead session) and relaunches again (which must not count it a
//! second time).
//!
//! `harness = false`, like the crash matrix: the child dies from the real main
//! thread (an uncaught panic must unwind out of `main`; a panic on any other
//! thread is by definition an error, not a crash).

#![cfg(all(feature = "native", any(unix, windows)))]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::health::{SessionRecord, SessionStatus};
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};

const KIND_ENV: &str = "BUGSEE_HEALTH_E2E_KIND";
const DIR_ENV: &str = "BUGSEE_HEALTH_E2E_DIR";

#[derive(Debug)]
struct Boom;
impl std::fmt::Display for Boom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("boom")
    }
}
impl std::error::Error for Boom {}

fn launch(dir: &str, health: bool) -> bugsee::LaunchGuard {
    Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir)
            .app_version("4.5.6")
            .app_build("789")
            .release_health(health)
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch")
}

// ---------------------------------------------------------------------------
// The child.
// ---------------------------------------------------------------------------

#[inline(never)]
fn fault_null() {
    // SAFETY: deliberately faulting.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) }
}

fn run_child(kind: &str, dir: &str) -> ! {
    let guard = launch(dir, kind != "health_off");
    std::thread::sleep(Duration::from_millis(300));
    match kind {
        "clean" | "health_off" => {}
        "errored" => {
            Bugsee::capture_error(&Boom);
            Bugsee::capture_error(&Boom);
        }
        // The process survives a panic on another thread: an error, not a crash.
        "thread_panic" => {
            let _ = std::thread::Builder::new()
                .name("worker".into())
                .spawn(|| panic!("worker-thread-panic"))
                .unwrap()
                .join();
            std::thread::sleep(Duration::from_millis(300));
        }
        // The panic's snapshot must not outlive it: the process is then killed,
        // which is an abnormal exit, not the panic's crash. The pause lets the
        // worker persist the error count before the kill.
        "thread_panic_then_exit" => {
            let _ = std::thread::Builder::new()
                .name("worker".into())
                .spawn(|| panic!("worker-thread-panic"))
                .unwrap()
                .join();
            std::thread::sleep(Duration::from_millis(2500));
            std::process::exit(3);
        }
        "segv" => fault_null(),
        "unwinding_panic" => {
            let _keep = &guard;
            panic!("health-e2e-deliberate-panic");
        }
        "exit_without_shutdown" => std::process::exit(3),
        other => panic!("unknown kind {other}"),
    }
    drop(guard);
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------

struct Case {
    kind: &'static str,
    /// Whether the child is expected to die rather than exit 0.
    dies: bool,
    status: SessionStatus,
    min_errors: u64,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            kind: "clean",
            dies: false,
            status: SessionStatus::Ok,
            min_errors: 0,
        },
        Case {
            kind: "errored",
            dies: false,
            status: SessionStatus::Errored,
            min_errors: 2,
        },
        Case {
            kind: "thread_panic",
            dies: false,
            status: SessionStatus::Errored,
            min_errors: 1,
        },
        Case {
            kind: "thread_panic_then_exit",
            dies: true,
            status: SessionStatus::Abnormal,
            min_errors: 1,
        },
        Case {
            kind: "segv",
            dies: true,
            status: SessionStatus::Crashed,
            min_errors: 0,
        },
        Case {
            kind: "unwinding_panic",
            dies: true,
            status: SessionStatus::Crashed,
            min_errors: 0,
        },
        Case {
            kind: "exit_without_shutdown",
            dies: true,
            status: SessionStatus::Abnormal,
            min_errors: 0,
        },
    ]
}

/// Relaunch over `dir` (recovering whatever died), and return every finished
/// session, then stop cleanly.
fn relaunch(dir: &Path) -> Vec<SessionRecord> {
    let guard = launch(dir.to_str().unwrap(), true);
    std::thread::sleep(Duration::from_millis(300));
    Bugsee::flush(Duration::from_secs(10));
    let all = Bugsee::__finished_sessions();
    drop(guard);
    all
}

fn spawn_child(kind: &str, dir: &Path) -> std::process::Output {
    std::process::Command::new(std::env::current_exe().unwrap())
        .env(KIND_ENV, kind)
        .env(DIR_ENV, dir)
        .output()
        .expect("spawn child")
}

fn run_case(case: &Case) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-health-e2e-{}-{}",
        case.kind,
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        let out = spawn_child(case.kind, &dir);
        if out.status.success() == case.dies {
            return Err(format!(
                "child exit {:?} (wanted dies={}); stderr={}",
                out.status,
                case.dies,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        // Whatever happened, the child was generation 1.
        let first = relaunch(&dir);
        let mine: Vec<_> = first.iter().filter(|r| r.generation == 1).collect();
        if mine.len() != 1 {
            return Err(format!("want one record for the child, got {first:?}"));
        }
        let r = mine[0];
        if r.status != case.status {
            return Err(format!(
                "status {:?}, want {:?}: {r:?}",
                r.status, case.status
            ));
        }
        if r.errors < case.min_errors {
            return Err(format!(
                "errors {}, want >= {}: {r:?}",
                r.errors, case.min_errors
            ));
        }
        if r.app_version.as_deref() != Some("4.5.6") || r.app_build.as_deref() != Some("789") {
            return Err(format!("release lost: {r:?}"));
        }
        if r.duration_ms.is_none() || r.ended_at.is_none() {
            return Err(format!("no end or duration: {r:?}"));
        }

        // A later launch must not count the dead session again, nor change it.
        let second = relaunch(&dir);
        let again: Vec<_> = second.iter().filter(|x| x.generation == 1).collect();
        if again.len() != 1 || *again[0] != *r {
            return Err(format!("session counted or changed twice: {second:?}"));
        }
        // The relaunches themselves ended cleanly.
        let others = second.iter().filter(|x| x.generation != 1);
        for o in others {
            if o.status != SessionStatus::Ok {
                return Err(format!("a clean relaunch is not ok: {o:?}"));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Off by default: a run without release health leaves no record, and a later
/// run that has it on does not invent one for it.
fn run_off() -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-health-e2e-off-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        let out = spawn_child("health_off", &dir);
        if !out.status.success() {
            return Err(format!("child failed: {:?}", out.status));
        }
        if dir.join("health").exists() {
            return Err("release health off still wrote records".into());
        }
        let all = relaunch(&dir);
        if all.iter().any(|r| r.generation == 1) {
            return Err(format!("a record appeared for the untracked run: {all:?}"));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn main() {
    if let Ok(kind) = std::env::var(KIND_ENV) {
        let dir = std::env::var(DIR_ENV).expect("data dir");
        run_child(&kind, &dir);
    }
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let mut failed = Vec::new();
    let mut run = |name: &str, f: &dyn Fn() -> Result<(), String>| {
        if filter.as_deref().is_some_and(|x| !name.contains(x)) {
            return;
        }
        match f() {
            Ok(()) => println!("test release_health::{name} ... ok"),
            Err(e) => {
                println!("test release_health::{name} ... FAILED\n    {e}");
                failed.push(name.to_string());
            }
        }
    };
    for case in cases() {
        run(case.kind, &|| run_case(&case));
    }
    run("off_by_default", &run_off);
    if !failed.is_empty() {
        println!("\nrelease health failures: {failed:?}");
        std::process::exit(1);
    }
}

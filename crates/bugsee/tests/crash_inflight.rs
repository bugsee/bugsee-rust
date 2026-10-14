//
//  crash_inflight.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Best-effort flush of in-flight events at crash time (issue #13).
//!
//! A producer only queues an entry; the capture worker appends it to disk a
//! little later. These tests die for real, in a child process, immediately after
//! emitting a burst of events, and read what the next launch recovers:
//!
//! * `burst_then_segv` — every event emitted before the fault, including the
//!   ones still queued when it hit, is in the recovered report.
//! * `wedged_worker_then_segv` — the worker is stuck in a host callback, so the
//!   flush cannot succeed. The process must still die promptly (the handler is
//!   bounded; the 5 s watchdog must not be what ends it) and the crash is still
//!   reported, with the events that had already reached disk.
//!
//! `harness = false` (see `Cargo.toml`): the child must fault from the real main
//! thread.

#![cfg(all(feature = "native", unix))]

use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::{Map, Value};
use zip::ZipArchive;

const KIND_ENV: &str = "BUGSEE_INFLIGHT_KIND";
const DIR_ENV: &str = "BUGSEE_INFLIGHT_DIR";
/// Events in the burst. Sized so the producer (which only queues) finishes well
/// before the worker (which serializes and writes) has drained, yet the worker
/// can drain within the 50 ms handler budget.
const BURST: usize = 1000;

#[inline(never)]
fn fault_null() {
    // SAFETY: deliberately faulting.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) }
}

fn emit_burst(prefix: &str) {
    let pad = "x".repeat(400);
    for i in 0..BURST {
        let mut params = Map::new();
        params.insert("pad".into(), Value::String(pad.clone()));
        Bugsee::event_with(format!("{prefix}-{i}"), params);
    }
}

fn run_child(kind: &str, dir: &str) -> ! {
    let wedge = kind == "wedged_worker_then_segv";
    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir)
            .native_crash_capture(true)
            // Runs on the capture worker: wedging it stands in for a worker blocked
            // on a dead disk or a host callback that never returns.
            .before_send(move |_| {
                if wedge {
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                }
                #[allow(unreachable_code)]
                true
            })
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch");
    std::thread::sleep(Duration::from_millis(300));

    match kind {
        "burst_then_segv" => emit_burst("burst"),
        "wedged_worker_then_segv" => {
            // These reach disk before the worker is wedged...
            for i in 0..20 {
                Bugsee::event(format!("early-{i}"));
            }
            Bugsee::flush(Duration::from_secs(2));
            // ...then a report wedges it in `before_send`, and everything after
            // is stuck in the queue.
            Bugsee::capture_message(LogLevel::Error, "wedge the worker");
            std::thread::sleep(Duration::from_millis(300));
            emit_burst("stuck");
        }
        other => panic!("unknown kind {other}"),
    }
    println!("FAULTING");
    let _ = std::io::stdout().flush();
    fault_null();
    std::process::exit(0);
}

fn event_names(bundle: &[u8]) -> Vec<String> {
    let mut zip = ZipArchive::new(Cursor::new(bundle)).expect("bundle is a zip");
    let mut names = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).expect("zip entry");
        if !entry.name().contains("events.user") {
            continue;
        }
        let mut raw = Vec::new();
        entry.read_to_end(&mut raw).expect("read events");
        let v: Value = serde_json::from_slice(&raw).expect("events parse");
        for e in v["events"].as_array().cloned().unwrap_or_default() {
            if let Some(n) = e["name"].as_str() {
                names.push(n.to_string());
            }
        }
    }
    names
}

fn crash_json(bundle: &[u8]) -> Value {
    let mut zip = ZipArchive::new(Cursor::new(bundle)).expect("bundle is a zip");
    let mut entry = zip.by_name("crash.json").expect("crash.json");
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn recover(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");
    std::thread::sleep(Duration::from_millis(1500));
    Bugsee::flush(Duration::from_secs(10));
    let bundles = mock.uploaded_bundles.lock().unwrap().clone();
    drop(guard);
    bundles
}

/// Run the child to its death; returns the time from its "about to fault" line to
/// its exit.
fn crash_child(kind: &str, dir: &std::path::Path) -> Result<Duration, String> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .env(KIND_ENV, kind)
        .env(DIR_ENV, dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let faulted = Instant::now();
    if line.trim() != "FAULTING" {
        let _ = child.kill();
        return Err(format!("child never reached the fault: {line:?}"));
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let took = faulted.elapsed();
    if status.success() {
        return Err("child should have died".into());
    }
    Ok(took)
}

fn with_dir<T>(
    kind: &str,
    f: impl FnOnce(&std::path::Path) -> Result<T, String>,
) -> Result<T, String> {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-inflight-{kind}-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let r = f(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn burst_then_segv() -> Result<(), String> {
    with_dir("burst", |dir| {
        crash_child("burst_then_segv", dir)?;
        let bundles = recover(dir);
        if bundles.len() != 1 {
            return Err(format!("expected one report, got {}", bundles.len()));
        }
        let crash = crash_json(&bundles[0]);
        if crash["signal"]["name"] != "SIGSEGV" {
            return Err(format!("not a SIGSEGV report: {crash}"));
        }
        let names = event_names(&bundles[0]);
        let last = format!("burst-{}", BURST - 1);
        let have = names.iter().filter(|n| n.starts_with("burst-")).count();
        if !names.contains(&last) {
            return Err(format!(
                "the last event before the crash was lost ({have}/{BURST} recovered)"
            ));
        }
        if have != BURST {
            return Err(format!("only {have}/{BURST} events recovered"));
        }
        Ok(())
    })
}

fn wedged_worker_then_segv() -> Result<(), String> {
    with_dir("wedged", |dir| {
        let took = crash_child("wedged_worker_then_segv", dir)?;
        if took > Duration::from_secs(2) {
            return Err(format!("the dying process hung for {took:?}"));
        }
        let bundles = recover(dir);
        if bundles.len() != 1 {
            return Err(format!("expected one report, got {}", bundles.len()));
        }
        let crash = crash_json(&bundles[0]);
        let has_frames = crash["frames"].as_array().is_some_and(|f| !f.is_empty());
        if crash["signal"]["name"] != "SIGSEGV" || !has_frames {
            return Err(format!("crash report incomplete: {crash}"));
        }
        let names = event_names(&bundles[0]);
        if !names.contains(&"early-19".to_string()) {
            return Err(format!("events already on disk were lost: {names:?}"));
        }
        Ok(())
    })
}

type Case = fn() -> Result<(), String>;

fn main() {
    if let Ok(kind) = std::env::var(KIND_ENV) {
        run_child(&kind, &std::env::var(DIR_ENV).expect("dir"));
    }
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let cases: [(&str, Case); 2] = [
        ("burst_then_segv", burst_then_segv),
        ("wedged_worker_then_segv", wedged_worker_then_segv),
    ];
    let mut failed = Vec::new();
    for (name, case) in cases {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        match case() {
            Ok(()) => println!("test crash_inflight::{name} ... ok"),
            Err(e) => {
                println!("test crash_inflight::{name} ... FAILED\n    {e}");
                failed.push(name);
            }
        }
    }
    if !failed.is_empty() {
        std::process::exit(1);
    }
}

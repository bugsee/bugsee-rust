//
//  recovery_e2e.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Phase 3 next-launch recovery: a prior session that ended abnormally (its
//! liveness marker survived) is recovered on the next launch — its captured
//! window is delivered with an abnormal-exit crash payload.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::capture::record;
use bugsee_core::{MockTransport, Recorder, RecorderConfig};
use serde_json::Value;
use zip::ZipArchive;

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("bugsee-recover-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Lay down a crashed generation `gen` on disk: gen counter, a `.alive` marker,
/// and a `log.part` with two records.
fn seed_crashed_session(data: &Path, generation: u64) {
    std::fs::write(data.join("gen"), generation.to_string()).unwrap();

    let part = data.join("parts").join(generation.to_string()).join("0");
    std::fs::create_dir_all(&part).unwrap();
    let mut bytes = Vec::new();
    for (ts, msg) in [(100i64, "pre-crash line A"), (200, "pre-crash line B")] {
        let payload = format!(
            r#"{{"timestamp":{ts},"level":1,"source":2,"message":"{msg}"}}"#
        );
        record::frame(ts, payload.as_bytes(), &mut bytes);
    }
    std::fs::write(part.join("log.part"), &bytes).unwrap();

    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "4242").unwrap();
}

#[test]
fn abnormal_prior_session_is_recovered_on_next_launch() {
    let dir = TempDir::new();
    seed_crashed_session(&dir.path, 1);

    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "TOKEN"), transport.clone()).unwrap();

    // recovery runs on the worker before the drain loop; flush waits past it.
    assert!(recorder.flush(Duration::from_secs(5)));

    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "the crashed session was delivered");

    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();

    // crash.json is an abnormal-exit payload.
    let mut cbytes = Vec::new();
    zip.by_name("crash.json").unwrap().read_to_end(&mut cbytes).unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();
    assert_eq!(crash["exception"]["name"], "AppExit");
    assert_eq!(crash["exception"]["domain"], "AppExit::Unknown");
    assert_eq!(crash["ndkCrash"], false);

    // The pre-crash log window rode along.
    let log_name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .find(|n| n.ends_with(".log.json"))
        .expect("log capture file present");
    let mut lbytes = Vec::new();
    zip.by_name(&log_name).unwrap().read_to_end(&mut lbytes).unwrap();
    let log: Value = serde_json::from_slice(&lbytes).unwrap();
    let msgs: Vec<&str> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["message"].as_str().unwrap())
        .collect();
    assert_eq!(msgs, vec!["pre-crash line A", "pre-crash line B"]);

    // request.json is a crash issue.
    let mut rbytes = Vec::new();
    zip.by_name("request.json").unwrap().read_to_end(&mut rbytes).unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["type"], "crash");
    assert_eq!(req["source"]["type"], "crash");

    // The recovered generation's on-disk state was cleaned up.
    drop(bundles);
    assert!(!dir.path.join("parts").join("1").exists(), "prior parts removed");
    assert!(!dir.path.join("sessions").join("1.alive").exists(), "marker removed");

    drop(recorder);
}

/// Seed a prior generation that died from a native fault: captured log +
/// crash-info marker + a (fake) minidump.
fn seed_native_crash(data: &Path, generation: u64) {
    std::fs::write(data.join("gen"), generation.to_string()).unwrap();
    let gen_dir = data.join("parts").join(generation.to_string());
    let part = gen_dir.join("0");
    std::fs::create_dir_all(&part).unwrap();

    let mut bytes = Vec::new();
    record::frame(
        50,
        br#"{"timestamp":50,"level":1,"source":2,"message":"before segfault"}"#,
        &mut bytes,
    );
    std::fs::write(part.join("log.part"), &bytes).unwrap();

    std::fs::write(gen_dir.join("crash.info"), "signal=11\ncode=1\naddress=0x0\n").unwrap();
    std::fs::write(gen_dir.join("crash.minidump"), b"MDMP\x00fake-minidump-bytes").unwrap();

    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "1").unwrap();
}

#[test]
fn native_crash_is_recovered_with_minidump_and_signal() {
    let dir = TempDir::new();
    seed_native_crash(&dir.path, 1);

    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "T"), transport.clone()).unwrap();
    assert!(recorder.flush(Duration::from_secs(5)));

    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();

    // Thin native crash.json with signal info.
    let mut cbytes = Vec::new();
    zip.by_name("crash.json").unwrap().read_to_end(&mut cbytes).unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();
    assert_eq!(crash["ndkCrash"], true);
    assert_eq!(crash["exception_type"], "native");
    assert_eq!(crash["handled"], false);
    assert_eq!(crash["signal"]["number"], 11);
    assert_eq!(crash["signal"]["name"], "SIGSEGV");
    assert!(crash.get("exception").is_none(), "native variant is thin (no exception)");

    // The minidump is bundled and listed with type `minidump`.
    let mut mbytes = Vec::new();
    zip.by_name("manifest.json").unwrap().read_to_end(&mut mbytes).unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    let types: Vec<&str> =
        man["files"].as_array().unwrap().iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"minidump"), "minidump bundled: {types:?}");
    assert!(types.contains(&"log"), "pre-crash log window bundled");
}

/// Seed a prior generation that panicked and aborted: SIGABRT crash-info plus a
/// persisted panic snapshot.
fn seed_aborting_panic(data: &Path, generation: u64) {
    use bugsee_core::model::crash::{Frame, FrameData};
    use bugsee_core::panic_info::PanicInfo;

    std::fs::write(data.join("gen"), generation.to_string()).unwrap();
    let gen_dir = data.join("parts").join(generation.to_string());
    std::fs::create_dir_all(gen_dir.join("0")).unwrap();

    // SIGABRT marker from the native handler.
    std::fs::write(gen_dir.join("crash.info"), "signal=6\ncode=0\naddress=0x0\n").unwrap();

    // Panic snapshot from the observer.
    let info = PanicInfo {
        reason: "index out of bounds".into(),
        file: Some("src/checkout.rs".into()),
        line: 42,
        column: 9,
        timestamp: 1000,
        frames: vec![Frame {
            trace: "app::checkout::settle".into(),
            hidden: false,
            data: FrameData {
                source: Some("src/checkout.rs".into()),
                member_class: Some("app::checkout".into()),
                member: Some("settle".into()),
                line: 42,
            },
        }],
    };
    info.write_to(&gen_dir.join("panic.info")).unwrap();

    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "1").unwrap();
}

#[test]
fn aborting_panic_correlates_with_sigabrt_into_one_event() {
    let dir = TempDir::new();
    seed_aborting_panic(&dir.path, 1);

    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "T"), transport.clone()).unwrap();
    assert!(recorder.flush(Duration::from_secs(5)));

    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1, "exactly one correlated event, not two");

    let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
    let mut cbytes = Vec::new();
    zip.by_name("crash.json").unwrap().read_to_end(&mut cbytes).unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();

    // One managed crash event carrying the Rust panic frames; the SIGABRT is
    // recorded via the exception `domain` (the contract reserves the top-level
    // `signal` object for the native variant, so a managed variant must not emit
    // it, nor the non-contract `mechanism` key).
    assert_eq!(crash["handled"], false);
    assert_eq!(crash["ndkCrash"], false);
    assert!(crash.get("mechanism").is_none(), "no off-contract mechanism key");
    assert!(crash.get("signal").is_none(), "no signal object on the managed variant");
    assert_eq!(crash["exception"]["name"], "panic");
    assert_eq!(crash["exception"]["domain"], "Signal::SIGABRT");
    let reason = crash["exception"]["reason"].as_str().unwrap();
    assert!(reason.contains("index out of bounds") && reason.contains("checkout.rs:42:9"));
    assert_eq!(crash["exception"]["frames"][0]["trace"], "app::checkout::settle");
    assert_eq!(crash["signatures"].as_array().unwrap().len(), 1);
}

#[test]
fn clean_shutdown_is_not_recovered() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());

    // First session: launch and cleanly drop (removes its marker).
    {
        let r = Recorder::launch(RecorderConfig::new(&dir.path, "T"), transport.clone()).unwrap();
        r.flush(Duration::from_secs(5));
    } // clean Drop → session.end()

    // Second session should find nothing to recover.
    let transport2 = Arc::new(MockTransport::default());
    let r2 = Recorder::launch(RecorderConfig::new(&dir.path, "T"), transport2.clone()).unwrap();
    assert!(r2.flush(Duration::from_secs(5)));
    assert_eq!(
        transport2.uploaded_bundles.lock().unwrap().len(),
        0,
        "a cleanly-closed session is never recovered"
    );
}

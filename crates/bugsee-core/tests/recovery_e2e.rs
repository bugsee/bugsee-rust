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
        path.push(format!(
            "bugsee-recover-{}",
            bugsee_core::util::random_hex(8)
        ));
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
        let payload = format!(r#"{{"timestamp":{ts},"level":1,"source":2,"message":"{msg}"}}"#);
        record::frame(ts, payload.as_bytes(), &mut bytes);
    }
    std::fs::write(part.join("log.part"), &bytes).unwrap();

    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // Marker records the (dead) owner pid; "0" is a portable "owner gone"
    // sentinel so recovery's liveness check does not treat it as a live peer.
    std::fs::write(sessions.join(format!("{generation}.alive")), "0").unwrap();
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
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut cbytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();
    assert_eq!(crash["exception"]["name"], "AppExit");
    assert_eq!(crash["exception"]["domain"], "AppExit::Unknown");
    assert_eq!(crash["ndkCrash"], false);

    // The pre-crash log window rode along.
    let log_name = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
        .find(|n| n.ends_with(".log.json"))
        .expect("log capture file present");
    let mut lbytes = Vec::new();
    zip.by_name(&log_name)
        .unwrap()
        .read_to_end(&mut lbytes)
        .unwrap();
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
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut rbytes)
        .unwrap();
    let req: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(req["type"], "crash");
    assert_eq!(req["source"]["type"], "crash");

    // The recovered generation's on-disk state was cleaned up.
    drop(bundles);
    assert!(
        !dir.path.join("parts").join("1").exists(),
        "prior parts removed"
    );
    assert!(
        !dir.path.join("sessions").join("1.alive").exists(),
        "marker removed"
    );

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

    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=11\ncode=1\naddress=0x0\n",
    )
    .unwrap();
    std::fs::write(
        gen_dir.join("crash.minidump"),
        b"MDMP\x00fake-minidump-bytes",
    )
    .unwrap();

    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "0").unwrap();
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
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut cbytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();
    assert_eq!(crash["ndkCrash"], true);
    assert_eq!(crash["exception_type"], "native");
    assert_eq!(crash["handled"], false);
    assert_eq!(crash["signal"]["number"], 11);
    assert_eq!(crash["signal"]["name"], "SIGSEGV");
    assert!(
        crash.get("exception").is_none(),
        "native variant is thin (no exception)"
    );

    // The minidump is bundled and listed with type `minidump`.
    let mut mbytes = Vec::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_end(&mut mbytes)
        .unwrap();
    let man: Value = serde_json::from_slice(&mbytes).unwrap();
    let types: Vec<&str> = man["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["type"].as_str().unwrap())
        .collect();
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

    // SIGABRT marker from the native handler. The `time=` matches the panic
    // snapshot's timestamp below so the freshness check correlates them (a stale
    // snapshot outside the window would fall through to a thin native report).
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=6\ncode=0\naddress=0x0\ntime=1000\n",
    )
    .unwrap();

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
    std::fs::write(sessions.join(format!("{generation}.alive")), "0").unwrap();
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
    zip.by_name("crash.json")
        .unwrap()
        .read_to_end(&mut cbytes)
        .unwrap();
    let crash: Value = serde_json::from_slice(&cbytes).unwrap();

    // One managed crash event carrying the Rust panic frames. The contract
    // reserves the top-level `signal` object for the native variant (a managed
    // variant must not emit it, nor the non-contract `mechanism` key), and the
    // `domain` inventory is limited to AppHang::*/AppExit::*/null — so an aborting
    // panic reports `domain: null` (the SIGABRT is implied by the aborting panic).
    assert_eq!(crash["handled"], false);
    assert_eq!(crash["ndkCrash"], false);
    assert!(
        crash.get("mechanism").is_none(),
        "no off-contract mechanism key"
    );
    assert!(
        crash.get("signal").is_none(),
        "no signal object on the managed variant"
    );
    assert_eq!(crash["exception"]["name"], "panic");
    assert!(
        crash["exception"]["domain"].is_null(),
        "domain null: Signal::* is off the contract inventory"
    );
    let reason = crash["exception"]["reason"].as_str().unwrap();
    assert!(reason.contains("index out of bounds") && reason.contains("checkout.rs:42:9"));
    assert_eq!(
        crash["exception"]["frames"][0]["trace"],
        "app::checkout::settle"
    );
    assert_eq!(crash["signatures"].as_array().unwrap().len(), 1);
}

/// A liveness marker still owned by a *live other* process (a peer sharing the
/// data dir) is not recovered — it is not a crash. Once the owner exits, the same
/// marker becomes recoverable.
#[cfg(unix)]
#[test]
fn live_peer_marker_is_skipped_until_owner_exits() {
    use bugsee_core::recovery::find_pending;

    let dir = TempDir::new();
    // A real, live child process whose pid we record as the marker's owner.
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let live_pid = child.id();

    let sessions = dir.path.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("1.alive"), live_pid.to_string()).unwrap();

    // Owner alive → skipped (deferred, not resurrected as a crash).
    assert!(
        find_pending(&dir.path, 2).is_empty(),
        "a live peer's session must not be recovered"
    );

    // Owner exits → the very same marker is now recoverable.
    child.kill().unwrap();
    child.wait().unwrap();
    let pending = find_pending(&dir.path, 2);
    assert_eq!(
        pending.len(),
        1,
        "once the owner is gone the marker is recovered"
    );
    assert_eq!(pending[0].generation, 1);
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

/// A process that dies mid-`write` (or whose in-flight flush was cut short by the
/// crash handler's budget) leaves a torn tail on the part it was appending to.
/// Recovery must deliver every complete record before it and ignore whatever
/// follows — never panic, never loop, never read past the buffer.
#[test]
fn a_torn_or_garbage_part_tail_is_ignored_on_recovery() {
    let header = |len: u32| {
        let mut t = Vec::new();
        t.extend_from_slice(&300u64.to_le_bytes());
        t.extend_from_slice(&len.to_le_bytes());
        t
    };
    let mut cut = header(64);
    cut.extend_from_slice(br#"{"timestamp":300,"lev"#);
    let mut absurd = header(u32::MAX);
    absurd.extend_from_slice(b"junk");
    let tails: Vec<(&str, Vec<u8>)> = vec![
        ("partial header", vec![0xAB; 5]),
        ("header only, payload missing", header(64)),
        ("payload cut mid-way", cut),
        ("absurd length prefix", absurd),
        ("zeroed tail of a grown file", vec![0u8; 4096]),
        (
            "random garbage",
            (0..777u32).map(|i| (i * 31 + 7) as u8).collect(),
        ),
    ];

    for (what, tail) in tails {
        let dir = TempDir::new();
        seed_crashed_session(&dir.path, 1);
        let log = dir.path.join("parts/1/0/log.part");
        let mut bytes = std::fs::read(&log).unwrap();
        bytes.extend_from_slice(&tail);
        std::fs::write(&log, &bytes).unwrap();

        let transport = Arc::new(MockTransport::default());
        let recorder =
            Recorder::launch(RecorderConfig::new(&dir.path, "TOKEN"), transport.clone()).unwrap();
        assert!(recorder.flush(Duration::from_secs(5)), "{what}");

        let bundles = transport.uploaded_bundles.lock().unwrap();
        assert_eq!(bundles.len(), 1, "{what}: the session was still delivered");
        let mut zip = ZipArchive::new(Cursor::new(bundles[0].clone())).unwrap();
        let log_name = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().to_string())
            .find(|n| n.ends_with(".log.json"))
            .unwrap_or_else(|| panic!("{what}: no log file"));
        let mut raw = Vec::new();
        zip.by_name(&log_name)
            .unwrap()
            .read_to_end(&mut raw)
            .unwrap();
        let log: Value = serde_json::from_slice(&raw).unwrap();
        let msgs: Vec<&str> = log["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["message"].as_str().unwrap())
            .collect();
        assert_eq!(msgs, vec!["pre-crash line A", "pre-crash line B"], "{what}");
        drop(bundles);
        drop(recorder);
    }
}

fn request_and_manifest(bundle: &[u8]) -> (Value, Value) {
    let mut zip = ZipArchive::new(Cursor::new(bundle.to_vec())).unwrap();
    let mut read = |name: &str| {
        let mut b = Vec::new();
        zip.by_name(name).unwrap().read_to_end(&mut b).unwrap();
        serde_json::from_slice::<Value>(&b).unwrap()
    };
    (read("request.json"), read("manifest.json"))
}

#[test]
fn a_scope_that_cannot_be_persisted_degrades_the_recorder_and_drops_the_stale_file() {
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    recorder.with_scope(|s| s.set_email(Some("a@example.com".into())));
    assert!(scope_file.exists());
    assert!(!recorder.is_degraded(), "a healthy write is not degraded");

    // Over the persistable cap: the old identity must not outlive the change
    // (a crash would report the wrong user), and the loss is surfaced.
    recorder.with_scope(|s| {
        s.set_email(Some("b@example.com".into()));
        s.set_attribute("big", Value::from("x".repeat(2 << 20)));
    });
    assert!(!scope_file.exists(), "the stale identity is not kept");
    assert!(recorder.is_degraded(), "the lost scope is surfaced");

    // It is not re-staged on every call while it stays hopeless.
    recorder.with_scope(|s| s.labels.push("l".into()));
    assert!(!scope_file.exists());
    // Once it fits again, persistence resumes.
    recorder.with_scope(|s| {
        s.attributes.remove("big");
    });
    assert!(scope_file.exists());
    drop(recorder);
}

#[test]
fn a_failed_additive_scope_write_keeps_the_identity_on_disk() {
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    recorder.with_scope(|s| s.set_email(Some("user@example.com".into())));
    // The next change (the email is untouched) cannot be persisted.
    recorder.with_scope(|s| s.set_attribute("big", Value::from("x".repeat(2 << 20))));
    assert!(recorder.is_degraded(), "the lost write is surfaced");
    let kept = std::fs::read(&scope_file).expect("the identity written earlier survives");
    let kept: Value = serde_json::from_slice(&kept).unwrap();
    assert_eq!(kept["email"], "user@example.com");
    assert!(kept["attributes"].get("big").is_none());
    drop(recorder);
}

#[cfg(unix)]
#[test]
fn a_transient_write_failure_that_kept_the_old_file_is_retried_on_the_next_call() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    let parts = scope_file.parent().unwrap().to_path_buf();
    recorder.with_scope(|s| s.set_email(Some("user@example.com".into())));

    // A read-only session directory: the temp file cannot be written, so the
    // old file (same email) is kept while the in-memory scope moves on.
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o555)).unwrap();
    recorder.with_scope(|s| s.labels.push("beta".into()));
    let writable = std::fs::write(parts.join("probe"), b"").is_ok(); // running as root
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o755)).unwrap();
    if writable {
        return;
    }
    assert!(recorder.is_degraded());
    let lagging: Value = serde_json::from_slice(&std::fs::read(&scope_file).unwrap()).unwrap();
    assert_eq!(
        lagging["labels"],
        serde_json::json!([]),
        "the kept file lags"
    );

    // The host sets the very same value again (no change). While the failure is
    // fresh the retry backs off, so a bad disk does not cost a write per call.
    let same = |recorder: &Recorder| {
        recorder.with_scope(|s| {
            if !s.labels.contains(&"beta".to_string()) {
                s.labels.push("beta".into());
            }
        })
    };
    same(&recorder);
    let still: Value = serde_json::from_slice(&std::fs::read(&scope_file).unwrap()).unwrap();
    assert_eq!(still["labels"], serde_json::json!([]), "retry backs off");

    // After the back-off the lagging file is brought up to date.
    std::thread::sleep(Duration::from_millis(1200));
    same(&recorder);
    let healed: Value = serde_json::from_slice(&std::fs::read(&scope_file).unwrap()).unwrap();
    assert_eq!(healed["labels"], serde_json::json!(["beta"]));
    drop(recorder);
}

#[test]
fn a_failed_write_that_changes_an_attribute_never_keeps_the_old_value() {
    // Hosts that carry identity in an attribute (no email) must not be reported
    // as the previous user when the write that changed it fails.
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    recorder.with_scope(|s| s.set_attribute("user_id", Value::from("A")));
    assert!(scope_file.exists());
    recorder.with_scope(|s| {
        s.set_attribute("user_id", Value::from("B"));
        s.set_attribute("big", Value::from("x".repeat(2 << 20)));
    });
    assert!(recorder.is_degraded());
    assert!(
        !scope_file.exists(),
        "user A must not outlive the change to B"
    );
    drop(recorder);
}

#[cfg(unix)]
#[test]
fn an_identity_change_whose_old_file_cannot_be_removed_is_never_left_as_the_old_user() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    let parts = scope_file.parent().unwrap().to_path_buf();
    recorder.with_scope(|s| s.set_email(Some("a@example.com".into())));

    // Read-only directory: neither the new write nor the unlink of the old file
    // can happen, so `scope.json` still names user A while the session is B.
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o555)).unwrap();
    recorder.with_scope(|s| s.set_email(Some("b@example.com".into())));
    let root = std::fs::write(parts.join("probe"), b"").is_ok();
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o755)).unwrap();
    if root {
        return;
    }
    assert!(recorder.is_degraded());

    // The disk is writable again; the host repeats the same email (no change).
    // After the back-off the stale identity must be replaced, never kept.
    std::thread::sleep(Duration::from_millis(1200));
    recorder.with_scope(|s| s.set_email(Some("b@example.com".into())));
    let now: Value = serde_json::from_slice(&std::fs::read(&scope_file).unwrap()).unwrap();
    assert_eq!(now["email"], "b@example.com", "never the previous user");
    drop(recorder);
}

#[cfg(unix)]
#[test]
fn a_logout_whose_old_file_cannot_be_removed_is_retried_until_it_is_gone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    let parts = scope_file.parent().unwrap().to_path_buf();
    recorder.with_scope(|s| s.set_email(Some("a@example.com".into())));
    assert!(scope_file.exists());

    // Logout while the directory is read-only: the unlink fails and the file
    // still names user A although the scope is now empty.
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o555)).unwrap();
    recorder.with_scope(|s| s.set_email(None));
    let root = std::fs::write(parts.join("probe"), b"").is_ok();
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o755)).unwrap();
    if root {
        return;
    }
    assert!(scope_file.exists(), "the unlink really failed");

    // Writable again; the host logs out again (an empty scope, unchanged).
    std::thread::sleep(Duration::from_millis(1200));
    recorder.with_scope(|s| s.set_email(None));
    assert!(
        !scope_file.exists(),
        "the stale identity is removed, not kept"
    );
    drop(recorder);
}

/// A recorder whose `scope.json` slot holds a directory, so every rename onto it
/// (and every unlink of it) fails while temp files still write: the state of a
/// destination held open by a scanner. Returns the recorder and the scope path.
#[cfg(unix)]
fn recorder_with_unreplaceable_scope_file(dir: &TempDir) -> (Recorder, PathBuf) {
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    recorder.with_scope(|s| s.set_email(Some("a@example.com".into())));
    std::fs::remove_file(&scope_file).unwrap();
    std::fs::create_dir(&scope_file).unwrap();
    (recorder, scope_file)
}

#[cfg(unix)]
#[test]
fn a_kept_temp_file_survives_a_later_write_that_stages_nothing_if_it_only_lacks_things() {
    use bugsee_core::model::scope::Scope;
    let dir = TempDir::new();
    let (recorder, scope_file) = recorder_with_unreplaceable_scope_file(&dir);
    // B is fully written to a temp file but cannot be renamed into place.
    recorder.with_scope(|s| s.set_email(Some("b@example.com".into())));
    assert_eq!(
        Scope::load_recovering(&scope_file)
            .unwrap()
            .email
            .as_deref(),
        Some("b@example.com")
    );
    // The next change only adds to B but cannot be staged (over the cap): the
    // kept temp still lacks only that, so it must not be swept away.
    recorder.with_scope(|s| s.set_attribute("big", Value::from("x".repeat(2 << 20))));
    assert_eq!(
        Scope::load_recovering(&scope_file)
            .unwrap()
            .email
            .as_deref(),
        Some("b@example.com"),
        "the newest complete snapshot survives"
    );
    drop(recorder);
}

#[cfg(unix)]
#[test]
fn a_kept_temp_file_is_dropped_once_the_old_file_is_gone_and_the_identity_moved_on() {
    use bugsee_core::model::scope::Scope;
    let dir = TempDir::new();
    let (recorder, scope_file) = recorder_with_unreplaceable_scope_file(&dir);
    recorder.with_scope(|s| s.set_email(Some("b@example.com".into())));
    // While the old file lingers the kept temp is what recovery prefers.
    assert_eq!(
        Scope::load_recovering(&scope_file)
            .unwrap()
            .email
            .as_deref(),
        Some("b@example.com")
    );
    // The handle on the old file goes away; then the identity moves on to C but
    // C cannot be staged. The kept temp now names someone else and nothing is
    // left to fall back to, so reporting less beats reporting B.
    std::fs::remove_dir(&scope_file).unwrap();
    recorder.with_scope(|s| {
        s.set_email(Some("c@example.com".into()));
        s.set_attribute("big", Value::from("x".repeat(2 << 20)));
    });
    assert!(
        Scope::load_recovering(&scope_file).is_none(),
        "no stale identity is left to recover"
    );
    drop(recorder);
}

#[cfg(unix)]
#[test]
fn a_too_large_identity_change_whose_old_file_cannot_be_removed_is_retried() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let recorder = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = recorder.crash_info_path().with_file_name("scope.json");
    let parts = scope_file.parent().unwrap().to_path_buf();
    recorder.with_scope(|s| s.set_email(Some("a@example.com".into())));

    // A to B, with an attribute over the cap, while the directory is read-only:
    // nothing can be staged and the old file cannot be unlinked.
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o555)).unwrap();
    recorder.with_scope(|s| {
        s.set_email(Some("b@example.com".into()));
        s.set_attribute("big", Value::from("x".repeat(2 << 20)));
    });
    let root = std::fs::write(parts.join("probe"), b"").is_ok();
    std::fs::set_permissions(&parts, std::fs::Permissions::from_mode(0o755)).unwrap();
    if root {
        return;
    }
    assert!(scope_file.exists(), "the unlink really failed");

    // Writable again; the host repeats B (unchanged, still over the cap). The
    // stale identity must still go, even though the scope itself cannot persist.
    std::thread::sleep(Duration::from_millis(1200));
    recorder.with_scope(|s| s.set_email(Some("b@example.com".into())));
    assert!(!scope_file.exists(), "user A is not left on disk");
    drop(recorder);
}

#[test]
fn a_session_persists_its_scope_and_a_crash_recovers_it() {
    // Run 1: scope lands in the session directory as it changes.
    let dir = TempDir::new();
    let first = Recorder::launch(
        RecorderConfig::new(&dir.path, "TOKEN"),
        Arc::new(MockTransport::default()),
    )
    .unwrap();
    let scope_file = first.crash_info_path().with_file_name("scope.json");
    assert!(!scope_file.exists(), "nothing to persist yet");
    first.with_scope(|s| {
        s.set_email(Some("crashed@example.com".into()));
        s.labels.push("beta".into());
        s.set_attribute("plan", Value::from("pro"));
    });
    assert!(scope_file.exists(), "scope persisted on change");
    let scope_bytes = std::fs::read(&scope_file).unwrap();
    // A lost file is restored by the next call even if the value is unchanged.
    std::fs::remove_file(&scope_file).unwrap();
    first.with_scope(|s| s.set_email(Some("crashed@example.com".into())));
    assert!(scope_file.exists(), "a missing scope file is restored");
    first.with_scope(|s| s.set_attribute("later", Value::from(1)));
    assert!(scope_file.exists());
    assert_ne!(
        std::fs::read(&scope_file).unwrap(),
        scope_bytes,
        "rewritten on change"
    );
    first.with_scope(|s| {
        s.set_email(None);
        s.labels.clear();
        s.clear_all_attributes();
    });
    assert!(!scope_file.exists(), "an empty scope leaves no file");
    drop(first);

    // The crashed session, as a later launch finds it.
    let dir2 = TempDir::new();
    seed_crashed_session(&dir2.path, 1);
    std::fs::write(dir2.path.join("parts/1/scope.json"), scope_bytes).unwrap();

    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir2.path, "TOKEN"), transport.clone()).unwrap();
    // This run's own scope must not leak into the recovered crash.
    recorder.with_scope(|s| {
        s.set_email(Some("new-run@example.com".into()));
        s.labels.push("new-run-label".into());
        s.set_attribute("new_run_attr", Value::from(true));
    });
    assert!(recorder.flush(Duration::from_secs(5)));

    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let (req, manifest) = request_and_manifest(&bundles[0]);
    assert_eq!(req["email"], "crashed@example.com");
    assert_eq!(req["labels"], serde_json::json!(["beta"]));
    assert_eq!(manifest["attrs"]["plan"], "pro");
    assert!(manifest["attrs"].get("new_run_attr").is_none());
    drop(bundles);
    assert!(!dir2.path.join("parts/1").exists());
    drop(recorder);
}

/// Launch over a crashed session whose `parts/1` holds `files`, and return the
/// recovered report's request body.
fn recover_with_scope_files(files: &[(&str, &[u8])]) -> Value {
    let dir = TempDir::new();
    seed_crashed_session(&dir.path, 1);
    for (name, bytes) in files {
        std::fs::write(dir.path.join("parts/1").join(name), bytes).unwrap();
    }
    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "TOKEN"), transport.clone()).unwrap();
    assert!(recorder.flush(Duration::from_secs(5)));
    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let (req, _) = request_and_manifest(&bundles[0]);
    drop(bundles);
    assert!(
        !dir.path.join("parts/1").exists(),
        "evidence discarded once queued"
    );
    drop(recorder);
    req
}

#[test]
fn a_crash_mid_scope_write_recovers_the_newest_complete_scope() {
    // Died after the temp file was written, before the rename: `scope.json` is
    // one change behind and the temp file carries the identity the user had.
    let old = br#"{"email":"old@example.com","labels":[],"attributes":{}}"#;
    let new = br#"{"email":"new@example.com","labels":["beta"],"attributes":{}}"#;
    let req = recover_with_scope_files(&[("scope.json", old), ("scope.json.5.tmp", new)]);
    assert_eq!(req["email"], "new@example.com");
    assert_eq!(req["labels"], serde_json::json!(["beta"]));
}

#[test]
fn a_torn_scope_temp_file_falls_back_to_the_previous_scope() {
    let old = br#"{"email":"old@example.com","labels":[],"attributes":{}}"#;
    let torn = br#"{"email":"new@exam"#;
    let req = recover_with_scope_files(&[("scope.json", old), ("scope.json.5.tmp", torn)]);
    assert_eq!(req["email"], "old@example.com");
}

#[test]
fn a_crash_before_the_first_scope_rename_still_recovers_it() {
    let only = br#"{"email":"first@example.com","labels":[],"attributes":{}}"#;
    let req = recover_with_scope_files(&[("scope.json.1.tmp", only)]);
    assert_eq!(req["email"], "first@example.com");
}

#[test]
fn a_corrupt_persisted_scope_does_not_stop_recovery() {
    let dir = TempDir::new();
    seed_crashed_session(&dir.path, 1);
    std::fs::write(dir.path.join("parts/1/scope.json"), b"{torn").unwrap();
    let transport = Arc::new(MockTransport::default());
    let recorder =
        Recorder::launch(RecorderConfig::new(&dir.path, "TOKEN"), transport.clone()).unwrap();
    assert!(recorder.flush(Duration::from_secs(5)));
    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 1);
    let (req, _) = request_and_manifest(&bundles[0]);
    assert!(req["email"].is_null());
    drop(bundles);
    drop(recorder);
}

//
//  native_recovery_e2e.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The join between the two halves of native crash reporting, on every platform.
//!
//! Each half was already covered and neither proved the pair:
//!   * `bugsee-native/tests/crash_subprocess.rs` — a real crash writes a marker.
//!   * `bugsee-core/tests/recovery_e2e.rs` — recovery turns a marker into a
//!     report, but from markers the test itself hand-writes.
//!
//! So a platform whose handler wrote a marker recovery could not use, or whose
//! module map came out empty, passed both suites. That is not hypothetical: the
//! Windows module map and marker writer were written blind against a machine
//! that cannot run them, and "the fields are present" is a far weaker claim than
//! "a real crash produces a usable report".
//!
//! This drives the whole client path in-process — install, crash for real,
//! relaunch, recover, deliver — and asserts on the delivered `crash.json`. No
//! network, no credentials, no deployment, so it runs in the ordinary matrix
//! wherever native capture is implemented.

#![cfg(all(feature = "native", any(unix, windows)))]

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};
use serde_json::Value;
use zip::ZipArchive;

const CHILD_ENV: &str = "BUGSEE_NATIVE_E2E_CHILD";
const DIR_ENV: &str = "BUGSEE_NATIVE_E2E_DIR";

/// The child half: a real SDK launch with native capture, then a real fault.
fn run_as_child_if_requested() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let dir = std::env::var(DIR_ENV).expect("data dir");

    let _guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&dir)
            .native_crash_capture(true)
            // The child must not try to deliver anything; the parent is what
            // recovers and delivers.
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch");

    // Let the recorder establish its generation and liveness marker before the
    // fault, so the parent sees a session that ended abnormally.
    std::thread::sleep(Duration::from_millis(300));

    unsafe {
        let p: *mut u8 = std::ptr::null_mut();
        std::ptr::write_volatile(p, 1);
    }
    // Unreachable; if the platform failed to fault, exit cleanly so the parent's
    // "died abnormally" assertion reports that rather than timing out.
    std::process::exit(0);
}

fn crash_json_from(bundle: &[u8]) -> Value {
    let mut zip = ZipArchive::new(Cursor::new(bundle)).expect("bundle is a zip");
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).expect("zip entry");
        if entry.name() == "crash.json" {
            let mut raw = Vec::new();
            entry.read_to_end(&mut raw).expect("read crash.json");
            return serde_json::from_slice(&raw).expect("crash.json parses");
        }
    }
    panic!("no crash.json in the delivered bundle");
}

#[test]
fn a_real_native_crash_is_recovered_into_a_deliverable_report() {
    run_as_child_if_requested();

    let dir = std::env::temp_dir().join(format!(
        "bugsee-native-e2e-{}",
        bugsee::core::util::random_hex(8)
    ));
    std::fs::create_dir_all(&dir).expect("create data dir");

    // --- crash for real, in a child process -------------------------------
    let exe: PathBuf = std::env::current_exe().expect("test exe");
    let output = std::process::Command::new(exe)
        .env(CHILD_ENV, "1")
        .env(DIR_ENV, &dir)
        .output()
        .expect("spawn child");

    assert!(
        !output.status.success(),
        "child should have died from the fault; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // --- relaunch: next-launch recovery turns the marker into a report -----
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&dir)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");

    // Recovery runs on the worker thread; flush waits for the queue to drain.
    std::thread::sleep(Duration::from_millis(1500));
    Bugsee::flush(Duration::from_secs(10));

    let bundles = mock.uploaded_bundles.lock().unwrap().clone();
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        bundles.len(),
        1,
        "the crashed session should have produced exactly one report"
    );
    let crash = crash_json_from(&bundles[0]);

    // --- the report has to be USABLE, not merely present ------------------

    assert_eq!(
        crash["exception_type"], "native",
        "recovered as a native crash: {crash}"
    );
    assert_eq!(
        crash["signal"]["name"], "SIGSEGV",
        "a null write is a segfault on every platform we build for: {}",
        crash["signal"]
    );

    // The module map is what makes the crash symbolicatable at all. An empty
    // one, or one without code ids, means every symbol upload resolves nothing
    // — the exact failure that made native crashes useless before `code_id`.
    let modules = crash["modules"].as_array().expect("modules[]");
    assert!(!modules.is_empty(), "module map is empty");
    let with_code_id = modules
        .iter()
        .filter(|m| m["code_id"].as_str().is_some_and(|s| !s.is_empty()))
        .count();
    assert!(
        with_code_id > 0,
        "no module carries a code_id, so nothing can ever be symbolicated"
    );

    // Frames are module-relative; recovery derives `reladdr` by joining the
    // crash-time PCs against the module map, so a frame proves both halves
    // agreed on the module bases.
    let frames = crash["frames"].as_array().expect("frames[]");
    assert!(!frames.is_empty(), "no frames captured");
    assert!(
        frames
            .iter()
            .any(|f| f["module"].as_str().is_some_and(|s| !s.is_empty())),
        "no frame resolved to a module: {frames:?}"
    );

    // The client-side dedup signature is what groups a crash-on-launch loop
    // before the server ever sees it.
    assert!(
        crash["signatures"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "no client signature: a crash loop could not be suppressed locally"
    );
}

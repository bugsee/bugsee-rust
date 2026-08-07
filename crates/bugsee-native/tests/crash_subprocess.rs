//
//  crash_subprocess.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! A real fatal-crash test: a child process installs the native handler and
//! dereferences null; the parent asserts the crash-info marker was written and
//! the child died from the signal.
//!
//! **Unix only, and that gate is the point.** On Windows `write_marker` and
//! `append_frames` are still no-op stubs (`bugsee-native/src/lib.rs`), so an
//! access violation produces no marker at all — the handler installs, the
//! process dies, and nothing is recorded. This test would fail there for a real
//! reason, not an environmental one.
//!
//! **Deleting `#![cfg(unix)]` is the acceptance criterion for the Windows
//! native work.** The assertions below need no changes to be meaningful there:
//! the marker format is shared, and a Windows `EXCEPTION_ACCESS_VIOLATION` is
//! expected to map to `signal=11` exactly as `EXC_BAD_ACCESS` does on Apple.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

const CHILD_ENV: &str = "BUGSEE_CRASH_CHILD";
const PATH_ENV: &str = "BUGSEE_CRASH_MARKER";

/// The child half: install the handler, then crash. Runs only when re-invoked
/// with the child env var set.
fn run_as_child_if_requested() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let marker = std::env::var(PATH_ENV).expect("marker path");
    let _handler = bugsee_native::install(PathBuf::from(marker)).expect("install handler");

    // Deliberate null dereference → SIGSEGV / EXC_BAD_ACCESS.
    let p = std::ptr::null_mut::<u8>();
    unsafe {
        std::ptr::write_volatile(p, 1);
    }
    // Should never get here.
    std::process::exit(0);
}

#[test]
fn native_handler_writes_marker_on_segfault() {
    run_as_child_if_requested();

    // Parent: prepare a marker path and re-invoke this test as the crashing child.
    let mut dir = std::env::temp_dir();
    dir.push(format!("bugsee-native-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("crash.info");

    let exe = std::env::current_exe().unwrap();
    let output = Command::new(exe)
        .args([
            "native_handler_writes_marker_on_segfault",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env(PATH_ENV, &marker)
        .output()
        .expect("spawn child");

    // The child must have died from the crash, not exited cleanly.
    assert!(
        !output.status.success(),
        "child should have crashed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(marker.exists(), "crash-info marker written by the handler");
    let content = std::fs::read_to_string(&marker).unwrap();
    assert!(
        content.contains("signal=11"),
        "SIGSEGV recorded: {content:?}"
    );
    assert!(
        content.contains("address=0x"),
        "fault address recorded: {content:?}"
    );
    // The crashing thread's frames were captured (at least the faulting PC) for
    // the client-side native dedup signature.
    assert!(
        content.contains("frame=0x"),
        "crashing-thread frames captured: {content:?}"
    );

    // The module map was snapshotted at install time (base<TAB>name entries).
    let modules = dir.join("crash.modules");
    assert!(modules.exists(), "module map written at install");
    let mods = std::fs::read_to_string(&modules).unwrap();
    assert!(
        mods.lines().next().is_some_and(|l| l.contains('\t')),
        "module map has base\\tname rows: {mods:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

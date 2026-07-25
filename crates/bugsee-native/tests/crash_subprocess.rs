//
//  crash_subprocess.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! A real fatal-crash test: a child process installs the native handler and
//! dereferences null; the parent asserts the crash-info marker was written and
//! the child died from the signal.

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

    let _ = std::fs::remove_dir_all(&dir);
}

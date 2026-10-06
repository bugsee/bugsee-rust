//
//  crash_windows.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Windows fatal-crash cases beyond the plain access violation covered by
//! `crash_subprocess.rs`: a real stack overflow (the handler runs with almost no
//! stack) and a wedged handler (the watchdog must end the process).
//!
//! Each case runs in a child process that re-enters the same test function.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

const KIND_ENV: &str = "BUGSEE_CRASH_KIND";
const PATH_ENV: &str = "BUGSEE_CRASH_MARKER";

#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(n: u64) -> u64 {
    let pad = [n as u8; 256];
    std::hint::black_box(&pad);
    recurse(n + 1) + pad[0] as u64
}

#[inline(never)]
fn fault_null() {
    // SAFETY: deliberately faulting.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) }
}

fn run_as_child_if_requested() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let marker = std::env::var(PATH_ENV).expect("marker path");
    let _handler = bugsee_native::install(PathBuf::from(marker)).expect("install handler");
    match kind.as_str() {
        "stack_overflow" => {
            recurse(0);
        }
        "stack_overflow_thread" => {
            std::thread::spawn(|| {
                recurse(0);
            })
            .join()
            .unwrap();
        }
        "wedged_handler" => {
            bugsee_native::wedge_handler_for_test();
            fault_null();
        }
        other => panic!("unknown crash kind {other}"),
    }
    std::process::exit(0);
}

/// Run `cmd` to completion, killing it if it outlives `limit`, so a regression
/// that hangs the child fails fast instead of stalling the job.
fn output_with_timeout(mut cmd: Command, limit: std::time::Duration) -> std::process::Output {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn child");
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let started = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not exit within {limit:?}; killed");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    std::process::Output {
        status,
        stdout: t_out.join().unwrap_or_default(),
        stderr: t_err.join().unwrap_or_default(),
    }
}

fn spawn_child(kind: &str) -> (std::process::Output, String) {
    let dir = std::env::temp_dir().join(format!("bugsee-win-{}-{kind}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("crash.info");
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["windows_crashes", "--exact", "--nocapture"])
        .env(KIND_ENV, kind)
        .env(PATH_ENV, &marker);
    let output = output_with_timeout(cmd, std::time::Duration::from_secs(60));
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (output, written)
}

#[test]
fn windows_crashes() {
    run_as_child_if_requested();

    for kind in ["stack_overflow", "stack_overflow_thread"] {
        let (output, marker) = spawn_child(kind);
        assert!(!output.status.success(), "[{kind}] child must die");
        // A stack overflow is reported as a memory fault, like the other
        // platforms' SIGSEGV.
        assert!(
            marker.contains("signal=11"),
            "[{kind}] no marker survived a stack overflow: {marker:?}"
        );
        assert!(
            marker.contains("frame=0x"),
            "[{kind}] no frames recorded: {marker:?}"
        );
    }

    // A handler that never returns must be ended by the watchdog.
    let started = std::time::Instant::now();
    let (output, _) = spawn_child("wedged_handler");
    assert_eq!(
        output.status.code().map(|c| c as u32),
        Some(bugsee_native::WATCHDOG_EXIT_CODE),
        "a wedged handler must be killed by the watchdog: {:?}",
        output.status
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

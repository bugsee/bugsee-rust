//
//  origin_fork.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! A fork landing mid-capture must not leave the child with a locked
//! `backtrace` mutex: its next stack walk would hang forever.

#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bugsee_core::origin::capture_origin;

fn wait_with_deadline(pid: libc::pid_t, deadline: Duration) -> i32 {
    let start = Instant::now();
    loop {
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
            return status;
        }
        if start.elapsed() > deadline {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let mut s = 0;
            unsafe { libc::waitpid(pid, &mut s, 0) };
            panic!("child {pid} hung (backtrace mutex inherited locked?)");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn forking_during_a_capture_leaves_the_child_able_to_capture() {
    bugsee_core::fork::install_fork_hook();
    let stop = Arc::new(AtomicBool::new(false));
    let hammer = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = capture_origin();
            }
        })
    };

    for i in 0..25 {
        // SAFETY: the child only walks its stack and `_exit`s.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            let _ = capture_origin();
            unsafe { libc::_exit(0) };
        }
        let status = wait_with_deadline(pid, Duration::from_secs(30));
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "child {i} died (status {status})"
        );
    }

    stop.store(true, Ordering::Relaxed);
    hammer.join().unwrap();
}

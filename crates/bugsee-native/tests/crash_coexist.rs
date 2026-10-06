//
//  crash_coexist.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Coexistence with hosts that fault on purpose (a managed runtime's null checks,
//! a JIT's guard pages) and recover. A fault the host handled must leave NO crash
//! marker and must not uninstall the SDK; a real fatal fault afterwards must
//! still be reported.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;

const KIND_ENV: &str = "BUGSEE_COEXIST_KIND";
const PATH_ENV: &str = "BUGSEE_COEXIST_MARKER";

static PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The "runtime": makes the faulting page accessible and lets the instruction retry.
extern "C" fn recovering_host(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: async-signal-safe calls on a page this process mapped.
    unsafe {
        let addr = (*info).si_addr() as usize & !4095;
        if addr == PAGE.load(std::sync::atomic::Ordering::SeqCst) {
            libc::mprotect(addr as *mut _, 4096, libc::PROT_READ | libc::PROT_WRITE);
        } else {
            // Not ours: behave like a runtime that does not own the fault.
            let mut dfl: libc::sigaction = std::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGSEGV, &dfl, std::ptr::null_mut());
            libc::sigaction(libc::SIGBUS, &dfl, std::ptr::null_mut());
        }
    }
}

/// A runtime that swallows a *sent* signal and returns, but does not own real
/// faults (returning from those without fixing anything would loop forever).
extern "C" fn swallowing_host(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: async-signal-safe calls only.
    unsafe {
        // A fault has a small positive code (1..=15); a *sent* signal is <= 0 on
        // Linux and a large positive code on macOS (observed 0x200).
        let code = (*info).si_code;
        // Visible in the test output: what the OS reports as `si_code` here is
        // exactly what the SDK's sent-vs-fault decision hinges on.
        let mut buf = *b"HOST_SAW_CODE=0x00000000\n";
        for i in 0..8 {
            let nib = ((code as u32) >> (28 - 4 * i)) & 0xF;
            buf[14 + i] = b"0123456789abcdef"[nib as usize];
        }
        libc::write(1, buf.as_ptr().cast(), buf.len());
        if (1..=15).contains(&code) {
            let mut dfl: libc::sigaction = std::mem::zeroed();
            dfl.sa_sigaction = libc::SIG_DFL;
            libc::sigaction(libc::SIGSEGV, &dfl, std::ptr::null_mut());
            libc::sigaction(libc::SIGBUS, &dfl, std::ptr::null_mut());
        }
    }
}

fn install_host(sig: libc::c_int, siginfo: bool) {
    // SAFETY: zeroed + filled sigaction.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = if siginfo {
            recovering_host as *const () as usize
        } else {
            swallowing_host as *const () as usize
        };
        sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
        // A protection fault is SIGBUS on some platforms (macOS).
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
    }
}

fn guarded_page() -> *mut u8 {
    // SAFETY: anonymous mapping.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        assert_ne!(p, libc::MAP_FAILED);
        PAGE.store(p as usize, std::sync::atomic::Ordering::SeqCst);
        p as *mut u8
    }
}

fn say(msg: &str) {
    println!("{msg}");
}

fn child() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let marker = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    match kind.as_str() {
        // Host handler installed BEFORE the SDK; faults repeatedly, then crashes for real.
        "recovered_then_fatal" => {
            install_host(libc::SIGSEGV, true);
            let _h = bugsee_native::install(marker.clone()).unwrap();
            for round in 1..=3 {
                let p = guarded_page();
                // SAFETY: deliberate fault the host recovers from.
                unsafe { std::ptr::write_volatile(p, 7) };
                say(&format!(
                    "RECOVERED {round} marker_present={}",
                    marker.exists()
                ));
            }
            // Not the host's page: the host declines, so this one is real.
            // SAFETY: deliberate null write.
            unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        }
        // A signal that was only SENT, swallowed by the host's handler.
        "sent_and_swallowed" => {
            install_host(libc::SIGSEGV, false);
            let _h = bugsee_native::install(marker.clone()).unwrap();
            // SAFETY: raises a signal at ourselves.
            unsafe { libc::raise(libc::SIGSEGV) };
            say(&format!("SURVIVED marker_present={}", marker.exists()));
            // And the SDK is still armed afterwards.
            // SAFETY: deliberate null write.
            unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        }
        other => panic!("unknown kind {other}"),
    }
    std::process::exit(0);
}

fn run(kind: &str, test: &str) -> (std::process::Output, String) {
    let dir = std::env::temp_dir().join(format!("bugsee-coexist-{kind}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("crash.info");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env(KIND_ENV, kind)
        .env(PATH_ENV, &marker)
        .output()
        .unwrap();
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (out, written)
}

#[test]
fn a_fault_the_host_recovers_leaves_no_marker_and_the_sdk_stays_armed() {
    child();
    let (out, marker) = run(
        "recovered_then_fatal",
        "a_fault_the_host_recovers_leaves_no_marker_and_the_sdk_stays_armed",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for round in 1..=3 {
        assert!(
            stdout.contains(&format!("RECOVERED {round} marker_present=false")),
            "round {round}: a recovered fault must leave no marker; stdout={stdout}"
        );
    }
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(
        marker.contains("signal=11"),
        "real crash recorded: {marker:?}"
    );
    assert!(
        marker.contains("address=0x0\n"),
        "the REAL fault (null), not a recovered one: {marker:?}"
    );
}

#[test]
fn a_sent_signal_swallowed_by_the_host_leaves_no_marker() {
    child();
    let (out, marker) = run(
        "sent_and_swallowed",
        "a_sent_signal_swallowed_by_the_host_leaves_no_marker",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SURVIVED marker_present=false"),
        "stdout={stdout}"
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(marker.contains("address=0x0\n"), "{marker:?}");
}

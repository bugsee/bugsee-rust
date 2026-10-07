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

static SWALLOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A runtime that swallows ONE signal (the one the test sends) and returns, and
/// declines everything after (returning from a real fault without fixing it would
/// loop forever). It cannot use `si_code` to tell the two apart: macOS delivers a
/// sent `SIGSEGV` with a fault-like code.
extern "C" fn swallowing_host(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    if !SWALLOWED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    // SAFETY: async-signal-safe calls only.
    unsafe {
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(libc::SIGSEGV, &dfl, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &dfl, std::ptr::null_mut());
    }
}

/// A one-shot (`SA_RESETHAND`) crash logger: notes the fault and returns without
/// fixing anything. Run by the kernel, it is gone after the first fault and the
/// retriggered fault then kills the process; run by someone who forgets that, it
/// loops forever.
extern "C" fn log_and_return(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
    const MSG: &[u8] = b"HOST_LOGGED\n";
    // SAFETY: async-signal-safe.
    unsafe { libc::write(1, MSG.as_ptr().cast(), MSG.len()) };
}

fn sleep_ms(ms: i64) {
    let ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: ms * 1_000_000,
    };
    // SAFETY: nanosleep is async-signal-safe.
    unsafe { libc::nanosleep(&ts, std::ptr::null_mut()) };
}

fn decline() {
    // SAFETY: async-signal-safe calls only.
    unsafe {
        let mut dfl: libc::sigaction = std::mem::zeroed();
        dfl.sa_sigaction = libc::SIG_DFL;
        libc::sigaction(libc::SIGSEGV, &dfl, std::ptr::null_mut());
        libc::sigaction(libc::SIGBUS, &dfl, std::ptr::null_mut());
    }
}

/// A runtime that is slow to decide: it takes a while to recover its own page's
/// fault, and a long while to give up on anyone else's.
extern "C" fn slow_host(_: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: async-signal-safe calls on a page this process mapped.
    unsafe {
        let addr = (*info).si_addr() as usize & !4095;
        if addr == PAGE.load(std::sync::atomic::Ordering::SeqCst) {
            sleep_ms(150);
            libc::mprotect(addr as *mut _, 4096, libc::PROT_READ | libc::PROT_WRITE);
        } else {
            sleep_ms(600);
            decline();
        }
    }
}

/// What a host installed AFTER the SDK saw as the previous handler (the SDK's).
static CHAIN_PREV: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A well-behaved host installed on top of the SDK: recovers its own faults and
/// passes everything else down the chain.
extern "C" fn chaining_host(sig: libc::c_int, info: *mut libc::siginfo_t, uc: *mut libc::c_void) {
    // SAFETY: async-signal-safe calls; `CHAIN_PREV` holds the SA_SIGINFO handler
    // that was installed before this one.
    unsafe {
        let addr = (*info).si_addr() as usize & !4095;
        if addr == PAGE.load(std::sync::atomic::Ordering::SeqCst) {
            libc::mprotect(addr as *mut _, 4096, libc::PROT_READ | libc::PROT_WRITE);
        } else {
            let prev = CHAIN_PREV.load(std::sync::atomic::Ordering::SeqCst);
            let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                std::mem::transmute(prev);
            f(sig, info, uc);
        }
    }
}

fn install_host_with(
    handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void),
    extra_flags: libc::c_int,
) {
    // SAFETY: zeroed + filled sigaction.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as usize;
        sa.sa_flags = libc::SA_ONSTACK | libc::SA_SIGINFO | extra_flags;
        libc::sigemptyset(&mut sa.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGSEGV, &sa, &mut old);
        CHAIN_PREV.store(old.sa_sigaction, std::sync::atomic::Ordering::SeqCst);
        // A protection fault is SIGBUS on some platforms (macOS).
        libc::sigaction(libc::SIGBUS, &sa, std::ptr::null_mut());
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
            // On a failure, which invocation left it (its `code=` tells).
            if let Ok(left) = std::fs::read_to_string(&marker) {
                say(&format!("LEFTOVER_MARKER {}", left.replace('\n', " | ")));
            }
            // And the SDK is still armed afterwards.
            // SAFETY: deliberate null write.
            unsafe { std::ptr::write_volatile(std::ptr::null_mut::<u8>(), 1) };
        }
        // The thread already has a SMALL alternate stack (Rust gives its threads
        // one sized for its own tiny handler). Unwinding in our handler must not
        // overflow it, or the crash arrives without frames.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "small_altstack" => {
            // SAFETY: leaks an 8 KiB buffer as this thread's alternate stack.
            unsafe {
                let kb: usize = 8;
                let buf = Box::leak(vec![0u8; kb * 1024].into_boxed_slice());
                let st = libc::stack_t {
                    ss_sp: buf.as_mut_ptr().cast(),
                    ss_flags: 0,
                    ss_size: buf.len(),
                };
                assert_eq!(libc::sigaltstack(&st, std::ptr::null_mut()), 0);
            }
            let _h = bugsee_native::install(marker.clone()).unwrap();
            // SAFETY: deliberate null write.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
        }
        // Two threads fault at once: one the host recovers from slowly, one it
        // never will. The recovery must not delete the other thread's marker.
        "racing_threads" => {
            install_host_with(slow_host, 0);
            let _h = bugsee_native::install(marker.clone()).unwrap();
            let p = guarded_page() as usize;
            let recovered = std::thread::spawn(move || {
                // SAFETY: deliberate fault the host recovers from.
                unsafe { std::ptr::write_volatile(p as *mut u8, 7) };
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            // SAFETY: deliberate null write; nobody recovers it.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
            let _ = recovered.join();
        }
        // The same race the other way round: the fatal fault comes FIRST and the
        // one the host recovers from arrives while it is still being handled.
        "racing_fatal_first" => {
            install_host_with(slow_host, 0);
            let _h = bugsee_native::install(marker.clone()).unwrap();
            let p = guarded_page() as usize;
            let recovered = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // SAFETY: deliberate fault the host recovers from.
                unsafe { std::ptr::write_volatile(p as *mut u8, 7) };
            });
            // SAFETY: deliberate null write; nobody recovers it.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
            let _ = recovered.join();
        }
        // A host whose handler is one-shot (`SA_RESETHAND`): the kernel would reset
        // it before running it, and so must the SDK when it calls it.
        "one_shot_host" => {
            install_host_with(log_and_return, libc::SA_RESETHAND);
            let _h = bugsee_native::install(marker.clone()).unwrap();
            // SAFETY: deliberate null write; the host only logs it.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
        }
        // The host installs itself AFTER the SDK, recovers its own faults, and
        // passes every other fault down the chain.
        "host_after_sdk" => {
            let _h = bugsee_native::install(marker.clone()).unwrap();
            install_host_with(chaining_host, 0);
            for round in 1..=3 {
                let p = guarded_page();
                // SAFETY: deliberate fault the host recovers from.
                unsafe { std::ptr::write_volatile(p, 7) };
                say(&format!(
                    "RECOVERED {round} marker_present={}",
                    marker.exists()
                ));
            }
            // SAFETY: deliberate null write.
            unsafe {
                std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1)
            };
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "thread_altstack" => {
            let _h = bugsee_native::install(marker.clone()).unwrap();
            let kb: usize = 12;
            std::thread::spawn(move || {
                // SAFETY: leaks a buffer as this thread's alternate stack.
                unsafe {
                    let buf = Box::leak(vec![0u8; kb * 1024].into_boxed_slice());
                    let st = libc::stack_t {
                        ss_sp: buf.as_mut_ptr().cast(),
                        ss_flags: 0,
                        ss_size: buf.len(),
                    };
                    assert_eq!(libc::sigaltstack(&st, std::ptr::null_mut()), 0);
                    std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1);
                }
            })
            .join()
            .unwrap();
        }
        other => panic!("unknown kind {other}"),
    }
    std::process::exit(0);
}

fn run(kind: &str, test: &str) -> (std::process::Output, String) {
    let dir = std::env::temp_dir().join(format!("bugsee-coexist-{kind}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("crash.info");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture"])
        .env(KIND_ENV, kind)
        .env(PATH_ENV, &marker)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    // A hung child (a handler that retriggers into itself) must fail the test,
    // not hang the suite.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let out = match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(out) => out.unwrap(),
        Err(_) => {
            // SAFETY: kills the child we spawned.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("{kind}: the child hung");
        }
    };
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

// Linux/Android only: macOS refuses an alternate stack under 32 KiB, and its
// handler walks frame pointers from the signal context in a few reads anyway.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_small_existing_alternate_stack_still_yields_frames() {
    child();
    let (out, marker) = run(
        "small_altstack",
        "a_small_existing_alternate_stack_still_yields_frames",
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(marker.contains("signal=11"), "{marker:?}");
    assert!(
        marker.contains("frame=0x"),
        "the handler must not overflow a small alternate stack: {marker:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_thread_with_a_tiny_alternate_stack_still_yields_frames() {
    child();
    // Rust gives every thread an alternate stack sized for its own small handler.
    let (out, marker) = run(
        "thread_altstack",
        "a_thread_with_a_tiny_alternate_stack_still_yields_frames",
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(marker.contains("signal=11"), "{marker:?}");
    assert!(
        marker.contains("frame=0x"),
        "unwinding must fit a small thread alternate stack: {marker:?}"
    );
}

#[test]
fn recovering_one_threads_fault_does_not_delete_another_threads_marker() {
    child();
    let (out, marker) = run(
        "racing_threads",
        "recovering_one_threads_fault_does_not_delete_another_threads_marker",
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(
        marker.contains("address=0x0\n"),
        "the real crash's marker must survive the other thread's recovery: {marker:?}"
    );
    assert!(marker.contains("frame=0x"), "{marker:?}");
    // The interrupted PC is in the header AND is the first unwound frame: it must
    // be listed once.
    let frames: Vec<&str> = marker.lines().filter(|l| l.starts_with("frame=")).collect();
    assert!(frames.len() >= 2, "{marker:?}");
    assert_ne!(
        frames[0], frames[1],
        "the first frame is duplicated: {marker:?}"
    );
}

#[test]
fn a_fatal_fault_survives_a_later_fault_the_host_recovers() {
    child();
    let (out, marker) = run(
        "racing_fatal_first",
        "a_fatal_fault_survives_a_later_fault_the_host_recovers",
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(
        marker.contains("address=0x0\n"),
        "a later recovered fault must neither overwrite nor delete the fatal one's marker: {marker:?}"
    );
    assert!(marker.contains("frame=0x"), "{marker:?}");
}

#[test]
fn a_one_shot_logging_host_does_not_make_the_crash_loop() {
    child();
    let (out, marker) = run(
        "one_shot_host",
        "a_one_shot_logging_host_does_not_make_the_crash_loop",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("HOST_LOGGED").count(),
        1,
        "a one-shot handler runs once, as the kernel would have it: {stdout}"
    );
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(marker.contains("address=0x0\n"), "{marker:?}");
    assert!(marker.contains("frame=0x"), "{marker:?}");
}

#[test]
fn a_host_installed_after_the_sdk_recovers_quietly_and_passes_the_rest_down() {
    child();
    let (out, marker) = run(
        "host_after_sdk",
        "a_host_installed_after_the_sdk_recovers_quietly_and_passes_the_rest_down",
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for round in 1..=3 {
        assert!(
            stdout.contains(&format!("RECOVERED {round} marker_present=false")),
            "round {round}: stdout={stdout}"
        );
    }
    assert!(
        matches!(out.status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
        "{:?}",
        out.status
    );
    assert!(marker.contains("address=0x0\n"), "{marker:?}");
    assert!(
        marker.contains("frame=0x"),
        "a fault the host declines still gets a full report: {marker:?}"
    );
}

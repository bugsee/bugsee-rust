//
//  crash_matrix.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The fatal-crash matrix: every way a process realistically dies from a fault,
//! each in its own child process, each asserting the marker a next launch would
//! recover from.
//!
//! `crash_subprocess.rs` covers the plain null dereference on all platforms.
//! This file widens it to the cases that distinguish a robust handler from one
//! that merely works on the happy path — stack overflow (the handler runs with
//! almost no stack), a crash off the main thread, and the rest of the fatal
//! signals — and pins that the recorded stack STARTS AT THE CRASH SITE rather
//! than inside the handler.

#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;

const KIND_ENV: &str = "BUGSEE_CRASH_KIND";
const PATH_ENV: &str = "BUGSEE_CRASH_MARKER";

/// The function the deliberate fault happens in. Never inlined, so its address
/// bounds the crash site for the first-frame assertion.
#[inline(never)]
fn fault_null() {
    // SAFETY: deliberately faulting.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) }
}

/// A real illegal-instruction fault. (Not `raise(SIGILL)`: on Apple a signal
/// sent by the process never becomes a Mach exception, so the handler — which
/// is driven by Mach exceptions there — rightly never sees it.)
#[inline(never)]
fn fault_illegal_instruction() {
    // SAFETY: deliberately faulting.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("ud2");
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("udf #0");
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        libc::raise(libc::SIGILL);
    }
}

/// A real integer divide-by-zero. Only x86 traps on it; aarch64 returns 0.
#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn fault_divide_by_zero() {
    // SAFETY: deliberately faulting.
    unsafe {
        std::arch::asm!(
            "xor edx, edx",
            "xor ecx, ecx",
            "mov eax, 1",
            "div ecx",
            out("eax") _, out("ecx") _, out("edx") _,
        );
    }
}
#[cfg(not(target_arch = "x86_64"))]
fn fault_divide_by_zero() {
    unreachable!("no trapping integer divide on this architecture");
}

/// Unbounded recursion with a frame big enough to exhaust the stack quickly.
#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(n: u64) -> u64 {
    let pad = [n as u8; 256];
    std::hint::black_box(&pad);
    recurse(n + 1) + pad[0] as u64
}

/// `(address of the crash function, size to allow for it)` printed by the child
/// so the parent can check the first recorded frame lies inside it.
fn crash_fn(kind: &str) -> Option<usize> {
    match kind {
        "segv" | "segv_thread" => Some(fault_null as fn() as usize),
        "stack_overflow" | "stack_overflow_thread" => Some(recurse as fn(u64) -> u64 as usize),
        _ => None,
    }
}

fn run_as_child_if_requested() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let marker = std::env::var(PATH_ENV).expect("marker path");
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if kind == "host_handler" {
        install_host_segv_handler();
    }
    let _handler = bugsee_native::install(PathBuf::from(marker)).expect("install handler");
    if let Some(f) = crash_fn(&kind) {
        println!("CRASH_FN={f:#x}");
    }
    match kind.as_str() {
        "segv" => fault_null(),
        "segv_thread" => {
            std::thread::spawn(fault_null).join().unwrap();
        }
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
        "abort" => std::process::abort(),
        "fpe" => fault_divide_by_zero(),
        "ill" => fault_illegal_instruction(),
        "bus" => unsafe {
            // A REAL bus error: touch a mapped page that lies beyond the end of
            // its backing file. (A `raise(SIGBUS)` is not equivalent — the Rust
            // runtime's own SIGBUS handler consumes a signal that was sent
            // rather than faulted, and the process would simply carry on.)
            let path = std::ffi::CString::new(std::env::var(PATH_ENV).unwrap() + ".bus").unwrap();
            let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600);
            let p = libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert_ne!(p, libc::MAP_FAILED, "mmap");
            std::ptr::write_volatile(p as *mut u8, 1);
        },
        // The marker path is a FIFO nobody reads, so the handler's `open` blocks
        // forever: the watchdog must kill the process instead of leaving it hung.
        "hung_handler" => fault_null(),
        // A host crash handler installed BEFORE ours must still run afterwards.
        "host_handler" => fault_null(),
        other => panic!("unknown crash kind {other}"),
    }
    // A handled-and-returned signal must still not let the process carry on.
    std::process::exit(0);
}

/// Stand-in for a host application's (or another reporter's) own `SIGSEGV`
/// handler, installed before the SDK. It announces itself and exits 77.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn install_host_segv_handler() {
    extern "C" fn host(_: libc::c_int, _: *mut libc::siginfo_t, _: *mut libc::c_void) {
        const MSG: &[u8] = b"HOST_HANDLER_RAN\n";
        // SAFETY: async-signal-safe calls only.
        unsafe {
            libc::write(1, MSG.as_ptr().cast(), MSG.len());
            libc::_exit(77);
        }
    }
    // SAFETY: installing a handler with a zeroed, then filled-in, sigaction.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = host as *const () as usize;
        sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGSEGV, &sa, std::ptr::null_mut());
    }
}

struct Crashed {
    marker: String,
    stdout: String,
    status: std::process::ExitStatus,
}

fn spawn_child(kind: &str, marker: &std::path::Path) -> std::process::Output {
    Command::new(std::env::current_exe().unwrap())
        .args(["crash_matrix", "--exact", "--nocapture"])
        .env(KIND_ENV, kind)
        .env(PATH_ENV, marker)
        .output()
        .expect("spawn child")
}

fn scratch_dir(kind: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bugsee-matrix-{}-{kind}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn crash(kind: &str) -> Crashed {
    let dir = scratch_dir(kind);
    let marker = dir.join("crash.info");
    let output = spawn_child(kind, &marker);

    let c = Crashed {
        marker: std::fs::read_to_string(&marker).unwrap_or_default(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        status: output.status,
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        !c.status.success(),
        "[{kind}] child must die, not exit cleanly; stdout={:?}",
        c.stdout
    );
    c
}

fn frames(marker: &str) -> Vec<usize> {
    marker
        .lines()
        .flat_map(|l| l.split_whitespace())
        .filter_map(|t| t.strip_prefix("frame=0x"))
        .filter_map(|h| usize::from_str_radix(h, 16).ok())
        .collect()
}

fn recorded_signal(marker: &str) -> Option<i32> {
    marker
        .split_whitespace()
        .find_map(|t| t.strip_prefix("signal="))
        .and_then(|v| v.parse().ok())
}

fn crash_fn_from(stdout: &str) -> Option<usize> {
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("CRASH_FN=0x"))
        .and_then(|h| usize::from_str_radix(h.trim(), 16).ok())
}

/// One test, one table: the child re-enters this same function (see `crash`).
#[test]
fn crash_matrix() {
    run_as_child_if_requested();

    // (kind, signal the marker must record)
    // `mut` is only needed on targets where one of the cfg-gated pushes below exists.
    #[allow(unused_mut)]
    let mut cases: Vec<(&str, i32)> = vec![
        ("segv", libc::SIGSEGV),
        ("segv_thread", libc::SIGSEGV),
        ("stack_overflow", libc::SIGSEGV),
        ("stack_overflow_thread", libc::SIGSEGV),
        ("abort", libc::SIGABRT),
        ("ill", libc::SIGILL),
    ];
    // Only x86 has a trapping integer divide.
    #[cfg(target_arch = "x86_64")]
    cases.push(("fpe", libc::SIGFPE));
    // Apple reports every EXC_BAD_ACCESS as SIGSEGV (including the kernel's
    // bus-error flavour), so the real-SIGBUS case is only meaningful elsewhere.
    #[cfg(not(target_vendor = "apple"))]
    cases.push(("bus", libc::SIGBUS));

    for (kind, want_signal) in cases {
        let c = crash(kind);
        assert_eq!(
            recorded_signal(&c.marker),
            Some(want_signal),
            "[{kind}] wrong signal recorded: {:?}",
            c.marker
        );
        assert!(
            c.marker.contains("address=0x") && c.marker.contains("time="),
            "[{kind}] marker header incomplete: {:?}",
            c.marker
        );
        let fr = frames(&c.marker);
        assert!(!fr.is_empty(), "[{kind}] no frames: {:?}", c.marker);
        assert!(fr.len() <= 64, "[{kind}] frame cap exceeded: {}", fr.len());

        // The crash is a fatal signal to the OS too: the handler must let the
        // process die rather than swallow it. (Rust's own stack-overflow handler
        // converts that case to an abort, so only assert "a signal killed it".)
        assert!(
            c.status.signal().is_some(),
            "[{kind}] process was not terminated by a signal: {:?}",
            c.status
        );

        // The stack starts at the crash site, not the handler. Linux reads the
        // PC from the signal's ucontext, so it is exact there. A debug build
        // faults inside the `write_volatile` helper one call below the crash
        // function, so the function may be frame 0 or 1 — the handler's own
        // frames (7 deep) can never satisfy that.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Some(f) = crash_fn_from(&c.stdout) {
            assert!(
                fr.iter().take(2).any(|pc| (f..f + 512).contains(pc)),
                "[{kind}] crash function at {f:#x} is not in the first frames {:x?}; \
                 the handler's own frames leaked into the report",
                &fr[..fr.len().min(4)]
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let _ = crash_fn_from(&c.stdout);
    }
}

/// A handler that can block (here: on a FIFO with no reader, standing in for a
/// dead network mount) must not turn a crash into a hang. The watchdog kills the
/// process with `SIGALRM` once the handler's budget is spent.
#[test]
fn a_hung_handler_is_killed_instead_of_hanging() {
    let dir = scratch_dir("hung");
    let marker = dir.join("crash.info");
    let c_path = std::ffi::CString::new(marker.to_str().unwrap()).unwrap();
    // SAFETY: valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0, "mkfifo");

    let started = std::time::Instant::now();
    let output = spawn_child("hung_handler", &marker);
    let took = started.elapsed();
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        output.status.signal(),
        Some(libc::SIGALRM),
        "a wedged handler must be ended by the watchdog, got {:?}",
        output.status
    );
    assert!(
        took < std::time::Duration::from_secs(30),
        "the watchdog took {took:?}"
    );
}

/// The SDK must not swallow a crash another handler installed earlier wants to
/// see: after recording its marker, the previous handler still runs.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_previously_installed_handler_still_runs_after_ours() {
    let dir = scratch_dir("host");
    let marker = dir.join("crash.info");
    let output = spawn_child("host_handler", &marker);
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        written.contains("signal=11"),
        "our marker is written first: {written:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(77),
        "the host's handler must run and decide the exit; got {:?}",
        output.status
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("HOST_HANDLER_RAN"));
}

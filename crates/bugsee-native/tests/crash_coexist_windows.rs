//
//  crash_coexist_windows.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Windows counterpart of `crash_coexist.rs`: a host that recovers from access
//! violations through a vectored exception handler (a managed runtime's null
//! checks, a JIT's guard pages) must leave no crash marker, and a real fault
//! afterwards must still be reported.
//!
//! The SDK reports from the unhandled-exception filter, which runs only after
//! every vectored handler and SEH frame declined, so a recovered fault never
//! reaches it — in either installation order, which is what this exercises.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::Command;

const KIND_ENV: &str = "BUGSEE_COEXIST_KIND";
const PATH_ENV: &str = "BUGSEE_COEXIST_MARKER";

const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const MEM_COMMIT_RESERVE: u32 = 0x3000;
const PAGE_NOACCESS: u32 = 0x01;
const PAGE_READWRITE: u32 = 0x04;

static PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut core::ffi::c_void,
    number_parameters: u32,
    information: [usize; 15],
}

#[repr(C)]
struct ExceptionPointers {
    record: *mut ExceptionRecord,
    context: *mut core::ffi::c_void,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn AddVectoredExceptionHandler(
        first: u32,
        handler: unsafe extern "system" fn(*mut ExceptionPointers) -> i32,
    ) -> *mut core::ffi::c_void;
    fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
    fn VirtualProtect(addr: *mut u8, size: usize, protect: u32, old: *mut u32) -> i32;
}

/// The "runtime": makes its own guard page accessible and retries the instruction.
unsafe extern "system" fn host(info: *mut ExceptionPointers) -> i32 {
    // SAFETY: the OS passes valid exception pointers.
    unsafe {
        let rec = &*(*info).record;
        // information[1] is the faulting address of an access violation.
        if rec.code == EXCEPTION_ACCESS_VIOLATION
            && rec.information[1] & !0xFFF == PAGE.load(std::sync::atomic::Ordering::SeqCst)
        {
            let mut old = 0u32;
            VirtualProtect(
                (rec.information[1] & !0xFFF) as *mut u8,
                4096,
                PAGE_READWRITE,
                &mut old,
            );
            return EXCEPTION_CONTINUE_EXECUTION;
        }
    }
    EXCEPTION_CONTINUE_SEARCH
}

fn guarded_page() -> *mut u8 {
    // SAFETY: fresh allocation.
    unsafe {
        let p = VirtualAlloc(
            std::ptr::null_mut(),
            4096,
            MEM_COMMIT_RESERVE,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null());
        PAGE.store(p as usize, std::sync::atomic::Ordering::SeqCst);
        p
    }
}

fn child() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let marker = PathBuf::from(std::env::var(PATH_ENV).unwrap());
    // SAFETY: registers a handler with the documented signature.
    let add_host = || unsafe {
        AddVectoredExceptionHandler(1, host);
    };
    match kind.as_str() {
        "host_before_sdk" => add_host(),
        "host_after_sdk" => {}
        other => panic!("unknown kind {other}"),
    }
    let _h = bugsee_native::install(marker.clone()).unwrap();
    if kind == "host_after_sdk" {
        add_host();
    }
    for round in 1..=3 {
        let p = guarded_page();
        // SAFETY: deliberate fault the host recovers from.
        unsafe { std::ptr::write_volatile(p, 7) };
        println!("RECOVERED {round} marker_present={}", marker.exists());
    }
    // Not the host's page: nobody recovers this one.
    // SAFETY: deliberate null write.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) };
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

fn check(kind: &str, test: &str) {
    let (out, marker) = run(kind, test);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for round in 1..=3 {
        assert!(
            stdout.contains(&format!("RECOVERED {round} marker_present=false")),
            "round {round}: a recovered fault must leave no marker; stdout={stdout}"
        );
    }
    assert!(
        !out.status.success(),
        "the real fault must kill the process"
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
fn a_fault_a_host_handler_installed_before_the_sdk_recovers_leaves_no_marker() {
    child();
    check(
        "host_before_sdk",
        "a_fault_a_host_handler_installed_before_the_sdk_recovers_leaves_no_marker",
    );
}

#[test]
fn a_fault_a_host_handler_installed_after_the_sdk_recovers_leaves_no_marker() {
    child();
    check(
        "host_after_sdk",
        "a_fault_a_host_handler_installed_after_the_sdk_recovers_leaves_no_marker",
    );
}

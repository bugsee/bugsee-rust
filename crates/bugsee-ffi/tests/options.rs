//
//  options.rs
//  bugsee-ffi
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The options-handle launch path, storage status, host-error reporting and
//! shutdown, exercised through the C ABI. One test function: the SDK is a
//! process-global singleton, so the steps must run in order.

use std::ffi::{CStr, CString};

use bugsee_ffi::*;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn last_error() -> String {
    let p = bugsee_last_error();
    assert!(!p.is_null());
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

#[test]
fn launch_with_options_then_shutdown() {
    // Before launch.
    let mut flags = 99u32;
    assert_eq!(
        unsafe { bugsee_storage_status(&mut flags) },
        BugseeStatus::NotLaunched
    );
    assert_eq!(flags, 0, "flags are zeroed on failure");
    assert!(last_error().contains("not launched"));
    assert_eq!(bugsee_shutdown(100), BugseeStatus::NotLaunched);
    assert_eq!(
        unsafe { bugsee_storage_status(std::ptr::null_mut()) },
        BugseeStatus::InvalidArgument
    );

    let data = std::env::temp_dir().join(format!("bugsee-ffi-opts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);

    let mut options = std::ptr::null_mut();
    let token = c(&format!("ffiOptions{}", std::process::id()));
    unsafe {
        assert_eq!(
            bugsee_options_new(bugsee_abi_version(), token.as_ptr(), &mut options),
            BugseeStatus::Ok
        );
        assert_eq!(
            bugsee_options_set_data_dir(options, c(data.to_str().unwrap()).as_ptr()),
            BugseeStatus::Ok
        );
        assert_eq!(
            bugsee_options_set_endpoint(options, c("http://127.0.0.1:1").as_ptr()),
            BugseeStatus::Ok
        );
        assert_eq!(
            bugsee_options_set_native_crash_capture(options, 0),
            BugseeStatus::Ok
        );
        assert_eq!(
            bugsee_options_set_app_version(options, c("9.9.9").as_ptr()),
            BugseeStatus::Ok
        );
        assert_eq!(
            bugsee_options_set_launch_recovery_budget_ms(options, 500),
            BugseeStatus::Ok
        );
        assert_eq!(bugsee_launch_with_options(options), BugseeStatus::Ok);
        // The handle is not consumed: launching again from it works (replacing
        // the running SDK), and it is freed afterwards.
        assert_eq!(bugsee_launch_with_options(options), BugseeStatus::Ok);
        bugsee_options_free(options);
    }
    assert_eq!(bugsee_is_active(), 1);
    assert!(data.exists(), "the host-supplied data directory is used");

    let mut flags = 0u32;
    assert_eq!(
        unsafe { bugsee_storage_status(&mut flags) },
        BugseeStatus::Ok
    );
    assert_ne!(flags & BUGSEE_STORAGE_DISK, 0);
    assert_ne!(flags & BUGSEE_STORAGE_CRASH_RECOVERY, 0);
    assert_eq!(flags & BUGSEE_STORAGE_DEGRADED, 0);
    assert_eq!(flags & BUGSEE_STORAGE_MAY_BE_EPHEMERAL, 0);

    let doc = br#"{"name":"RuntimeError","reason":"boom",
        "causes":[{"name":"IOError","reason":"disk full"}],
        "frames":[{"function":"run","module":"App.Job","file":"job.php","line":42}]}"#;
    assert_eq!(
        unsafe { bugsee_report_error(doc.as_ptr().cast(), doc.len()) },
        BugseeStatus::Ok
    );
    let bad = br#"{"frames":[{"line":1}]}"#;
    assert_eq!(
        unsafe { bugsee_report_error(bad.as_ptr().cast(), bad.len()) },
        BugseeStatus::InvalidArgument
    );
    assert!(last_error().contains("frames[0]"));

    // The report is queued for an unreachable endpoint, so the flush cannot
    // drain: shutdown reports Timeout but stops the SDK regardless.
    assert_eq!(bugsee_shutdown(500), BugseeStatus::Timeout);
    assert_eq!(bugsee_is_active(), 0);
    assert_eq!(bugsee_shutdown(100), BugseeStatus::NotLaunched);

    // Opt-in termination flush installs (idempotently) without taking a signal.
    assert_eq!(bugsee_install_termination_flush(1000), BugseeStatus::Ok);
    assert_eq!(bugsee_install_termination_flush(2000), BugseeStatus::Ok);

    let _ = std::fs::remove_dir_all(data);
}

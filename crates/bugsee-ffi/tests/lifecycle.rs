//
//  lifecycle.rs
//  bugsee-ffi
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Exercise the C ABI from Rust: launch → capture → flush → stop, plus argument
//! validation. Delivery goes to an unreachable endpoint (validated elsewhere);
//! this proves the ABI is callable and panic-safe.

use std::ffi::CString;

use bugsee_ffi::*;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

#[test]
fn full_c_abi_lifecycle() {
    let token = c(&format!("ffiLifecycle{}", std::process::id()));
    let endpoint = c("http://127.0.0.1:1"); // connection refused; offline-safe

    assert_eq!(
        unsafe { bugsee_launch_with_endpoint(token.as_ptr(), endpoint.as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(bugsee_is_active(), 1);

    assert_eq!(
        unsafe { bugsee_log(BugseeStatus::Ok as i32 + 3, c("hello").as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe { bugsee_event(c("app_started").as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe {
            bugsee_event_with_params(c("promo").as_ptr(), c(r#"{"code":"SAVE10"}"#).as_ptr())
        },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe { bugsee_trace(c("temp").as_ptr(), c("21.5").as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe { bugsee_set_email(c("user@example.com").as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe { bugsee_set_attribute(c("tier").as_ptr(), c(r#""premium""#).as_ptr()) },
        BugseeStatus::Ok
    );
    assert_eq!(
        unsafe { bugsee_capture_exception(c("PaymentError").as_ptr(), c("declined").as_ptr()) },
        BugseeStatus::Ok
    );

    // Invalid JSON parameters are rejected, not panicked.
    assert_eq!(
        unsafe { bugsee_event_with_params(c("bad").as_ptr(), c("this is not json").as_ptr()) },
        BugseeStatus::InvalidArgument
    );
    // Null arguments are rejected.
    assert_eq!(
        unsafe { bugsee_event(std::ptr::null()) },
        BugseeStatus::InvalidArgument
    );

    assert_eq!(bugsee_pause(), BugseeStatus::Ok);
    assert_eq!(bugsee_is_active(), 0, "paused is not active");
    assert_eq!(bugsee_resume(), BugseeStatus::Ok);

    // The captured exception is queued for an unreachable endpoint, so this flush
    // is launched but cannot drain — it reports Timeout, distinct from
    // NotLaunched (F1), and no longer silently deletes the still-deliverable
    // queued report to "succeed" (F13).
    assert_eq!(
        bugsee_flush(2000),
        BugseeStatus::Timeout,
        "a launched-but-undrained flush reports Timeout, not NotLaunched/Ok"
    );
    assert_eq!(bugsee_stop(), BugseeStatus::Ok);
    assert_eq!(bugsee_is_active(), 0, "stopped is not active");

    // Cleanup the default per-token data dir.
    let mut data = std::env::temp_dir();
    data.push("bugsee");
    let _ = std::fs::remove_dir_all(data);
}

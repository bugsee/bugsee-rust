//
//  lib.rs
//  bugsee-ffi
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! C ABI for the Bugsee Rust SDK.
//!
//! Every exported function runs inside a panic boundary (`catch_unwind`), so a
//! Rust panic never unwinds across the C ABI — it is contained and reported as
//! [`BugseeStatus::Panic`]. String arguments are borrowed C strings (UTF-8);
//! null or invalid arguments yield [`BugseeStatus::InvalidArgument`].
//!
//! See `include/bugsee.h` for the C declarations and `DESIGN.md` §7/§9/§33.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::time::Duration;

use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::{Map, Value};

/// Result of an FFI call.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BugseeStatus {
    Ok = 0,
    InvalidArgument = 1,
    InternalError = 2,
    /// A Rust panic was contained at the boundary.
    Panic = 3,
    NotLaunched = 4,
}

/// Copy a C string into an owned `String` (UTF-8), or `None` if null / not valid
/// UTF-8. Returning an owned value avoids any borrow outliving the C pointer.
///
/// # Safety
/// `ptr` must be null or a valid, NUL-terminated C string that stays alive for
/// the duration of the call.
unsafe fn cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Run `f` inside a panic boundary, mapping a contained panic to `Panic`.
fn guarded(f: impl FnOnce() -> BugseeStatus) -> BugseeStatus {
    match bugsee::guard(f) {
        Ok(status) => status,
        Err(_) => BugseeStatus::Panic,
    }
}

/// Launch the SDK with default options for `app_token`.
///
/// # Safety
/// `app_token` must be a valid NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_launch(app_token: *const c_char) -> BugseeStatus {
    guarded(|| {
        let Some(token) = (unsafe { cstr(app_token) }) else {
            return BugseeStatus::InvalidArgument;
        };
        launch_with_options(LaunchOptions::new(token))
    })
}

/// Launch with an explicit API endpoint override (for testing / self-hosting).
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_launch_with_endpoint(
    app_token: *const c_char,
    endpoint: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let Some(token) = (unsafe { cstr(app_token) }) else {
            return BugseeStatus::InvalidArgument;
        };
        let mut options = LaunchOptions::new(token);
        if let Some(ep) = unsafe { cstr(endpoint) } {
            options = options.endpoint(ep);
        }
        launch_with_options(options)
    })
}

fn launch_with_options(options: LaunchOptions) -> BugseeStatus {
    match Bugsee::launch_with(options) {
        Ok(guard) => {
            // The SDK lifetime is controlled explicitly via bugsee_stop, so the
            // guard must not drop here (which would stop the SDK immediately).
            std::mem::forget(guard);
            BugseeStatus::Ok
        }
        Err(_) => BugseeStatus::InternalError,
    }
}

/// Stop the SDK, flushing pending work.
#[no_mangle]
pub extern "C" fn bugsee_stop() -> BugseeStatus {
    guarded(|| {
        Bugsee::stop();
        BugseeStatus::Ok
    })
}

/// Whether the SDK is launched and not paused (1 = yes, 0 = no).
#[no_mangle]
pub extern "C" fn bugsee_is_active() -> i32 {
    match bugsee::guard(Bugsee::is_active) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Pause capture.
#[no_mangle]
pub extern "C" fn bugsee_pause() -> BugseeStatus {
    guarded(|| {
        Bugsee::pause();
        BugseeStatus::Ok
    })
}

/// Resume capture.
#[no_mangle]
pub extern "C" fn bugsee_resume() -> BugseeStatus {
    guarded(|| {
        Bugsee::resume();
        BugseeStatus::Ok
    })
}

/// Log a message at `level` (1=Error … 5=Verbose).
///
/// # Safety
/// `message` must be a valid NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_log(level: i32, message: *const c_char) -> BugseeStatus {
    guarded(|| {
        let Some(msg) = (unsafe { cstr(message) }) else {
            return BugseeStatus::InvalidArgument;
        };
        Bugsee::log(level_from_int(level), msg);
        BugseeStatus::Ok
    })
}

/// Record a developer event.
///
/// # Safety
/// `name` must be a valid NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_event(name: *const c_char) -> BugseeStatus {
    guarded(|| {
        let Some(name) = (unsafe { cstr(name) }) else {
            return BugseeStatus::InvalidArgument;
        };
        Bugsee::event(name);
        BugseeStatus::Ok
    })
}

/// Record a developer event with a JSON object of parameters.
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_event_with_params(
    name: *const c_char,
    params_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let Some(name) = (unsafe { cstr(name) }) else {
            return BugseeStatus::InvalidArgument;
        };
        let params = match unsafe { cstr(params_json) } {
            Some(json) => match serde_json::from_str::<Map<String, Value>>(&json) {
                Ok(m) => m,
                Err(_) => return BugseeStatus::InvalidArgument,
            },
            None => Map::new(),
        };
        Bugsee::event_with(name, params);
        BugseeStatus::Ok
    })
}

/// Record a named value trace (value is a JSON scalar/array/object).
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_trace(
    name: *const c_char,
    value_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let (Some(name), Some(json)) = (unsafe { cstr(name) }, unsafe { cstr(value_json) }) else {
            return BugseeStatus::InvalidArgument;
        };
        match serde_json::from_str::<Value>(&json) {
            Ok(value) => {
                Bugsee::trace(name, value);
                BugseeStatus::Ok
            }
            Err(_) => BugseeStatus::InvalidArgument,
        }
    })
}

/// Set the user identifier.
///
/// # Safety
/// `email` must be a valid NUL-terminated C string or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_set_email(email: *const c_char) -> BugseeStatus {
    guarded(|| {
        let Some(email) = (unsafe { cstr(email) }) else {
            return BugseeStatus::InvalidArgument;
        };
        Bugsee::set_email(email);
        BugseeStatus::Ok
    })
}

/// Set a report-level attribute (value is a JSON value).
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_set_attribute(
    key: *const c_char,
    value_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let (Some(key), Some(json)) = (unsafe { cstr(key) }, unsafe { cstr(value_json) }) else {
            return BugseeStatus::InvalidArgument;
        };
        match serde_json::from_str::<Value>(&json) {
            Ok(value) => {
                Bugsee::set_attribute(key, value);
                BugseeStatus::Ok
            }
            Err(_) => BugseeStatus::InvalidArgument,
        }
    })
}

/// Report a handled exception (name + reason) from the host language.
///
/// # Safety
/// Both pointers must be valid NUL-terminated C strings or null.
#[no_mangle]
pub unsafe extern "C" fn bugsee_capture_exception(
    name: *const c_char,
    reason: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let name = unsafe { cstr(name) }.unwrap_or_else(|| "Exception".to_string());
        let reason = unsafe { cstr(reason) }.unwrap_or_default();
        Bugsee::capture_message(LogLevel::Error, format!("{name}: {reason}"));
        BugseeStatus::Ok
    })
}

/// Trigger an immediate manual report.
#[no_mangle]
pub extern "C" fn bugsee_upload() -> BugseeStatus {
    guarded(|| {
        Bugsee::upload();
        BugseeStatus::Ok
    })
}

/// Block until pending work drains, or `timeout_ms` elapses.
#[no_mangle]
pub extern "C" fn bugsee_flush(timeout_ms: u32) -> BugseeStatus {
    guarded(|| {
        if Bugsee::flush(Duration::from_millis(timeout_ms as u64)) {
            BugseeStatus::Ok
        } else {
            BugseeStatus::NotLaunched
        }
    })
}

fn level_from_int(level: i32) -> LogLevel {
    match level {
        1 => LogLevel::Error,
        2 => LogLevel::Warning,
        3 => LogLevel::Info,
        4 => LogLevel::Debug,
        _ => LogLevel::Verbose,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_contains_panics() {
        let status = guarded(|| panic!("boom across the FFI boundary"));
        assert_eq!(
            status,
            BugseeStatus::Panic,
            "panic must not unwind across FFI"
        );
    }

    #[test]
    fn guarded_passes_status_through() {
        assert_eq!(guarded(|| BugseeStatus::Ok), BugseeStatus::Ok);
    }

    #[test]
    fn null_arguments_are_rejected() {
        let status = unsafe { bugsee_log(3, std::ptr::null()) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        let status = unsafe { bugsee_event(std::ptr::null()) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
    }
}

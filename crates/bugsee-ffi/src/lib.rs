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
//! See `include/bugsee.h` for the C declarations, `docs/ffi-audit.md` for the
//! design record, and `DESIGN.md` §7/§9/§33.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;
use std::time::Duration;

use bugsee::core::errors::Cause;
use bugsee::core::model::crash::{Frame, FrameData};
use bugsee::{Bugsee, LaunchOptions, LogLevel, StorageMode};
use serde_json::{Map, Value};

/// Result of an FFI call. Callers must treat any value they do not know as an
/// error: new codes may be added without an ABI version bump.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BugseeStatus {
    Ok = 0,
    InvalidArgument = 1,
    InternalError = 2,
    /// A Rust panic was contained at the boundary.
    Panic = 3,
    NotLaunched = 4,
    /// A `flush` or `shutdown` did not drain before its timeout (after a
    /// `shutdown` the SDK is stopped regardless).
    Timeout = 5,
    /// The ABI version the caller was built against is not the one this
    /// library implements.
    AbiMismatch = 6,
}

/// The ABI version this library implements: a monotonic integer bumped on any
/// incompatible change to the C surface, independent of the crate version.
/// Until the surface is frozen, any release may bump it. Compare it with
/// `bugsee_abi_version()` at load time.
pub const BUGSEE_ABI_VERSION: u32 = 1;

/// Log level for [`bugsee_log`]: errors.
pub const BUGSEE_LEVEL_ERROR: i32 = 1;
/// Log level for [`bugsee_log`]: warnings.
pub const BUGSEE_LEVEL_WARNING: i32 = 2;
/// Log level for [`bugsee_log`]: informational messages.
pub const BUGSEE_LEVEL_INFO: i32 = 3;
/// Log level for [`bugsee_log`]: debugging output.
pub const BUGSEE_LEVEL_DEBUG: i32 = 4;
/// Log level for [`bugsee_log`]: verbose output.
pub const BUGSEE_LEVEL_VERBOSE: i32 = 5;

/// Storage flag for `bugsee_storage_status`: state is kept on disk (clear:
/// the SDK runs from memory and nothing survives the process).
pub const BUGSEE_STORAGE_DISK: u32 = 1 << 0;
/// Storage flag: a disk write has failed since launch; crash recovery can no
/// longer be relied on.
pub const BUGSEE_STORAGE_DEGRADED: u32 = 1 << 1;
/// Storage flag: a fatal crash can be recovered on the next launch.
pub const BUGSEE_STORAGE_CRASH_RECOVERY: u32 = 1 << 2;
/// Storage flag: the data directory is the temp-dir fallback and may be wiped
/// on restart.
pub const BUGSEE_STORAGE_MAY_BE_EPHEMERAL: u32 = 1 << 3;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Record `message` as this thread's last error (NUL bytes are dropped).
fn set_last_error(message: impl Into<Vec<u8>>) {
    let mut bytes: Vec<u8> = message.into();
    bytes.retain(|b| *b != 0);
    let msg = CString::new(bytes).unwrap_or_default();
    // `try_with`: never panic from a destructor-time call.
    let _ = LAST_ERROR.try_with(|slot| *slot.borrow_mut() = Some(msg));
}

/// Record `message` and return `status`.
fn fail(status: BugseeStatus, message: impl Into<Vec<u8>>) -> BugseeStatus {
    set_last_error(message);
    status
}

fn invalid(message: &str) -> BugseeStatus {
    fail(BugseeStatus::InvalidArgument, message)
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

/// Read a *required* string argument, recording why on failure.
///
/// # Safety
/// Same as [`cstr`].
unsafe fn cstr_required(ptr: *const c_char, what: &str) -> Result<String, BugseeStatus> {
    if ptr.is_null() {
        return Err(invalid(&format!("{what} must not be NULL")));
    }
    unsafe { cstr(ptr) }.ok_or_else(|| invalid(&format!("{what} is not valid UTF-8")))
}

/// Read an *optional* string argument: `Ok(None)` for NULL, `Ok(Some(_))` for
/// valid UTF-8, `Err(InvalidArgument)` (recorded) for invalid UTF-8, rather
/// than silently using the default.
///
/// # Safety
/// Same as [`cstr`].
unsafe fn cstr_optional(ptr: *const c_char, what: &str) -> Result<Option<String>, BugseeStatus> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|s| Some(s.to_owned()))
        .map_err(|_| invalid(&format!("{what} is not valid UTF-8")))
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Run `f` inside a panic boundary, mapping a contained panic to `Panic`.
fn guarded(f: impl FnOnce() -> BugseeStatus) -> BugseeStatus {
    match bugsee::guard(f) {
        Ok(status) => status,
        Err(payload) => {
            let message = panic_message(&*payload);
            // A payload whose destructor panics must not unwind out of `extern "C"`.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)));
            fail(
                BugseeStatus::Panic,
                format!("panic contained at the FFI boundary: {message}"),
            )
        }
    }
}

/// Run `f` (which records its own failure) inside the panic boundary.
fn guarded_result(f: impl FnOnce() -> Result<(), BugseeStatus>) -> BugseeStatus {
    guarded(|| match f() {
        Ok(()) => BugseeStatus::Ok,
        Err(status) => status,
    })
}

/// The ABI version this library implements (`BUGSEE_ABI_VERSION`). Compare it
/// with the value from the header you built against before calling anything
/// else. It changes only on an incompatible change to the C surface and is
/// independent of the crate version.
#[no_mangle]
pub extern "C" fn bugsee_abi_version() -> u32 {
    BUGSEE_ABI_VERSION
}

/// The crate version as a static NUL-terminated string (for diagnostics, not
/// compatibility checks: use `bugsee_abi_version` for that). The pointer is
/// valid for the life of the process and must not be freed.
#[no_mangle]
pub extern "C" fn bugsee_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// A static, human-readable name for a status code, as a NUL-terminated string
/// (never NULL; unknown values give "unknown status"). Takes an integer, not
/// the enum, so a value from a newer library is safe to pass. The pointer is
/// valid for the life of the process and must not be freed.
#[no_mangle]
pub extern "C" fn bugsee_status_string(status: i32) -> *const c_char {
    let s: &'static str = match status {
        0 => "ok\0",
        1 => "invalid argument\0",
        2 => "internal error\0",
        3 => "panic contained at the FFI boundary\0",
        4 => "SDK not launched\0",
        5 => "timed out\0",
        6 => "ABI version mismatch\0",
        _ => "unknown status\0",
    };
    s.as_ptr().cast()
}

/// The message for the most recent non-OK status returned to the **calling
/// thread** by any SDK function, or NULL if none has occurred on this thread.
///
/// A successful call does not clear it, so read it right after the failing
/// call. The pointer is owned by the SDK, must not be freed, and stays valid
/// only until the next SDK call on the same thread that fails (copy it if you
/// need it longer). The text is for humans, not for parsing.
#[no_mangle]
pub extern "C" fn bugsee_last_error() -> *const c_char {
    LAST_ERROR
        .try_with(|slot| {
            slot.borrow()
                .as_ref()
                .map_or(std::ptr::null(), |msg| msg.as_ptr())
        })
        .unwrap_or(std::ptr::null())
}

/// Launch options under construction: a plain bag of values the host fills with
/// the `bugsee_options_set_*` setters, then hands to
/// `bugsee_launch_with_options`. Opaque to C; created by `bugsee_options_new`,
/// released by `bugsee_options_free`.
#[derive(Debug, Clone)]
pub struct BugseeOptions {
    app_token: String,
    endpoint: Option<String>,
    data_dir: Option<PathBuf>,
    app_version: Option<String>,
    app_build: Option<String>,
    app_package_id: Option<String>,
    native_crash_capture: Option<bool>,
    system_telemetry: Option<bool>,
    report_panics_from_hook: Option<bool>,
    scrubbing: Option<bool>,
    rate_limiting: Option<bool>,
    release_health: Option<bool>,
    sample_rate: Option<f64>,
    launch_recovery_budget: Option<Duration>,
    flush_at_exit: Option<Duration>,
}

impl BugseeOptions {
    fn new(app_token: String) -> Self {
        BugseeOptions {
            app_token,
            endpoint: None,
            data_dir: None,
            app_version: None,
            app_build: None,
            app_package_id: None,
            native_crash_capture: None,
            system_telemetry: None,
            report_panics_from_hook: None,
            scrubbing: None,
            rate_limiting: None,
            release_health: None,
            sample_rate: None,
            launch_recovery_budget: None,
            flush_at_exit: None,
        }
    }

    fn to_launch_options(&self) -> LaunchOptions {
        let mut o = LaunchOptions::new(self.app_token.clone());
        if let Some(v) = &self.endpoint {
            o = o.endpoint(v.clone());
        }
        if let Some(v) = &self.data_dir {
            o = o.data_dir(v.clone());
        }
        if let Some(v) = &self.app_version {
            o = o.app_version(v.clone());
        }
        if let Some(v) = &self.app_build {
            o = o.app_build(v.clone());
        }
        if let Some(v) = &self.app_package_id {
            o = o.app_package_id(v.clone());
        }
        if let Some(v) = self.native_crash_capture {
            o = o.native_crash_capture(v);
        }
        if let Some(v) = self.system_telemetry {
            o = o.system_telemetry(v);
        }
        if let Some(v) = self.report_panics_from_hook {
            o = o.report_panics_from_hook(v);
        }
        if self.scrubbing == Some(false) {
            o = o.disable_scrubbing();
        }
        if self.rate_limiting == Some(false) {
            o = o.disable_rate_limiting();
        }
        if let Some(v) = self.release_health {
            o = o.release_health(v);
        }
        if let Some(v) = self.sample_rate {
            o = o.sample_rate(v);
        }
        if let Some(v) = self.launch_recovery_budget {
            o = o.launch_recovery_budget(v);
        }
        if let Some(v) = self.flush_at_exit {
            o = o.flush_at_exit(v);
        }
        o
    }
}

/// Create an options handle for `app_token`, written to `*out`.
///
/// `abi_version` must be the `BUGSEE_ABI_VERSION` the caller was built against;
/// a different value fails with `BUGSEE_STATUS_ABI_MISMATCH`. `*out` is set to
/// NULL on every failure. Every other option starts at its documented default.
///
/// # Safety
/// `app_token` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`); it is copied, so it need only live for the call. `out`
/// must be a valid pointer to a writable `BugseeOptions *` (NULL is
/// `INVALID_ARGUMENT`). On success the caller owns `*out` and must release it
/// with `bugsee_options_free`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_new(
    abi_version: u32,
    app_token: *const c_char,
    out: *mut *mut BugseeOptions,
) -> BugseeStatus {
    guarded_result(|| {
        if out.is_null() {
            return Err(invalid("out must not be NULL"));
        }
        unsafe { *out = std::ptr::null_mut() };
        if abi_version != BUGSEE_ABI_VERSION {
            return Err(fail(
                BugseeStatus::AbiMismatch,
                format!(
                    "caller built against ABI version {abi_version}, library implements {BUGSEE_ABI_VERSION}"
                ),
            ));
        }
        let app_token = unsafe { cstr_required(app_token, "app_token") }?;
        unsafe { *out = Box::into_raw(Box::new(BugseeOptions::new(app_token))) };
        Ok(())
    })
}

/// Release an options handle. NULL is a no-op. Launching does not consume the
/// handle, so free it once you are done with it, whether or not the launch
/// succeeded.
///
/// # Safety
/// `options` must be NULL or a pointer returned by `bugsee_options_new` that
/// has not been freed already, and no other thread may be using it.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_free(options: *mut BugseeOptions) {
    if !options.is_null() {
        let _ = bugsee::guard(|| drop(unsafe { Box::from_raw(options) }));
    }
}

/// Borrow the handle mutably for a setter.
///
/// # Safety
/// `options` must be null or a live handle that no other thread is using.
unsafe fn options_mut<'a>(
    options: *mut BugseeOptions,
) -> Result<&'a mut BugseeOptions, BugseeStatus> {
    unsafe { options.as_mut() }.ok_or_else(|| invalid("options must not be NULL"))
}

/// Override the API endpoint (testing / self-hosting). NULL = production.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). `value` must be NULL (reset to
/// the default) or a valid NUL-terminated UTF-8 string, which is copied;
/// invalid UTF-8 is `INVALID_ARGUMENT`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_endpoint(
    options: *mut BugseeOptions,
    value: *const c_char,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        let value = unsafe { cstr_optional(value, "endpoint") }?;
        options.endpoint = value;
        Ok(())
    })
}

/// Set the on-disk data directory. It must outlive the process for crash
/// recovery to work, and is the setting a mobile host cannot do without
/// (point it at the app's private files directory). NULL = the SDK's default
/// resolution (the `BUGSEE_DATA_DIR` environment variable, then fallbacks).
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). `value` must be NULL (reset to
/// the default) or a valid NUL-terminated UTF-8 string, which is copied;
/// invalid UTF-8 is `INVALID_ARGUMENT`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_data_dir(
    options: *mut BugseeOptions,
    value: *const c_char,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        let value = unsafe { cstr_optional(value, "data_dir") }?;
        options.data_dir = value.map(PathBuf::from);
        Ok(())
    })
}

/// Set the application version reported with every issue. NULL = detect.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). `value` must be NULL (reset to
/// the default) or a valid NUL-terminated UTF-8 string, which is copied;
/// invalid UTF-8 is `INVALID_ARGUMENT`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_app_version(
    options: *mut BugseeOptions,
    value: *const c_char,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        let value = unsafe { cstr_optional(value, "app_version") }?;
        options.app_version = value;
        Ok(())
    })
}

/// Set the application build reported with every issue. NULL = detect.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). `value` must be NULL (reset to
/// the default) or a valid NUL-terminated UTF-8 string, which is copied;
/// invalid UTF-8 is `INVALID_ARGUMENT`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_app_build(
    options: *mut BugseeOptions,
    value: *const c_char,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        let value = unsafe { cstr_optional(value, "app_build") }?;
        options.app_build = value;
        Ok(())
    })
}

/// Set the application package / bundle id reported with every issue.
/// NULL = detect.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). `value` must be NULL (reset to
/// the default) or a valid NUL-terminated UTF-8 string, which is copied;
/// invalid UTF-8 is `INVALID_ARGUMENT`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_app_package_id(
    options: *mut BugseeOptions,
    value: *const c_char,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        let value = unsafe { cstr_optional(value, "app_package_id") }?;
        options.app_package_id = value;
        Ok(())
    })
}

/// Native crash capture (signal / exception handlers). Default on. A managed
/// host (JVM, V8, CLR, PHP) that owns native crash handling should turn it off.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_native_crash_capture(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.native_crash_capture = Some(enabled != 0);
        Ok(())
    })
}

/// System telemetry sampling (battery, memory, network, ...). Default on.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_system_telemetry(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.system_telemetry = Some(enabled != 0);
        Ok(())
    })
}

/// Report Rust panics from the panic hook, outside any guarded call.
/// Default off.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_report_panics_from_hook(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.report_panics_from_hook = Some(enabled != 0);
        Ok(())
    })
}

/// PII / secret scrubbing of every report. Default on; turning it off sends
/// reports exactly as captured.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_scrubbing(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.scrubbing = Some(enabled != 0);
        Ok(())
    })
}

/// Report rate limiting. Default on.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_rate_limiting(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.rate_limiting = Some(enabled != 0);
        Ok(())
    })
}

/// Release-health session tracking. Default off.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`). Any non-zero `enabled` is true.
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_release_health(
    options: *mut BugseeOptions,
    enabled: i32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.release_health = Some(enabled != 0);
        Ok(())
    })
}

/// Fraction of non-fatal reports to keep, in `[0.0, 1.0]` (default 1.0; crashes
/// are never sampled out). NaN or an out-of-range value is `INVALID_ARGUMENT`.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`).
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_sample_rate(
    options: *mut BugseeOptions,
    rate: f64,
) -> BugseeStatus {
    guarded_result(|| {
        let options = unsafe { options_mut(options) }?;
        if !(0.0..=1.0).contains(&rate) {
            return Err(invalid("sample_rate must be within [0.0, 1.0]"));
        }
        options.sample_rate = Some(rate);
        Ok(())
    })
}

/// Longest time `launch` may spend recovering a previous crash, in
/// milliseconds. Default: the SDK's built-in budget (a few seconds). 0 skips
/// the launch pass entirely: crashes are recovered in the background instead.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`).
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_launch_recovery_budget_ms(
    options: *mut BugseeOptions,
    budget_ms: u32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.launch_recovery_budget =
            Some(Duration::from_millis(budget_ms as u64));
        Ok(())
    })
}

/// Flush pending reports for up to `timeout_ms` when the process exits through
/// the normal runtime exit path (unix only; on Windows call `bugsee_shutdown`
/// yourself). 0 = off, which is the default.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is using (NULL is `INVALID_ARGUMENT`).
#[no_mangle]
pub unsafe extern "C" fn bugsee_options_set_flush_at_exit_ms(
    options: *mut BugseeOptions,
    timeout_ms: u32,
) -> BugseeStatus {
    guarded_result(|| {
        unsafe { options_mut(options) }?.flush_at_exit =
            (timeout_ms > 0).then(|| Duration::from_millis(timeout_ms as u64));
        Ok(())
    })
}

/// Launch the SDK with the settings in `options`. The handle is only read, not
/// consumed: free it with `bugsee_options_free` afterwards. Launching while
/// already launched replaces the running SDK.
///
/// # Safety
/// `options` must be a live handle from `bugsee_options_new` that no other
/// thread is modifying during the call (NULL is `INVALID_ARGUMENT`).
#[no_mangle]
pub unsafe extern "C" fn bugsee_launch_with_options(options: *const BugseeOptions) -> BugseeStatus {
    guarded(|| {
        let Some(options) = (unsafe { options.as_ref() }) else {
            return invalid("options must not be NULL");
        };
        launch_with_options(options.to_launch_options())
    })
}

/// Launch the SDK with default options for `app_token`.
///
/// # Safety
/// `app_token` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`); it is copied, so it need only live for the call.
#[no_mangle]
pub unsafe extern "C" fn bugsee_launch(app_token: *const c_char) -> BugseeStatus {
    guarded(|| {
        let token = match unsafe { cstr_required(app_token, "app_token") } {
            Ok(t) => t,
            Err(status) => return status,
        };
        launch_with_options(LaunchOptions::new(token))
    })
}

/// Launch with an explicit API endpoint override (for testing / self-hosting).
///
/// # Safety
/// `app_token` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`). `endpoint` may be NULL (the default endpoint) or a valid
/// NUL-terminated UTF-8 string; invalid UTF-8 is `INVALID_ARGUMENT`. Both are
/// copied, so they need only live for the call.
#[no_mangle]
pub unsafe extern "C" fn bugsee_launch_with_endpoint(
    app_token: *const c_char,
    endpoint: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let token = match unsafe { cstr_required(app_token, "app_token") } {
            Ok(t) => t,
            Err(status) => return status,
        };
        let mut options = LaunchOptions::new(token);
        let endpoint = match unsafe { cstr_optional(endpoint, "endpoint") } {
            Ok(e) => e,
            Err(status) => return status,
        };
        if let Some(ep) = endpoint {
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
        Err(e) => fail(BugseeStatus::InternalError, format!("launch failed: {e}")),
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

/// Log a message at `level` (1=Error … 5=Verbose; anything else is
/// `INVALID_ARGUMENT`).
///
/// # Safety
/// `message` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`); it is copied.
#[no_mangle]
pub unsafe extern "C" fn bugsee_log(level: i32, message: *const c_char) -> BugseeStatus {
    guarded(|| {
        let Some(level) = level_from_int(level) else {
            return invalid("log level must be 1 (error) to 5 (verbose)");
        };
        let msg = match unsafe { cstr_required(message, "message") } {
            Ok(m) => m,
            Err(status) => return status,
        };
        Bugsee::log(level, msg);
        BugseeStatus::Ok
    })
}

/// Record a developer event.
///
/// # Safety
/// `name` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`); it is copied.
#[no_mangle]
pub unsafe extern "C" fn bugsee_event(name: *const c_char) -> BugseeStatus {
    guarded(|| {
        let name = match unsafe { cstr_required(name, "name") } {
            Ok(n) => n,
            Err(status) => return status,
        };
        Bugsee::event(name);
        BugseeStatus::Ok
    })
}

/// Record a developer event with a JSON object of parameters.
///
/// # Safety
/// `name` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`). `params_json` may be NULL (no parameters) or a valid
/// NUL-terminated UTF-8 JSON object; anything else is `INVALID_ARGUMENT`. Both are
/// copied.
#[no_mangle]
pub unsafe extern "C" fn bugsee_event_with_params(
    name: *const c_char,
    params_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let name = match unsafe { cstr_required(name, "name") } {
            Ok(n) => n,
            Err(status) => return status,
        };
        let params_json = match unsafe { cstr_optional(params_json, "params_json") } {
            Ok(p) => p,
            Err(status) => return status,
        };
        let params = match params_json {
            Some(json) => match serde_json::from_str::<Map<String, Value>>(&json) {
                Ok(m) => m,
                Err(e) => return invalid(&format!("params_json is not a JSON object: {e}")),
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
/// Both pointers must be valid NUL-terminated UTF-8 strings (NULL is
/// `INVALID_ARGUMENT`); they are copied. `value_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn bugsee_trace(
    name: *const c_char,
    value_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let name = match unsafe { cstr_required(name, "name") } {
            Ok(n) => n,
            Err(status) => return status,
        };
        let json = match unsafe { cstr_required(value_json, "value_json") } {
            Ok(j) => j,
            Err(status) => return status,
        };
        match serde_json::from_str::<Value>(&json) {
            Ok(value) => {
                Bugsee::trace(name, value);
                BugseeStatus::Ok
            }
            Err(e) => invalid(&format!("value_json is not valid JSON: {e}")),
        }
    })
}

/// Set the user identifier.
///
/// # Safety
/// `email` must be a valid NUL-terminated UTF-8 string (NULL is
/// `INVALID_ARGUMENT`); it is copied.
#[no_mangle]
pub unsafe extern "C" fn bugsee_set_email(email: *const c_char) -> BugseeStatus {
    guarded(|| {
        let email = match unsafe { cstr_required(email, "email") } {
            Ok(e) => e,
            Err(status) => return status,
        };
        Bugsee::set_email(email);
        BugseeStatus::Ok
    })
}

/// Set a report-level attribute (value is a JSON value).
///
/// # Safety
/// Both pointers must be valid NUL-terminated UTF-8 strings (NULL is
/// `INVALID_ARGUMENT`); they are copied. `value_json` must be valid JSON.
#[no_mangle]
pub unsafe extern "C" fn bugsee_set_attribute(
    key: *const c_char,
    value_json: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let key = match unsafe { cstr_required(key, "key") } {
            Ok(k) => k,
            Err(status) => return status,
        };
        let json = match unsafe { cstr_required(value_json, "value_json") } {
            Ok(j) => j,
            Err(status) => return status,
        };
        match serde_json::from_str::<Value>(&json) {
            Ok(value) => {
                Bugsee::set_attribute(key, value);
                BugseeStatus::Ok
            }
            Err(e) => invalid(&format!("value_json is not valid JSON: {e}")),
        }
    })
}

/// Report a handled exception (name + reason) from the host language. This is
/// lossy: it records a message issue with the Rust backtrace of the call site,
/// not the host's stack. Prefer `bugsee_report_error`.
///
/// # Safety
/// Both pointers may be NULL (`name` defaults to "Exception", `reason` to "") or
/// valid NUL-terminated UTF-8 strings; invalid UTF-8 is `INVALID_ARGUMENT`. Both
/// are copied.
#[no_mangle]
pub unsafe extern "C" fn bugsee_capture_exception(
    name: *const c_char,
    reason: *const c_char,
) -> BugseeStatus {
    guarded(|| {
        let name = match unsafe { cstr_optional(name, "name") } {
            Ok(n) => n,
            Err(status) => return status,
        };
        let reason = match unsafe { cstr_optional(reason, "reason") } {
            Ok(r) => r,
            Err(status) => return status,
        };
        let name = name.unwrap_or_else(|| "Exception".to_string());
        let reason = reason.unwrap_or_default();
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

/// Block until pending work drains, or `timeout_ms` elapses. Returns `Ok` on a
/// full drain, `Timeout` if the SDK is launched but the queue did not drain in
/// time, or `NotLaunched` if the SDK was never launched (F1).
#[no_mangle]
pub extern "C" fn bugsee_flush(timeout_ms: u32) -> BugseeStatus {
    guarded(|| {
        if !Bugsee::is_launched() {
            return fail(BugseeStatus::NotLaunched, "the SDK is not launched");
        }
        if Bugsee::flush(Duration::from_millis(timeout_ms as u64)) {
            BugseeStatus::Ok
        } else {
            fail(
                BugseeStatus::Timeout,
                "flush did not drain before the timeout",
            )
        }
    })
}

/// Flush, then stop: the call to make before a deliberate exit. Waits up to
/// `timeout_ms` for captured work to be persisted and the queue to drain, then
/// stops the SDK whatever the outcome. Returns `OK` if everything was delivered,
/// `TIMEOUT` if not (undelivered reports stay in the durable queue for the next
/// launch in disk mode; the SDK is stopped either way), or `NOT_LAUNCHED`.
/// Worst-case blocking time is `timeout_ms` plus about 4 seconds.
#[no_mangle]
pub extern "C" fn bugsee_shutdown(timeout_ms: u32) -> BugseeStatus {
    guarded(|| {
        if !Bugsee::is_launched() {
            return fail(BugseeStatus::NotLaunched, "the SDK is not launched");
        }
        if Bugsee::shutdown(Duration::from_millis(timeout_ms as u64)) {
            BugseeStatus::Ok
        } else {
            fail(
                BugseeStatus::Timeout,
                "shutdown flush did not drain before the timeout; the SDK is stopped",
            )
        }
    })
}

/// Opt in to a flush on `SIGTERM` / `SIGINT` (unix) or console control events
/// (Windows), waiting up to `timeout_ms`. Never on by default and it never
/// takes the signal from the host: the previous handler still runs. Idempotent
/// (a second call only updates the timeout) and valid before launch. Choose a
/// timeout below the orchestrator's grace period by at least 4 seconds.
#[no_mangle]
pub extern "C" fn bugsee_install_termination_flush(timeout_ms: u32) -> BugseeStatus {
    guarded(
        || match Bugsee::install_termination_flush(Duration::from_millis(timeout_ms as u64)) {
            Ok(()) => BugseeStatus::Ok,
            Err(e) => fail(
                BugseeStatus::InternalError,
                format!("installing the termination flush failed: {e}"),
            ),
        },
    )
}

/// Where the SDK keeps its state, as a bit set of `BUGSEE_STORAGE_*` flags
/// written to `*out_flags`; `NOT_LAUNCHED` (and `*out_flags` = 0) before launch.
/// Use it to confirm a deployment can recover from a crash: it needs
/// `BUGSEE_STORAGE_CRASH_RECOVERY`, and on a container
/// `BUGSEE_STORAGE_MAY_BE_EPHEMERAL` clear. Unknown bits must be ignored.
///
/// # Safety
/// `out_flags` must be a valid pointer to a writable `uint32_t` (NULL is
/// `INVALID_ARGUMENT`).
#[no_mangle]
pub unsafe extern "C" fn bugsee_storage_status(out_flags: *mut u32) -> BugseeStatus {
    guarded_result(|| {
        if out_flags.is_null() {
            return Err(invalid("out_flags must not be NULL"));
        }
        unsafe { *out_flags = 0 };
        let Some(status) = Bugsee::storage_status() else {
            return Err(fail(BugseeStatus::NotLaunched, "the SDK is not launched"));
        };
        let mut flags = 0;
        if status.mode == StorageMode::Disk {
            flags |= BUGSEE_STORAGE_DISK;
        }
        if status.degraded {
            flags |= BUGSEE_STORAGE_DEGRADED;
        }
        if status.crash_recovery_available() {
            flags |= BUGSEE_STORAGE_CRASH_RECOVERY;
        }
        if status.may_be_ephemeral() {
            flags |= BUGSEE_STORAGE_MAY_BE_EPHEMERAL;
        }
        unsafe { *out_flags = flags };
        Ok(())
    })
}

/// Largest `bugsee_report_error` document accepted, in bytes.
const MAX_ERROR_JSON: usize = 1 << 20;
/// Frames kept from a host error (the innermost are kept).
const MAX_HOST_FRAMES: usize = 512;
/// Cause-chain links kept from a host error.
const MAX_HOST_CAUSES: usize = 32;

fn str_field(obj: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` must be a string")),
    }
}

fn parse_frame(value: &Value, index: usize) -> Result<Frame, String> {
    let ctx = |e: String| format!("frames[{index}]: {e}");
    let obj = value
        .as_object()
        .ok_or_else(|| format!("frames[{index}] must be an object"))?;
    let function = str_field(obj, "function")
        .map_err(ctx)?
        .ok_or_else(|| format!("frames[{index}] needs a `function`"))?;
    let module = str_field(obj, "module").map_err(ctx)?;
    let file = str_field(obj, "file").map_err(ctx)?;
    let line = match obj.get("line") {
        None | Some(Value::Null) => -1,
        Some(v) => v
            .as_i64()
            .ok_or_else(|| format!("frames[{index}]: `line` must be an integer"))?,
    };
    let in_app = match obj.get("in_app") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(format!("frames[{index}]: `in_app` must be a boolean")),
    };
    let qualified = match &module {
        Some(m) if !m.is_empty() => format!("{m}::{function}"),
        _ => function.clone(),
    };
    let trace = match (&file, line) {
        (Some(f), l) if l >= 0 => format!("{qualified} ({f}:{l})"),
        (Some(f), _) => format!("{qualified} ({f})"),
        (None, _) => qualified,
    };
    Ok(Frame {
        trace,
        // Frames the host marks as not application code are hidden, so grouping
        // and the displayed crash site use the first in-app frame.
        hidden: !in_app,
        data: FrameData {
            source: file,
            member_class: module,
            member: Some(function),
            line,
        },
    })
}

/// A parsed `bugsee_report_error` document.
struct HostError {
    name: String,
    reason: String,
    causes: Vec<Cause>,
    frames: Vec<Frame>,
}

fn parse_host_error(json: &str) -> Result<HostError, String> {
    let value: Value = serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "the document must be a JSON object".to_string())?;
    let name = str_field(obj, "name")?.unwrap_or_else(|| "Exception".to_string());
    let reason = str_field(obj, "reason")?.unwrap_or_default();

    let mut causes = Vec::new();
    match obj.get("causes") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let c = item
                    .as_object()
                    .ok_or_else(|| format!("causes[{i}] must be an object"))?;
                let ctx = |e: String| format!("causes[{i}]: {e}");
                let name = str_field(c, "name")
                    .map_err(ctx)?
                    .unwrap_or_else(|| "Error".to_string());
                let reason = str_field(c, "reason").map_err(ctx)?.unwrap_or_default();
                if causes.len() < MAX_HOST_CAUSES {
                    causes.push(Cause { name, reason });
                }
            }
        }
        Some(_) => return Err("`causes` must be an array".to_string()),
    }

    let mut frames = Vec::new();
    match obj.get("frames") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for (i, item) in items.iter().enumerate() {
                let frame = parse_frame(item, i)?;
                if frames.len() < MAX_HOST_FRAMES {
                    frames.push(frame);
                }
            }
        }
        Some(_) => return Err("`frames` must be an array".to_string()),
    }
    Ok(HostError {
        name,
        reason,
        causes,
        frames,
    })
}

/// Report a **handled** error caught by the host language, with the host's own
/// stack, as a non-fatal `error` issue. Unlike `bugsee_capture_exception`, no
/// Rust backtrace of the binding's glue is attached.
///
/// `json` points at `len` bytes of a UTF-8 JSON object (no NUL terminator is
/// needed; up to 1 MiB):
///
/// `{"name": "RuntimeError", "reason": "boom",
///   "causes": [{"name": "IOError", "reason": "disk full"}],
///   "frames": [{"function": "run", "module": "App.Job", "file": "job.php",
///               "line": 42, "in_app": true}]}`
///
/// `name` defaults to "Exception", `reason` to "". `frames` run innermost call
/// first (at most 512 are kept); `function` is required per frame, and a frame
/// with `"in_app": false` is hidden from grouping. `causes` runs outermost first
/// (at most 32 are kept). Unknown fields are ignored. Does nothing (and returns
/// `OK`) before launch, like the other capture calls.
///
/// # Safety
/// `json` must point at `len` readable bytes that stay valid for the call (NULL
/// or a `len` of 0 is `INVALID_ARGUMENT`). Invalid UTF-8, malformed JSON, a
/// wrongly typed field or more than 1 MiB is `INVALID_ARGUMENT`; see
/// `bugsee_last_error`.
#[no_mangle]
pub unsafe extern "C" fn bugsee_report_error(json: *const c_char, len: usize) -> BugseeStatus {
    guarded_result(|| {
        if json.is_null() || len == 0 {
            return Err(invalid("json must be non-NULL with a non-zero length"));
        }
        if len > MAX_ERROR_JSON {
            return Err(invalid("json exceeds the 1 MiB limit"));
        }
        let bytes = unsafe { std::slice::from_raw_parts(json.cast::<u8>(), len) };
        let text = std::str::from_utf8(bytes).map_err(|_| invalid("json is not valid UTF-8"))?;
        let error = parse_host_error(text).map_err(|e| invalid(&format!("json: {e}")))?;
        Bugsee::capture_host_error(&error.name, &error.reason, &error.causes, error.frames);
        Ok(())
    })
}

/// `None` for a level outside 1..=5: the caller gets `INVALID_ARGUMENT`.
fn level_from_int(level: i32) -> Option<LogLevel> {
    match level {
        BUGSEE_LEVEL_ERROR => Some(LogLevel::Error),
        BUGSEE_LEVEL_WARNING => Some(LogLevel::Warning),
        BUGSEE_LEVEL_INFO => Some(LogLevel::Info),
        BUGSEE_LEVEL_DEBUG => Some(LogLevel::Debug),
        BUGSEE_LEVEL_VERBOSE => Some(LogLevel::Verbose),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_error() -> Option<String> {
        let p = bugsee_last_error();
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }

    fn new_options(token: &CStr) -> *mut BugseeOptions {
        let mut out = std::ptr::null_mut();
        let status = unsafe { bugsee_options_new(BUGSEE_ABI_VERSION, token.as_ptr(), &mut out) };
        assert_eq!(status, BugseeStatus::Ok);
        assert!(!out.is_null());
        out
    }

    #[test]
    fn guarded_contains_panics() {
        let status = guarded(|| panic!("boom across the FFI boundary"));
        assert_eq!(
            status,
            BugseeStatus::Panic,
            "panic must not unwind across FFI"
        );
        assert!(last_error()
            .unwrap()
            .contains("boom across the FFI boundary"));
    }

    #[test]
    fn guarded_passes_status_through() {
        assert_eq!(guarded(|| BugseeStatus::Ok), BugseeStatus::Ok);
    }

    #[test]
    fn null_arguments_are_rejected() {
        let status = unsafe { bugsee_log(3, std::ptr::null()) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        assert!(last_error().unwrap().contains("message must not be NULL"));
        let status = unsafe { bugsee_event(std::ptr::null()) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        assert!(last_error().unwrap().contains("name must not be NULL"));
    }

    #[test]
    fn out_of_range_log_level_is_rejected() {
        for level in [0, -3, 6, 99] {
            assert_eq!(
                unsafe { bugsee_log(level, c"x".as_ptr()) },
                BugseeStatus::InvalidArgument,
                "level {level}"
            );
        }
        for level in 1..=5 {
            assert_eq!(
                unsafe { bugsee_log(level, c"x".as_ptr()) },
                BugseeStatus::Ok
            );
        }
    }

    #[test]
    fn invalid_utf8_on_optional_args_is_rejected() {
        let bad = c"\xff\xfe".as_ptr();
        let ok = c"x".as_ptr();
        unsafe {
            assert_eq!(
                bugsee_launch_with_endpoint(ok, bad),
                BugseeStatus::InvalidArgument
            );
            assert_eq!(
                bugsee_event_with_params(ok, bad),
                BugseeStatus::InvalidArgument
            );
            assert_eq!(
                bugsee_capture_exception(bad, ok),
                BugseeStatus::InvalidArgument
            );
            assert_eq!(
                bugsee_capture_exception(ok, bad),
                BugseeStatus::InvalidArgument
            );
            // NULL stays legal on optional args.
            assert_eq!(
                bugsee_capture_exception(std::ptr::null(), std::ptr::null()),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_event_with_params(ok, std::ptr::null()),
                BugseeStatus::Ok
            );
        }
    }

    #[test]
    fn last_error_is_per_thread_and_sticky() {
        // A fresh thread has no error.
        std::thread::spawn(|| assert!(bugsee_last_error().is_null()))
            .join()
            .unwrap();
        assert_eq!(
            unsafe { bugsee_event(std::ptr::null()) },
            BugseeStatus::InvalidArgument
        );
        let msg = last_error().unwrap();
        // A success does not clear it.
        assert_eq!(unsafe { bugsee_event(c"ok".as_ptr()) }, BugseeStatus::Ok);
        assert_eq!(last_error().as_deref(), Some(msg.as_str()));
        // Another thread's failure is not visible here.
        std::thread::spawn(|| {
            let _ = unsafe { bugsee_log(99, c"x".as_ptr()) };
            assert!(last_error().unwrap().contains("log level"));
        })
        .join()
        .unwrap();
        assert_eq!(last_error().as_deref(), Some(msg.as_str()));
    }

    #[test]
    fn last_error_describes_the_first_bad_argument() {
        assert_eq!(
            unsafe { bugsee_trace(std::ptr::null(), std::ptr::null()) },
            BugseeStatus::InvalidArgument
        );
        assert!(last_error().unwrap().contains("name must not be NULL"));
        assert_eq!(
            unsafe { bugsee_set_attribute(std::ptr::null(), std::ptr::null()) },
            BugseeStatus::InvalidArgument
        );
        assert!(last_error().unwrap().contains("key must not be NULL"));
        assert_eq!(
            unsafe { bugsee_capture_exception(c"\xff".as_ptr(), c"\xfe".as_ptr()) },
            BugseeStatus::InvalidArgument
        );
        assert!(last_error().unwrap().contains("name is not valid UTF-8"));
    }

    #[test]
    fn panicking_payload_destructor_is_contained() {
        struct Bomb;
        impl Drop for Bomb {
            fn drop(&mut self) {
                if !std::thread::panicking() {
                    panic!("drop bomb");
                }
            }
        }
        let status = guarded(|| std::panic::panic_any(Bomb));
        assert_eq!(status, BugseeStatus::Panic);
    }

    #[test]
    fn abi_and_version_queries() {
        assert_eq!(bugsee_abi_version(), BUGSEE_ABI_VERSION);
        let v = unsafe { CStr::from_ptr(bugsee_version()) }
            .to_str()
            .unwrap();
        assert_eq!(v, env!("CARGO_PKG_VERSION"));
        for (code, text) in [
            (0, "ok"),
            (1, "invalid argument"),
            (2, "internal error"),
            (3, "panic contained at the FFI boundary"),
            (4, "SDK not launched"),
            (5, "timed out"),
            (6, "ABI version mismatch"),
        ] {
            let s = unsafe { CStr::from_ptr(bugsee_status_string(code)) };
            assert_eq!(s.to_str().unwrap(), text);
        }
        for code in [-1, 7, i32::MAX] {
            let s = unsafe { CStr::from_ptr(bugsee_status_string(code)) };
            assert_eq!(s.to_str().unwrap(), "unknown status");
        }
    }

    #[test]
    fn options_new_checks_abi_and_arguments() {
        let mut out = std::ptr::dangling_mut::<BugseeOptions>();
        let status = unsafe { bugsee_options_new(BUGSEE_ABI_VERSION + 1, c"t".as_ptr(), &mut out) };
        assert_eq!(status, BugseeStatus::AbiMismatch);
        assert!(out.is_null(), "*out is cleared on failure");
        assert!(last_error().unwrap().contains("ABI version"));

        let status = unsafe { bugsee_options_new(BUGSEE_ABI_VERSION, std::ptr::null(), &mut out) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        assert!(out.is_null());
        let status =
            unsafe { bugsee_options_new(BUGSEE_ABI_VERSION, c"t".as_ptr(), std::ptr::null_mut()) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        let status = unsafe { bugsee_options_new(BUGSEE_ABI_VERSION, c"\xff".as_ptr(), &mut out) };
        assert_eq!(status, BugseeStatus::InvalidArgument);
        assert!(out.is_null());
    }

    #[test]
    fn options_setters_round_trip_into_launch_options() {
        let handle = new_options(c"tok");
        unsafe {
            assert_eq!(
                bugsee_options_set_data_dir(handle, c"/data/x".as_ptr()),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_endpoint(handle, c"http://e".as_ptr()),
                BugseeStatus::Ok
            );
            assert_eq!(bugsee_options_set_scrubbing(handle, 0), BugseeStatus::Ok);
            assert_eq!(
                bugsee_options_set_rate_limiting(handle, 0),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_release_health(handle, 1),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_native_crash_capture(handle, 0),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_sample_rate(handle, 0.25),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_launch_recovery_budget_ms(handle, 1500),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_flush_at_exit_ms(handle, 2000),
                BugseeStatus::Ok
            );
            let o = &*handle;
            assert_eq!(o.data_dir.as_deref(), Some(std::path::Path::new("/data/x")));
            assert_eq!(o.endpoint.as_deref(), Some("http://e"));
            assert_eq!(o.scrubbing, Some(false));
            assert_eq!(o.rate_limiting, Some(false));
            assert_eq!(o.release_health, Some(true));
            assert_eq!(o.native_crash_capture, Some(false));
            assert_eq!(o.sample_rate, Some(0.25));
            assert_eq!(o.launch_recovery_budget, Some(Duration::from_millis(1500)));
            assert_eq!(o.flush_at_exit, Some(Duration::from_millis(2000)));
            let _ = o.to_launch_options();

            // NULL resets a string; 0 disables flush-at-exit.
            assert_eq!(
                bugsee_options_set_data_dir(handle, std::ptr::null()),
                BugseeStatus::Ok
            );
            assert_eq!(
                bugsee_options_set_flush_at_exit_ms(handle, 0),
                BugseeStatus::Ok
            );
            assert!((*handle).data_dir.is_none());
            assert!((*handle).flush_at_exit.is_none());

            bugsee_options_free(handle);
            bugsee_options_free(std::ptr::null_mut());
        }
    }

    #[test]
    fn options_setters_reject_bad_input() {
        let handle = new_options(c"tok");
        unsafe {
            assert_eq!(
                bugsee_options_set_data_dir(handle, c"\xff".as_ptr()),
                BugseeStatus::InvalidArgument
            );
            assert!((*handle).data_dir.is_none());
            for rate in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
                assert_eq!(
                    bugsee_options_set_sample_rate(handle, rate),
                    BugseeStatus::InvalidArgument,
                    "rate {rate}"
                );
            }
            assert_eq!(
                bugsee_options_set_scrubbing(std::ptr::null_mut(), 1),
                BugseeStatus::InvalidArgument
            );
            assert_eq!(
                bugsee_launch_with_options(std::ptr::null()),
                BugseeStatus::InvalidArgument
            );
            bugsee_options_free(handle);
        }
    }

    #[test]
    fn report_error_validates_documents() {
        let send = |s: &[u8]| unsafe { bugsee_report_error(s.as_ptr().cast(), s.len()) };
        assert_eq!(
            unsafe { bugsee_report_error(std::ptr::null(), 4) },
            BugseeStatus::InvalidArgument
        );
        assert_eq!(send(b""), BugseeStatus::InvalidArgument);
        assert_eq!(send(b"not json"), BugseeStatus::InvalidArgument);
        assert_eq!(send(b"[]"), BugseeStatus::InvalidArgument);
        assert_eq!(send(b"\xff\xfe"), BugseeStatus::InvalidArgument);
        assert_eq!(send(br#"{"name": 5}"#), BugseeStatus::InvalidArgument);
        assert_eq!(
            send(br#"{"frames": [{"line": 1}]}"#),
            BugseeStatus::InvalidArgument
        );
        assert_eq!(
            send(br#"{"frames": [{"function": "f", "line": "x"}]}"#),
            BugseeStatus::InvalidArgument
        );
        assert!(last_error().unwrap().contains("frames[0]"));
        assert_eq!(send(br#"{"causes": {}}"#), BugseeStatus::InvalidArgument);
        // Entries past the caps are still validated.
        let mut late_bad = String::from(r#"{"frames":["#);
        for _ in 0..MAX_HOST_FRAMES {
            late_bad.push_str(r#"{"function":"f"},"#);
        }
        late_bad.push_str(r#"{"line":1}]}"#);
        assert_eq!(send(late_bad.as_bytes()), BugseeStatus::InvalidArgument);
        // Length is honoured: a NUL-free slice shorter than the buffer is fine.
        assert_eq!(
            send(br#"{"name":"E"} trailing"#),
            BugseeStatus::InvalidArgument
        );
        let big = vec![b' '; MAX_ERROR_JSON + 1];
        assert_eq!(send(&big), BugseeStatus::InvalidArgument);
        // Valid, with defaults and unknown fields. Not launched here: a no-op.
        assert_eq!(send(br#"{"extra": true}"#), BugseeStatus::Ok);
    }

    #[test]
    fn host_error_maps_frames_and_causes() {
        let e = parse_host_error(
            r#"{"name":"RuntimeError","reason":"boom",
                "causes":[{"name":"IOError","reason":"disk full"},{}],
                "frames":[
                  {"function":"run","module":"App.Job","file":"job.php","line":42},
                  {"function":"vendor_call","in_app":false},
                  {"function":"main","file":"main.php"}]}"#,
        )
        .unwrap();
        assert_eq!(e.name, "RuntimeError");
        assert_eq!(e.causes.len(), 2);
        assert_eq!(e.causes[1].name, "Error");
        assert_eq!(e.frames.len(), 3);
        assert_eq!(e.frames[0].trace, "App.Job::run (job.php:42)");
        assert!(!e.frames[0].hidden);
        assert_eq!(e.frames[0].data.line, 42);
        assert_eq!(e.frames[0].data.member_class.as_deref(), Some("App.Job"));
        assert!(e.frames[1].hidden);
        assert_eq!(e.frames[1].trace, "vendor_call");
        assert_eq!(e.frames[1].data.line, -1);
        assert_eq!(e.frames[2].trace, "main (main.php)");
    }

    #[test]
    fn host_error_caps_frames_and_causes() {
        let frames: Vec<String> = (0..600)
            .map(|i| format!(r#"{{"function":"f{i}"}}"#))
            .collect();
        let causes: Vec<&str> = (0..40).map(|_| "{}").collect();
        let json = format!(
            r#"{{"frames":[{}],"causes":[{}]}}"#,
            frames.join(","),
            causes.join(",")
        );
        let e = parse_host_error(&json).unwrap();
        assert_eq!(e.frames.len(), MAX_HOST_FRAMES);
        assert_eq!(e.causes.len(), MAX_HOST_CAUSES);
        assert_eq!(e.frames[0].data.member.as_deref(), Some("f0"));
    }
}

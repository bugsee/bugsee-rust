//
//  crash.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `crash.json` — the exception payload. This is the managed-runtime variant
//! used for Rust panics and handled errors (`exception` + `frames`, optional
//! `cause` chain). The thin native variant (signal + minidump reference) is
//! added in Phase 3. See report-bundle-structure §4.14.

use serde::{Deserialize, Serialize};

/// Structured frame metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// Source line; `-1` unknown, `-2` native.
    pub line: i64,
}

/// One stack frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// `crate::module::function (file:line)` display form.
    pub trace: String,
    /// `true` marks leading SDK-internal frames.
    pub hidden: bool,
    pub data: FrameData,
}

/// Whether a resolved symbol belongs to the SDK or the Rust panic/unwind
/// runtime rather than to application code.
///
/// Such frames are marked `hidden` so the backend's "first non-hidden frame"
/// rule reports the **user's crash site** — both for the displayed location and
/// for grouping. Getting this wrong is not cosmetic: an unhidden runtime frame
/// makes every panic in a binary look like it crashed in the same place (the
/// panic machinery), collapsing distinct bugs into one group.
///
/// Deliberately conservative — it matches only unambiguous SDK/runtime paths, so
/// it can never hide the application frame we are trying to surface. Shared by
/// the panic observer and the handled-error capture path so the two cannot drift.
pub fn is_internal_frame(name: &str) -> bool {
    /// Our own crates, matched on the FULL first path segment.
    ///
    /// A bare `starts_with("bugsee")` also swallows an application crate named
    /// e.g. `bugsee_e2e_app` or `bugsee_integration` — hiding the very frame we
    /// exist to report. Compare whole segments so only our crates match.
    const OUR_CRATES: &[&str] = &[
        "bugsee",
        "bugsee_core",
        "bugsee_panic",
        "bugsee_native",
        "bugsee_ffi",
        "bugsee_reqwest",
        "bugsee_tracing",
        "bugsee_log",
    ];

    // Trait-impl symbols render as `<T as Trait>::method`; look past the `<`.
    let root = name.trim_start_matches('<');
    let first_segment = root.split("::").next().unwrap_or("");
    if OUR_CRATES.contains(&first_segment) {
        return true;
    }

    // Compiler-internal shims live in a hashed namespace, e.g.
    // `__rustc[16f1505adc47261a]::rust_begin_unwind` — the frame the panic
    // runtime actually unwinds through on current toolchains. Match the segment
    // so the hash (which varies per build) is irrelevant.
    if first_segment.starts_with("__rustc") {
        return true;
    }

    // The symbolication machinery.
    if name.starts_with("backtrace::") || name.starts_with("std::backtrace") {
        return true;
    }

    // The panic/unwind runtime. `std::panicking` / `core::panicking` are matched
    // by the `std::panic` / `core::panic` prefixes.
    if name.starts_with("std::panic")
        || name.starts_with("core::panic")
        || name.starts_with("std::rt::")
        || name.starts_with("std::sys::backtrace")
        || name.starts_with("rust_begin_unwind")
        || name.starts_with("__rust_")
        || name.starts_with("_Unwind_")
    {
        return true;
    }

    // The hook/closure dispatch trampolines the panic runtime goes through, e.g.
    // `<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call` — this is
    // how the installed panic hook is invoked, and it sits directly above the
    // application frame, so leaving it visible hides the real crash site.
    if name.starts_with("<alloc::boxed::Box<") || name.starts_with("core::ops::function::") {
        return true;
    }

    false
}

/// A single exception in the (possibly nested) exception chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionInfo {
    pub name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub frames: Vec<Frame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ExceptionInfo>>,
}

/// The `crash.json` document (managed variant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashReport {
    /// Which SDK produced this crash document — always [`SOURCE_SDK`].
    ///
    /// Makes `crash.json` **self-describing** so the backend can pick its
    /// processor from the document itself. Routing previously depended solely on
    /// `request.json`'s `environment.sdk.type`, but the two do not travel
    /// together: on the worker's resymbolication path `crash.json` is fetched
    /// from S3 while `environment` comes from a separate API/DB read, so a
    /// missing or partial environment leaves the crash unroutable (or
    /// mis-routed to a platform-guessing fallback).
    ///
    /// Consumers treat this as authoritative and fall back to
    /// `environment.sdk.type` when absent, so documents from SDKs that do not
    /// yet emit it keep working unchanged.
    pub source_sdk: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub timestamp: i64,
    /// `false` = uncaught; `true` = logged/handled.
    pub handled: bool,
    /// Whether app symbols are obfuscated (affects server symbolication).
    pub obfuscated: bool,
    #[serde(rename = "ndkCrash")]
    pub ndk_crash: bool,
    /// `"exception"` | `"error"` | `"throwable"` for managed variants.
    pub exception_type: String,
    /// 0 or 1 lowercase SHA-1 hex dedup signature.
    pub signatures: Vec<String>,
    pub exception: ExceptionInfo,
}

/// The value every `crash.json` this SDK writes carries in `source_sdk`.
/// Deliberately the same identifier as `environment.sdk.type` (see
/// [`crate::model::environment::SDK_TYPE`]) so the two can never disagree.
pub const SOURCE_SDK: &str = crate::model::environment::SDK_TYPE;

impl CrashReport {
    /// Build a handled-error crash payload (`handled=true`, managed variant).
    pub fn handled_error(
        name: impl Into<String>,
        reason: impl Into<String>,
        frames: Vec<Frame>,
        signature: Option<String>,
        timestamp: i64,
    ) -> Self {
        CrashReport {
            source_sdk: SOURCE_SDK.to_string(),
            uuid: None,
            timestamp,
            handled: true,
            obfuscated: false,
            ndk_crash: false,
            exception_type: "error".into(),
            signatures: signature.into_iter().collect(),
            exception: ExceptionInfo {
                name: name.into(),
                reason: reason.into(),
                domain: None,
                frames,
                cause: None,
            },
        }
    }

    /// Serialize to `crash.json` bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_the_panic_runtime_and_hook_trampolines() {
        // The exact frame the SDK↔worker E2E harness caught being reported as the
        // crash site instead of the user's code: the panic hook is invoked
        // through a boxed-closure call, which sits directly above app code.
        assert!(is_internal_frame(
            "<alloc::boxed::Box<F,A> as core::ops::function::Fn<Args>>::call::h21e5ac2990050c81"
        ));
        for name in [
            "std::panicking::begin_panic",
            "std::panicking::rust_panic_with_hook",
            "core::panicking::panic_fmt",
            "rust_begin_unwind",
            // The hashed compiler-shim namespace on current toolchains — this
            // exact frame was the one leaking through as the "crash site".
            "__rustc[16f1505adc47261a]::rust_begin_unwind",
            "__rustc::rust_begin_unwind",
            "__rust_start_panic",
            "_Unwind_RaiseException",
            "std::sys::backtrace::__rust_begin_short_backtrace",
            "std::rt::lang_start_internal",
            "core::ops::function::FnOnce::call_once",
            "bugsee_panic::capture_frames",
            "bugsee::frames::capture",
            "backtrace::backtrace::trace",
        ] {
            assert!(is_internal_frame(name), "{name} must be hidden");
        }
    }

    #[test]
    fn keeps_application_frames_visible() {
        // Conservative by design — it must never hide the frame we exist to report.
        for name in [
            "bugsee_e2e_app::main",
            "myapp::checkout::pay",
            "my_crate::handler::process_request",
            // A user type whose path merely resembles a runtime one.
            "corelib::panic_button::press",
            "alloc_tracker::record",
            "std_helper::run",
            // An application crate whose name merely BEGINS with "bugsee" — a
            // bare prefix match would hide the user's own crash site.
            "bugsee_integration::checkout",
            "bugseeclient::run",
        ] {
            assert!(!is_internal_frame(name), "{name} must stay visible");
        }
    }
}

//
//  origin.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `source.origin` for `code_upload` reports: the first non-Bugsee call-stack
//! frame, so an app that uploads from several places can tell the sites apart.
//! Mirrors Android's `digestOriginFromStackTrace` and iOS's
//! `digestOriginFromCallStack`. Crash/error reports carry a full stack already
//! and leave `origin` null.

use std::backtrace::Backtrace;

/// Crates whose frames are part of the SDK itself and never the caller.
const SDK_CRATES: [&str; 2] = ["bugsee", "bugsee_core"];

/// Standard-library crates; their frames sit above the SDK's on a capture.
const STD_CRATES: [&str; 3] = ["std", "core", "alloc"];

/// Digest the call site of the code that is uploading a report, as
/// `path::to::function:line` (just `path::to::function` when the build carries
/// no line info). `None` when no frame resolves to a symbol, e.g. a stripped
/// release build — never a bare address.
pub fn capture_origin() -> Option<String> {
    // `force_capture` ignores RUST_BACKTRACE; this only runs on a code upload.
    digest_origin(&Backtrace::force_capture().to_string())
}

/// Pick the origin out of a rendered `std::backtrace::Backtrace` (its stable
/// `Display` form is the only way to read frames on stable Rust):
///
/// ```text
///    0: bugsee_core::origin::capture_origin
///              at ./src/origin.rs:24:5
///    1: 0x55d0c8a1b2c3 - <unknown>
/// ```
fn digest_origin(rendered: &str) -> Option<String> {
    let mut lines = rendered.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(symbol) = frame_symbol(line) else {
            continue;
        };
        // The optional `at file:line:col` line belongs to this frame.
        let location = lines
            .next_if(|l| l.trim_start().starts_with("at "))
            .and_then(|l| line_number(l.trim_start().trim_start_matches("at ")));
        if is_sdk_frame(symbol) || is_unresolved(symbol) {
            continue;
        }
        return Some(match location {
            Some(n) => format!("{symbol}:{n}"),
            None => symbol.to_string(),
        });
    }
    None
}

/// The symbol text of a `  N: symbol` frame line, `None` for anything else.
fn frame_symbol(line: &str) -> Option<&str> {
    let (index, rest) = line.trim_start().split_once(':')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(rest.trim())
}

/// `file:line:col` -> `line` (paths may hold colons, e.g. `C:\`).
fn line_number(location: &str) -> Option<u32> {
    let mut parts = location.rsplitn(3, ':');
    let _col = parts.next()?;
    parts.next()?.parse().ok()
}

/// Whether the symbol lives in the SDK or in the standard library (the
/// backtrace machinery tops every capture), neither of which is the caller.
fn is_sdk_frame(symbol: &str) -> bool {
    // Trait-impl frames render as `<bugsee::Type as Trait>::method`.
    let path = symbol.trim_start_matches('<');
    SDK_CRATES
        .iter()
        .chain(&STD_CRATES)
        .any(|c| path.strip_prefix(c).is_some_and(|r| r.starts_with("::")))
}

/// A frame std could not symbolize prints as `0x... - <unknown>`.
fn is_unresolved(symbol: &str) -> bool {
    symbol.starts_with("0x") || symbol.contains("<unknown>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_sdk_frames_and_picks_first_caller() {
        let bt = "   0: bugsee_core::origin::capture_origin\n             at ./src/origin.rs:24:5\n   \
                  1: bugsee_core::reporting::manual_upload_meta\n             at ./src/reporting.rs:1:1\n   \
                  2: bugsee::api::Bugsee::upload::{{closure}}\n             at ./src/api.rs:9:9\n   \
                  3: my_app::checkout::submit\n             at ./src/checkout.rs:42:13\n   \
                  4: my_app::main\n             at ./src/main.rs:7:5\n";
        assert_eq!(
            digest_origin(bt).as_deref(),
            Some("my_app::checkout::submit:42")
        );
    }

    #[test]
    fn skips_std_backtrace_frames_on_top() {
        let bt = "   0: std::backtrace_rs::backtrace::libunwind::trace\n             at /rustc/x/trace.rs:117:9\n   \
                  1: std::backtrace::Backtrace::create\n   \
                  2: bugsee_core::origin::capture_origin\n   \
                  3: app::go\n             at ./go.rs:5:2\n";
        assert_eq!(digest_origin(bt).as_deref(), Some("app::go:5"));
    }

    #[test]
    fn similarly_named_crates_are_not_sdk_frames() {
        let bt = "   0: bugsee_demo::run\n             at /a/b.rs:3:1\n";
        assert_eq!(digest_origin(bt).as_deref(), Some("bugsee_demo::run:3"));
    }

    #[test]
    fn trait_impl_frames_of_the_sdk_are_skipped() {
        let bt = "   0: <bugsee::report::Report as core::default::Default>::default\n   \
                  1: app::go\n             at C:\\src\\go.rs:5:2\n";
        assert_eq!(digest_origin(bt).as_deref(), Some("app::go:5"));
    }

    #[test]
    fn rejects_bare_addresses() {
        let bt = "   0: bugsee::api::Bugsee::upload\n   1: 0x55d0c8a1b2c3 - <unknown>\n";
        assert_eq!(digest_origin(bt), None);
        // A later symbolized frame is still found past an unresolved one.
        let bt = "   0: 0x55d0c8a1b2c3 - <unknown>\n   1: app::go\n";
        assert_eq!(digest_origin(bt).as_deref(), Some("app::go"));
    }

    #[test]
    fn disabled_or_empty_backtrace_yields_none() {
        assert_eq!(digest_origin("disabled backtrace"), None);
        assert_eq!(digest_origin(""), None);
    }
}

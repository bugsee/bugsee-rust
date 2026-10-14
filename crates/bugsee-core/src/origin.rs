//
//  origin.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `source.origin` for `code_upload` reports: the first call-stack frame above
//! the SDK, so an app that uploads from several places can tell the sites apart.
//! Mirrors Android's `digestOriginFromStackTrace` and iOS's
//! `digestOriginFromCallStack`. Crash/error reports carry a full stack already
//! and leave `origin` null.
//!
//! Best effort by design: the answer depends on the build's symbols, and an
//! unreliable answer is worse than none, so every doubtful case yields `None`.

use backtrace::SymbolName;

use crate::model::crash::is_internal_frame;

/// Longest origin kept; a deeply nested generic can render very long.
const MAX_ORIGIN_LEN: usize = 256;
/// Frames inspected before giving up.
const MAX_FRAMES: usize = 64;

/// SDK crate directory names, for recognising SDK frames by source path when a
/// build's symbol names are unqualified (e.g. `line-tables-only`).
const SDK_DIRS: [&str; 8] = [
    "bugsee",
    "bugsee-core",
    "bugsee-panic",
    "bugsee-native",
    "bugsee-ffi",
    "bugsee-reqwest",
    "bugsee-tracing",
    "bugsee-log",
];

/// One resolved symbol of a frame. A physical frame with inlined calls yields
/// several, innermost first.
#[derive(Debug, Clone, Default)]
struct Sym {
    name: Option<String>,
    file: Option<String>,
    line: Option<u32>,
}

/// Digest the call site of the code that is uploading a report, as
/// `path::to::function:line` (just the function when the build carries no line
/// info). `None` when it cannot be determined with confidence — a stripped
/// build, or a caller that is not Rust code (an FFI host).
pub fn capture_origin() -> Option<String> {
    // The `backtrace` crate's process-wide mutex does not survive `fork()`;
    // this registered lock makes a fork wait out the walk (see `lock_backtrace`).
    // Only code uploads pay for it.
    let _walk = crate::fork::lock_backtrace();
    let mut syms: Vec<Sym> = Vec::new();
    let mut frames = 0;
    let mut result = None;
    backtrace::trace(|frame| {
        // Resolve one frame at a time and stop as soon as the answer is known;
        // symbolizing the whole stack costs milliseconds.
        let mut resolved = Vec::new();
        backtrace::resolve_frame(frame, |symbol| {
            resolved.push(Sym {
                name: symbol.name().map(display_name),
                file: symbol.filename().map(|p| p.to_string_lossy().into_owned()),
                line: symbol.lineno(),
            });
        });
        push_frame(&mut syms, resolved);
        frames += 1;
        match scan(&syms) {
            Scan::Done(origin) => {
                result = origin;
                false
            }
            Scan::NeedMore => frames < MAX_FRAMES,
        }
    });
    result
}

/// Append one physical frame's symbols. `resolve_frame` may call back zero times
/// when it cannot symbolize; that frame still exists, so record it as unknown
/// rather than dropping it (which would make its caller look adjacent to the SDK).
fn push_frame(syms: &mut Vec<Sym>, resolved: Vec<Sym>) {
    if resolved.is_empty() {
        syms.push(Sym::default());
    } else {
        syms.extend(resolved);
    }
}

/// Rendered without the rustc hash suffix (`{:#}`), both mangling schemes.
fn display_name(name: SymbolName<'_>) -> String {
    format!("{name:#}")
}

enum Scan {
    /// The answer is settled: an origin, or `None` for "cannot tell".
    Done(Option<String>),
    /// Every symbol so far is SDK or runtime; look deeper.
    NeedMore,
}

/// Pick the origin from symbols ordered innermost first.
///
/// Nothing is accepted until an SDK frame has been seen, so frames above the SDK
/// (the unwinder, std's backtrace machinery) can never be mistaken for the
/// caller. The first frame after the SDK's is then the answer, or else `None`;
/// walking further down would only name runtime internals.
fn scan(syms: &[Sym]) -> Scan {
    let mut seen_sdk = false;
    for sym in syms {
        if is_sdk_frame(sym) {
            seen_sdk = true;
            continue;
        }
        if !seen_sdk {
            continue;
        }
        // The std/core/alloc and compiler-shim frames between the SDK and the
        // caller (closure dispatch, `FnOnce::call_once`) are transparent.
        if sym.name.as_deref().is_some_and(is_internal_frame) {
            continue;
        }
        return Scan::Done(format_origin(sym));
    }
    Scan::NeedMore
}

fn is_sdk_frame(sym: &Sym) -> bool {
    match sym.name.as_deref() {
        // A qualified symbol is authoritative: an app crate whose sources sit in
        // a directory merely named like an SDK crate is still the app.
        Some(n) if n.contains("::") => is_internal_frame(n) && !is_std_runtime(n),
        // Unqualified (`line-tables-only`) or unresolved names: fall back to the
        // source path.
        _ => sym.file.as_deref().is_some_and(in_sdk_dir),
    }
}

/// std/core/alloc and compiler shims: internal to the toolchain, not the SDK.
fn is_std_runtime(name: &str) -> bool {
    let root = name.trim_start_matches('<');
    [
        "std::",
        "core::",
        "alloc::",
        "__rust",
        "_Unwind_",
        "rust_begin_",
    ]
    .iter()
    .any(|p| root.starts_with(p))
}

/// Whether a source path lies inside one of the SDK crates (`<dir>/src/...`,
/// also a registry checkout like `bugsee-core-0.1.0/src/...`).
fn in_sdk_dir(path: &str) -> bool {
    let path = path.replace('\\', "/");
    let parts: Vec<&str> = path.split('/').collect();
    parts.windows(2).any(|w| {
        w[1] == "src"
            && SDK_DIRS.iter().any(|d| {
                w[0] == *d
                    || w[0]
                        .strip_prefix(d)
                        .and_then(|rest| rest.strip_prefix('-'))
                        .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            })
    })
}

/// The origin string for the caller's symbol, or `None` when it does not look
/// like Rust application code (libc/pthread internals under an FFI host, std
/// plumbing, or an unresolved address — never emit those).
fn format_origin(sym: &Sym) -> Option<String> {
    let name = sym.name.as_deref()?;
    let qualified = name.contains("::");
    let rust_file = sym.file.as_deref().is_some_and(|f| f.ends_with(".rs"));
    // Unqualified names (`line-tables-only`) are only trusted with a source file;
    // without one `__pthread_cond_wait`, `main` or `__mh_execute_header` would pass.
    if !(qualified || rust_file) || name.starts_with("0x") || name.contains("<unknown>") {
        return None;
    }
    if is_std_runtime(name) {
        return None;
    }
    let mut origin = match sym.line {
        Some(line) => format!("{name}:{line}"),
        None => name.to_string(),
    };
    if origin.len() > MAX_ORIGIN_LEN {
        let mut end = MAX_ORIGIN_LEN;
        while !origin.is_char_boundary(end) {
            end -= 1;
        }
        origin.truncate(end);
    }
    Some(origin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, file: Option<&str>, line: Option<u32>) -> Sym {
        Sym {
            name: Some(name.into()),
            file: file.map(Into::into),
            line,
        }
    }

    fn origin(syms: &[Sym]) -> Option<String> {
        match scan(syms) {
            Scan::Done(o) => o,
            Scan::NeedMore => None,
        }
    }

    #[test]
    fn skips_sdk_and_runtime_frames_and_picks_first_caller() {
        let syms = [
            sym("backtrace::backtrace::libunwind::trace", None, None),
            sym("bugsee_core::origin::capture_origin", None, None),
            sym("bugsee_core::reporting::manual_upload_meta", None, None),
            sym("bugsee::api::Bugsee::upload::{{closure}}", None, None),
            sym("core::ops::function::FnOnce::call_once", None, None),
            sym("my_app::checkout::submit", Some("/a/checkout.rs"), Some(42)),
            sym("my_app::main", Some("/a/main.rs"), Some(7)),
        ];
        assert_eq!(
            origin(&syms).as_deref(),
            Some("my_app::checkout::submit:42")
        );
    }

    #[test]
    fn app_under_an_sdk_named_directory_is_still_the_caller() {
        // Crate `my_app` checked out at /home/me/bugsee/: the qualified symbol wins.
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            sym("my_app::run", Some("/home/me/bugsee/src/main.rs"), Some(12)),
        ];
        assert_eq!(origin(&syms).as_deref(), Some("my_app::run:12"));
    }

    #[test]
    fn a_frame_that_failed_to_resolve_is_kept_as_unknown() {
        // `resolve_frame` calls back zero times for it; the parent must not be
        // promoted to the caller.
        let mut syms = vec![sym("bugsee::api::Bugsee::upload", None, None)];
        push_frame(&mut syms, Vec::new());
        push_frame(
            &mut syms,
            vec![sym("app::parent", Some("/a/p.rs"), Some(1))],
        );
        assert_eq!(syms.len(), 3);
        assert_eq!(origin(&syms), None);
    }

    #[test]
    fn nothing_is_accepted_before_an_sdk_frame() {
        // Stack that never mentions the SDK: cannot tell, so no origin.
        let syms = [sym("app::go", Some("/a/go.rs"), Some(1))];
        assert!(matches!(scan(&syms), Scan::NeedMore));
        assert_eq!(origin(&syms), None);
    }

    #[test]
    fn missing_line_keeps_the_symbol_alone() {
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            sym("app::go", None, None),
        ];
        assert_eq!(origin(&syms).as_deref(), Some("app::go"));
    }

    #[test]
    fn unqualified_sdk_frames_are_recognised_by_source_path() {
        // `line-tables-only`: names lose their crate path, files remain.
        let syms = [
            sym(
                "capture_origin",
                Some("/w/crates/bugsee-core/src/origin.rs"),
                Some(28),
            ),
            sym(
                "manual_upload_meta",
                Some("C:\\w\\crates\\bugsee-core\\src\\reporting.rs"),
                Some(9),
            ),
            sym("upload", Some("/reg/bugsee-0.1.0/src/api.rs"), Some(305)),
            sym("submit", Some("/app/src/checkout.rs"), Some(42)),
        ];
        assert_eq!(origin(&syms).as_deref(), Some("submit:42"));
    }

    #[test]
    fn app_paths_resembling_sdk_dirs_are_not_sdk() {
        assert!(!in_sdk_dir("/home/me/bugsee_demo/src/main.rs"));
        assert!(!in_sdk_dir("/home/me/bugsee/main.rs"));
        assert!(in_sdk_dir("/home/me/bugsee-ffi/src/lib.rs"));
    }

    #[test]
    fn similarly_named_app_crates_are_callers() {
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            sym("bugsee_demo::run", Some("/a/b.rs"), Some(3)),
        ];
        assert_eq!(origin(&syms).as_deref(), Some("bugsee_demo::run:3"));
    }

    #[test]
    fn inlined_calls_resolve_innermost_first() {
        // One physical frame, upload() inlined into `app::go`: both symbols
        // arrive in order, the SDK's first.
        let syms = [
            sym("bugsee_core::origin::capture_origin", None, None),
            sym("bugsee::api::Bugsee::upload", None, None),
            sym("app::go", Some("/a/go.rs"), Some(5)),
            sym("app::main", Some("/a/main.rs"), Some(1)),
        ];
        assert_eq!(origin(&syms).as_deref(), Some("app::go:5"));
    }

    #[test]
    fn trait_impl_sdk_frames_are_skipped() {
        let syms = [
            sym(
                "<bugsee::report::Report as core::default::Default>::default",
                None,
                None,
            ),
            sym("<app::Job as app::Run>::run", Some("/a/job.rs"), Some(8)),
        ];
        assert_eq!(
            origin(&syms).as_deref(),
            Some("<app::Job as app::Run>::run:8")
        );
    }

    #[test]
    fn ffi_host_callers_yield_none() {
        // The first frame past the SDK is runtime/libc, not Rust code; do not
        // walk further down to find something to blame.
        for name in [
            "__pthread_cond_wait",
            "__mh_execute_header",
            "main",
            "start",
        ] {
            let syms = [
                sym("bugsee_ffi::bugsee_upload::{{closure}}", None, None),
                sym(name, None, None),
                sym("app::deeper", Some("/a/b.rs"), Some(1)),
            ];
            assert_eq!(origin(&syms), None, "{name}");
        }
        let syms = [
            sym("bugsee_ffi::bugsee_upload", None, None),
            sym(
                "std::thread::Builder::spawn_unchecked_",
                Some("/r/thread.rs"),
                Some(1),
            ),
        ];
        assert_eq!(origin(&syms), None);
    }

    #[test]
    fn unresolved_frame_after_the_sdk_yields_none() {
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            Sym::default(),
            sym("app::go", Some("/a/go.rs"), Some(1)),
        ];
        assert_eq!(origin(&syms), None);
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            sym("0x55d0c8a1b2c3 - <unknown>", None, None),
        ];
        assert_eq!(origin(&syms), None);
    }

    #[test]
    fn origin_is_capped() {
        let long = format!("app::{}", "x".repeat(1000));
        let syms = [
            sym("bugsee::api::Bugsee::upload", None, None),
            sym(&long, Some("/a/b.rs"), Some(1)),
        ];
        assert_eq!(origin(&syms).unwrap().len(), MAX_ORIGIN_LEN);
    }

    #[test]
    fn names_render_without_hash_for_legacy_and_v0() {
        let legacy = SymbolName::new(b"_ZN3app2go17h0123456789abcdefE");
        assert_eq!(display_name(legacy), "app::go");
        let v0 = SymbolName::new(b"_RNvCs1234_3app2go");
        assert_eq!(display_name(v0), "app::go");
    }
}

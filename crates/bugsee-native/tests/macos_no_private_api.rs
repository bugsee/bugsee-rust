//
//  macos_no_private_api.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The macOS crash handler must not reference private Apple API.
//!
//! Mac App Store review (and sandboxed distribution generally) flags binaries
//! that resolve private symbols. The `crash-handler` crate did exactly that on
//! macOS — it `dlsym`s libproc's `proc_get_wakemon_params`, which has no public
//! header — so the SDK's own signal handler replaced it there. This is a cheap
//! tripwire over the bytes of the `bugsee-native` test binary: it catches that
//! symbol name (import or `dlsym` string) and the `crash-handler` crate coming
//! back (its embedded panic-location paths contain `crash-handler-0.`). It does
//! NOT inspect the shipped library; the authoritative check is the macOS CI step
//! that runs `nm -u` on the release `libbugsee_ffi.dylib`.

#![cfg(target_os = "macos")]

use std::path::PathBuf;

/// Needles are stored reversed so this file's own literals (which sit next to
/// each other in the binary's string pool) are not what the scan finds.
fn denylist() -> Vec<String> {
    [
        // libproc's private `proc_get_wakemon_params`.
        "smarap_nomekaw_teg_corp",
        // The crate whose macOS backend resolves it (Windows-only in this repo);
        // its versioned source path is embedded in any binary that links it.
        ".0-reldnah-hsarc",
    ]
    .iter()
    .map(|r| r.chars().rev().collect())
    .collect()
}

#[test]
fn binary_has_no_private_symbol_references() {
    // Force the handler into the link so its code is part of what is scanned.
    let install: fn(PathBuf) -> std::io::Result<bugsee_native::NativeHandler> =
        bugsee_native::install;
    std::hint::black_box(install);

    let exe = std::env::current_exe().expect("current_exe");
    let bytes = std::fs::read(&exe).expect("read test binary");

    for needle in denylist() {
        let hit = bytes.windows(needle.len()).any(|w| w == needle.as_bytes());
        assert!(!hit, "{} references private API `{needle}`", exe.display());
    }
}

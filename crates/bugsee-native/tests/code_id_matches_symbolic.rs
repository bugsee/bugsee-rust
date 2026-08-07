//
//  code_id_matches_symbolic.rs
//  bugsee-native
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The identity a crash reports must equal the identity an upload is keyed on.
//!
//! This is the single assumption the whole symbol pipeline rests on, and the one
//! that fails silently. A wrong `code_id` is not malformed — it is a
//! plausible-looking hex string that simply matches no symbol file ever
//! uploaded, so symbolication yields nothing and no error is raised anywhere. It
//! is exactly how native crashes were unsymbolicatable before `code_id` existed,
//! and it took an end-to-end run against a live deployment to notice.
//!
//! So this checks our arithmetic against the AUTHORITY rather than against
//! another copy of itself: `symbolic-debuginfo` is the crate `bugsee-cli` uses
//! to key an upload and the worker uses to key the symbol store. If the two
//! agree here, an upload can match; if they disagree, nothing downstream ever
//! will.
//!
//! Note the two sides deliberately read DIFFERENT things. `snapshot_modules`
//! parses the module as MAPPED (data-directory RVAs), which is all a crash
//! handler has; `symbolic` parses the file on disk. A test that fed our parser
//! the on-disk bytes would be exercising file offsets against a parser written
//! for RVAs — and could pass by accident on a binary whose file and section
//! alignments happen to coincide.

use std::path::Path;

/// The canonical storage form, mirroring the backend's `canonical()`:
/// lowercase, no dashes. Both sides are reduced to it before comparing, because
/// the pipeline compares them that way too.
fn canonical(id: &str) -> String {
    id.replace('-', "").to_lowercase()
}

#[test]
fn the_reported_code_id_is_what_an_upload_would_be_keyed_on() {
    let exe = std::env::current_exe().expect("current exe");
    let exe_name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .expect("exe file name")
        .to_string();

    // --- our side: the mapped image, as the handler sees it ----------------
    let modules = bugsee_native::snapshot_modules_for_test();
    assert!(
        !modules.is_empty(),
        "no modules enumerated at all — every upload would resolve nothing"
    );

    // Match on the stem so a platform-specific suffix (`.exe`) does not matter.
    let stem = Path::new(&exe_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .expect("exe stem");
    let ours = modules
        .iter()
        .find(|(_, _, _, name)| Path::new(name).file_stem().and_then(|s| s.to_str()) == Some(stem))
        .unwrap_or_else(|| {
            panic!(
                "the running executable ({stem}) is absent from its own module map; \
                 names seen: {:?}",
                modules.iter().map(|m| &m.3).collect::<Vec<_>>()
            )
        });
    let reported = canonical(&ours.2);
    assert!(
        !reported.is_empty(),
        "the main module carries no code_id, so its symbols can never be found"
    );

    // --- the authority: the same crate the CLI and worker use --------------
    let bytes = std::fs::read(&exe).expect("read own executable");
    let object = symbolic_debuginfo::Object::parse(&bytes).expect("symbolic parses the executable");

    let debug_id = canonical(&object.debug_id().to_string());
    let code_id = object
        .code_id()
        .map(|c| canonical(c.as_str()))
        .unwrap_or_default();

    // Which of the two carries the identity is format-dependent — PE keys on
    // the CodeView debug id, Mach-O and ELF on the code id (LC_UUID / GNU
    // build-id) — so a match with either is the pipeline matching.
    assert!(
        reported == debug_id || reported == code_id,
        "reported code_id does not match what an upload would be keyed on.\n  \
         reported by the SDK : {reported}\n  \
         symbolic debug_id   : {debug_id}\n  \
         symbolic code_id    : {code_id}\n  \
         Symbolication would silently resolve nothing."
    );
}

//
//  origin.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! `source.origin` for code uploads names the caller. This lives in an
//! integration test because inside the crate its own frames are skipped.

use bugsee_core::reporting::manual_upload_meta;

#[test]
fn code_upload_origin_names_the_call_site() {
    // Test builds carry debug info, so symbols resolve here.
    let origin = manual_upload_meta()
        .source
        .origin
        .expect("origin populated");
    assert!(
        origin.contains("code_upload_origin_names_the_call_site"),
        "{origin}"
    );
    // `symbol:line`, not a bare address.
    assert!(!origin.starts_with("0x"), "{origin}");
    assert!(
        origin.rsplit(':').next().unwrap().parse::<u32>().is_ok(),
        "{origin}"
    );
}

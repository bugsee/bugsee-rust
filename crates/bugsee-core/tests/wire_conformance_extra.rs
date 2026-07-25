//
//  wire_conformance_extra.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Golden serialization coverage the primary `wire_format` suite lacks:
//!   1. an APM `Transaction` with a nested `Span` (report-bundle-structure §4.12);
//!   2. the recovery `crash.json` variants (§4.14) — native (thin, with a
//!      `signal` object) and abnormal-exit (managed `AppExit`).
//!
//! The crash shapes are asserted against the RUST-visible recovery path
//! (`recovery::find_pending` + `recovery::build_report`) driven off a seeded
//! temp data dir, mirroring the harness style in `recovery_e2e.rs`.

use std::path::{Path, PathBuf};

use bugsee_core::model::perf::{Span, Status, Transaction};
use bugsee_core::recovery::{build_report, find_pending};
use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// §4.12 — APM transaction + nested span golden shape.
// ---------------------------------------------------------------------------

#[test]
fn transaction_with_nested_span_serializes_to_contract_shape() {
    let span = Span {
        span_id: "span-1".into(),
        parent_span_id: Some("root-span".into()),
        operation: "db.query".into(),
        description: Some("SELECT * FROM cart".into()),
        status: Status::Ok,
        start_timestamp_ms: 1_720_531_200_000,
        end_timestamp_ms: 1_720_531_200_005,
        duration_nanos: 5_000_000,
        start_offset_ns: None,
        finished: true,
        attributes: Map::new(),
    };
    let txn = Transaction {
        timestamp: 1_720_531_200_000,
        trace_id: Some("trace-abc123".into()),
        name: Some("GET /checkout".into()),
        operation: Some("http.server".into()),
        status: Status::Ok,
        start_timestamp_ms: 1_720_531_200_000,
        end_timestamp_ms: 1_720_531_200_005,
        duration_nanos: 5_000_000,
        is_snapshot: false,
        app_version: Some("1.2.3".into()),
        app_build: 100,
        spans: vec![span],
    };

    let v = serde_json::to_value(&txn).expect("serialize transaction");

    // Full golden shape — camelCase field names, SCREAMING_SNAKE status,
    // absent-when-None keys omitted (startOffsetNs, empty attributes).
    assert_eq!(
        v,
        json!({
            "timestamp": 1_720_531_200_000i64,
            "traceId": "trace-abc123",
            "name": "GET /checkout",
            "operation": "http.server",
            "status": "OK",
            "startTimestampMs": 1_720_531_200_000i64,
            "endTimestampMs": 1_720_531_200_005i64,
            "durationNanos": 5_000_000,
            "isSnapshot": false,
            "appVersion": "1.2.3",
            "appBuild": 100,
            "spans": [{
                "spanId": "span-1",
                "parentSpanId": "root-span",
                "operation": "db.query",
                "description": "SELECT * FROM cart",
                "status": "OK",
                "startTimestampMs": 1_720_531_200_000i64,
                "endTimestampMs": 1_720_531_200_005i64,
                "durationNanos": 5_000_000,
                "finished": true
            }]
        })
    );

    // Explicit spot-checks the review called out.
    assert!(v.get("traceId").is_some(), "camelCase traceId present");
    assert_eq!(
        v["startTimestampMs"],
        json!(1_720_531_200_000i64),
        "camelCase startTimestampMs"
    );
    assert_eq!(
        v["durationNanos"],
        json!(5_000_000),
        "camelCase durationNanos"
    );
    assert_eq!(v["status"], json!("OK"), "SCREAMING_SNAKE status");
    let span0 = &v["spans"][0];
    assert_eq!(
        span0["parentSpanId"],
        json!("root-span"),
        "camelCase parentSpanId"
    );
    assert_eq!(span0["finished"], json!(true), "finished present");
    assert!(
        span0.get("startOffsetNs").is_none(),
        "startOffsetNs omitted when None"
    );
    assert!(
        span0.get("attributes").is_none(),
        "empty attributes omitted"
    );
}

#[test]
fn span_error_status_uses_screaming_snake_case() {
    let span = Span {
        span_id: "s".into(),
        parent_span_id: None,
        operation: "op".into(),
        description: None,
        status: Status::DeadlineExceeded,
        start_timestamp_ms: 1,
        end_timestamp_ms: 2,
        duration_nanos: 1_000_000,
        start_offset_ns: Some(42),
        finished: false,
        attributes: Map::new(),
    };
    let v = serde_json::to_value(&span).expect("serialize span");
    assert_eq!(v["status"], json!("DEADLINE_EXCEEDED"));
    assert!(
        v.get("parentSpanId").is_none(),
        "parentSpanId omitted when None"
    );
    assert_eq!(
        v["startOffsetNs"],
        json!(42),
        "startOffsetNs present when Some"
    );
    assert_eq!(v["finished"], json!(false));
}

// ---------------------------------------------------------------------------
// §4.14 — recovery crash.json variants, via the RUST-visible recovery path.
// ---------------------------------------------------------------------------

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new() -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bugsee-wire-extra-{}",
            bugsee_core::util::random_hex(8)
        ));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Write the liveness marker for an abnormally-ended generation.
fn seed_alive_marker(data: &Path, generation: u64) {
    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "1").unwrap();
}

#[test]
fn native_crash_json_matches_contract_4_14() {
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);

    // Native fault: a crash-info marker + a (fake) minidump under the part dir.
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=11\ncode=1\naddress=0xdeadbeef\n",
    )
    .unwrap();
    std::fs::write(gen_dir.join("crash.minidump"), b"MDMP\x00fake-bytes").unwrap();

    let pending = find_pending(&dir.path, 2);
    assert_eq!(pending.len(), 1, "the crashed generation is pending");
    let report = build_report(&pending[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    // Thin native variant.
    assert_eq!(crash["ndkCrash"], json!(true));
    assert_eq!(crash["exception_type"], json!("native"));
    assert_eq!(crash["handled"], json!(false));
    assert_eq!(crash["obfuscated"], json!(false));
    assert_eq!(
        crash["signatures"],
        json!([]),
        "signatures computed server-side"
    );
    assert!(
        crash.get("exception").is_none(),
        "native variant carries no exception"
    );

    // Signal object per §4.14.
    assert_eq!(crash["signal"]["number"], json!(11));
    assert_eq!(crash["signal"]["name"], json!("SIGSEGV"));
    assert_eq!(crash["signal"]["code"], json!(1));
    assert_eq!(crash["signal"]["addr"], json!("0xdeadbeef"));
    assert!(crash["signal"]["code_name"].is_null());
    assert!(crash["signal"]["abort_message"].is_null());
    assert!(crash["signal"]["cause"].is_null());

    // The minidump is carried as an extra file.
    assert!(
        report.extra_files.iter().any(|f| f.file_type == "minidump"),
        "minidump bundled as an extra file"
    );
}

#[test]
fn abnormal_exit_crash_json_matches_contract_4_14() {
    let dir = TempDir::new();
    // Only the liveness marker survived — no native marker, minidump, or panic
    // snapshot — so this is an abnormal termination.
    seed_alive_marker(&dir.path, 1);

    let pending = find_pending(&dir.path, 2);
    assert_eq!(pending.len(), 1);
    let report = build_report(&pending[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    assert_eq!(crash["ndkCrash"], json!(false));
    assert_eq!(crash["handled"], json!(true));
    assert_eq!(crash["exception_type"], json!("exception"));
    assert_eq!(crash["exception"]["name"], json!("AppExit"));
    assert_eq!(crash["exception"]["domain"], json!("AppExit::Unknown"));
    assert_eq!(
        crash["exception"]["reason"],
        json!("Application terminated abnormally")
    );
    assert_eq!(crash["exception"]["frames"], json!([]));
    assert!(
        crash.get("signal").is_none(),
        "managed variant emits no top-level signal object"
    );
    assert_eq!(
        crash["signatures"]
            .as_array()
            .expect("signatures array")
            .len(),
        1,
        "abnormal-exit carries a single dedup signature"
    );
    assert!(
        report.extra_files.is_empty(),
        "no native artifacts for an abnormal exit"
    );
}

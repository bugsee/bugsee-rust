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

/// Write the liveness marker for an abnormally-ended generation. The marker
/// records the (now-dead) owning pid; `"0"` is a portable "owner gone" sentinel
/// (recovery's `process_is_alive` treats pid 0 as dead), so recovery does not
/// mistake the seeded generation for a still-running peer.
fn seed_alive_marker(data: &Path, generation: u64) {
    let sessions = data.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join(format!("{generation}.alive")), "0").unwrap();
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
fn native_crash_signature_from_module_offsets() {
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();

    // Crashing thread frames (absolute PCs) + the install-time module map.
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=11\ncode=1\naddress=0x0\ntime=0\nframe=0x1100\nframe=0x2200\n",
    )
    .unwrap();
    // base<TAB>size<TAB>name — 0x1100 falls in MyApp [0x1000,0x2000) at offset
    // 0x100; 0x2200 falls in libsystem [0x2000,0x3000) at offset 0x200.
    std::fs::write(
        gen_dir.join("crash.modules"),
        "1000\t1000\tMyApp\n2000\t1000\tlibsystem.dylib\n",
    )
    .unwrap();

    let pending = find_pending(&dir.path, 2);
    let report = build_report(&pending[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    let sigs = crash["signatures"].as_array().expect("signatures array");
    assert_eq!(sigs.len(), 1, "one client-side native dedup signature");
    let sig = sigs[0].as_str().unwrap();
    assert_eq!(sig.len(), 40, "lowercase SHA-1 hex");
    assert!(sig
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

    // Deterministic: the same crash on the next launch yields the same signature.
    let report2 = build_report(&find_pending(&dir.path, 2)[0], 999);
    let crash2: Value = serde_json::from_slice(&report2.crash_json).unwrap();
    assert_eq!(crash2["signatures"][0].as_str().unwrap(), sig, "stable");

    // A different crash site (different frame offsets) yields a different sig.
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=11\ncode=1\naddress=0x0\ntime=0\nframe=0x1900\n",
    )
    .unwrap();
    let other: Value =
        serde_json::from_slice(&build_report(&find_pending(&dir.path, 2)[0], 1).crash_json)
            .unwrap();
    assert_ne!(
        other["signatures"][0].as_str().unwrap(),
        sig,
        "distinct site"
    );
}

/// Persist a panic snapshot the way the observer's hook does, with one hidden
/// SDK frame ahead of the application frame.
fn seed_panic_info(gen_dir: &Path, reason: &str) {
    use bugsee_core::model::crash::{Frame, FrameData};
    use bugsee_core::panic_info::{PanicInfo, PANIC_INFO_NAME};

    let frame = |trace: &str, hidden: bool| Frame {
        trace: trace.into(),
        hidden,
        data: FrameData {
            source: Some("src/main.rs".into()),
            member_class: None,
            member: None,
            line: 25,
        },
    };
    let info = PanicInfo {
        reason: reason.to_string(),
        file: Some("src/main.rs".into()),
        line: 25,
        column: 13,
        timestamp: 1_720_531_200_000,
        frames: vec![
            frame("bugsee_panic::capture_frames", true),
            frame("app::checkout::pay", false),
        ],
    };
    info.write_to(&gen_dir.join(PANIC_INFO_NAME)).unwrap();
}

#[test]
fn lone_panic_snapshot_is_recovered_as_a_fatal_panic() {
    // An uncaught UNWINDING panic (the Rust default): the hook persisted a
    // snapshot, nothing caught it (a contained panic's snapshot is deleted by
    // `report_caught`), and the liveness marker survived because Recorder::drop
    // saw `thread::panicking()`. That must surface as the panic itself, NOT as a
    // generic abnormal exit — previously this case went entirely unreported.
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();
    seed_panic_info(&gen_dir, "checkout exploded");

    let report = build_report(&find_pending(&dir.path, 2)[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    assert_eq!(
        crash["exception_type"],
        json!("exception"),
        "managed variant"
    );
    assert_eq!(crash["handled"], json!(false), "a fatal panic is unhandled");
    assert_eq!(crash["ndkCrash"], json!(false));
    assert_eq!(crash["exception"]["name"], json!("panic"));
    assert!(
        crash["exception"]["reason"]
            .as_str()
            .unwrap()
            .contains("checkout exploded"),
        "the panic message is preserved: {:?}",
        crash["exception"]["reason"]
    );
    assert_ne!(
        crash["exception"]["name"], "AppExit",
        "must not degrade to the generic abnormal-exit report"
    );
    assert_eq!(
        crash["signatures"].as_array().map(|s| s.len()),
        Some(1),
        "carries a client dedup signature"
    );
}

#[test]
fn no_panic_snapshot_still_reports_an_abnormal_exit() {
    // Regression guard for the other half of the branch: a session that died
    // with neither a native signal nor a panic snapshot (OOM kill, SIGKILL) is
    // still the generic abnormal-exit report.
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    std::fs::create_dir_all(dir.path.join("parts").join("1")).unwrap();

    let report = build_report(&find_pending(&dir.path, 2)[0], 1);
    let crash: Value = serde_json::from_slice(&report.crash_json).unwrap();
    assert_eq!(crash["exception"]["name"], json!("AppExit"));
}

#[test]
fn native_crash_json_carries_modules_with_code_id_and_relative_frames() {
    // The symbolication contract: the backend keys its symbol store on a module's
    // code id (Mach-O LC_UUID / GNU build-id) and resolves `reladdr` within it.
    // Without these fields a native crash can never be symbolicated, no matter
    // what symbols the user uploads.
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=11\ncode=1\naddress=0x0\ntime=0\nframe=0x1100\nframe=0x2200\n",
    )
    .unwrap();
    // base <TAB> size <TAB> code_id <TAB> name
    std::fs::write(
        gen_dir.join("crash.modules"),
        "1000\t1000\tabcdef0123456789abcdef0123456789\tMyApp\n\
         2000\t1000\t\tlibnoid.so\n",
    )
    .unwrap();

    let report = build_report(&find_pending(&dir.path, 2)[0], 1);
    let crash: Value = serde_json::from_slice(&report.crash_json).unwrap();

    let modules = crash["modules"].as_array().expect("modules array");
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0]["filename"], "MyApp");
    assert_eq!(modules[0]["base_addr"], "0x1000");
    assert_eq!(modules[0]["end_addr"], "0x2000");
    assert_eq!(
        modules[0]["code_id"], "abcdef0123456789abcdef0123456789",
        "the symbol-store lookup key must reach the wire"
    );
    // A module with no build-id is still reported (address context) but carries
    // no code_id — it simply cannot resolve.
    assert!(
        modules[1].get("code_id").is_none(),
        "an empty code id must be omitted, not sent as \"\": {:?}",
        modules[1]
    );

    let frames = crash["frames"].as_array().expect("frames array");
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0]["addr"], "0x1100");
    assert_eq!(frames[0]["module"], "MyApp");
    assert_eq!(frames[0]["reladdr"], 0x100, "module-relative offset");
    assert_eq!(frames[1]["module"], "libnoid.so");
    assert_eq!(frames[1]["reladdr"], 0x200);
}

#[test]
fn legacy_module_map_formats_still_recover() {
    // A marker written by an older SDK build and recovered after an upgrade must
    // still produce a report (no code_id, and for the 2-field form no bounds).
    for map in [
        "1000\t1000\tMyApp\n", // base, size, name
        "1000\tMyApp\n",       // base, name
    ] {
        let dir = TempDir::new();
        seed_alive_marker(&dir.path, 1);
        let gen_dir = dir.path.join("parts").join("1");
        std::fs::create_dir_all(&gen_dir).unwrap();
        std::fs::write(
            gen_dir.join("crash.info"),
            "signal=11\ncode=1\naddress=0x0\ntime=0\nframe=0x1100\n",
        )
        .unwrap();
        std::fs::write(gen_dir.join("crash.modules"), map).unwrap();

        let report = build_report(&find_pending(&dir.path, 2)[0], 1);
        let crash: Value = serde_json::from_slice(&report.crash_json).unwrap();
        let modules = crash["modules"].as_array().expect("modules array");
        assert_eq!(modules[0]["filename"], "MyApp", "map: {map:?}");
        assert!(modules[0].get("code_id").is_none(), "map: {map:?}");
        // Still signs, so the crash-loop blacklist keeps working.
        assert!(crash["signatures"]
            .as_array()
            .is_some_and(|s| !s.is_empty()));
    }
}

#[test]
fn native_signature_skips_pc_outside_every_module_range() {
    // A PC that falls outside every module's [base, base+size) range (e.g. a
    // frame from a module dlopen'd after the install-time snapshot) must be
    // SKIPPED, not misattributed to the nearest-below module with a bogus,
    // ASLR-unstable offset (F25).
    let module_map = "1000\t1000\tMyApp\n"; // MyApp spans [0x1000, 0x2000)

    let sig_for = |frames: &str| -> String {
        let dir = TempDir::new();
        seed_alive_marker(&dir.path, 1);
        let gen_dir = dir.path.join("parts").join("1");
        std::fs::create_dir_all(&gen_dir).unwrap();
        std::fs::write(
            gen_dir.join("crash.info"),
            format!("signal=11\ncode=1\naddress=0x0\ntime=0\n{frames}"),
        )
        .unwrap();
        std::fs::write(gen_dir.join("crash.modules"), module_map).unwrap();
        let report = build_report(&find_pending(&dir.path, 2)[0], 1);
        let crash: Value = serde_json::from_slice(&report.crash_json).unwrap();
        crash["signatures"][0].as_str().unwrap().to_string()
    };

    // 0x1500 is in range (offset 0x500); 0x9999 is above MyApp's end (0x2000) and
    // resolves to nothing, so it contributes nothing to the signature.
    let with_out_of_range = sig_for("frame=0x1500\nframe=0x9999\n");
    let in_range_only = sig_for("frame=0x1500\n");
    assert_eq!(
        with_out_of_range, in_range_only,
        "an out-of-module PC must be skipped, not fold into the signature"
    );
}

#[test]
fn native_crash_json_without_crash_info_still_emits_signal() {
    // A minidump-only session (no crash-info marker) must not produce an
    // under-specified native payload — the `signal` object is always present,
    // with unknown/nulled fields.
    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();
    std::fs::write(gen_dir.join("crash.minidump"), b"MDMP\x00only").unwrap();

    let pending = find_pending(&dir.path, 2);
    assert_eq!(pending.len(), 1);
    let report = build_report(&pending[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    assert_eq!(crash["exception_type"], json!("native"));
    assert!(
        crash.get("signal").is_some(),
        "native variant always carries a signal object"
    );
    assert_eq!(crash["signal"]["number"], json!(0));
    assert_eq!(crash["signal"]["name"], json!("UNKNOWN"));
    assert!(crash["signal"]["cause"].is_null());
}

#[test]
fn correlated_panic_crash_json_matches_contract_4_14() {
    use bugsee_core::model::crash::{Frame, FrameData};
    use bugsee_core::panic_info::PanicInfo;

    let dir = TempDir::new();
    seed_alive_marker(&dir.path, 1);
    let gen_dir = dir.path.join("parts").join("1");
    std::fs::create_dir_all(&gen_dir).unwrap();

    // A SIGABRT native marker (with a fresh `time=`) + a panic snapshot whose
    // timestamp is within the correlation window: the two fold into ONE managed
    // crash event carrying the panic frames (not a thin native variant).
    std::fs::write(
        gen_dir.join("crash.info"),
        "signal=6\ncode=0\naddress=0x0\ntime=1000\n",
    )
    .unwrap();
    let info = PanicInfo {
        reason: "index out of bounds".into(),
        file: Some("src/checkout.rs".into()),
        line: 42,
        column: 9,
        timestamp: 1000,
        frames: vec![Frame {
            trace: "app::checkout::settle".into(),
            hidden: false,
            data: FrameData {
                source: Some("src/checkout.rs".into()),
                member_class: Some("app::checkout".into()),
                member: Some("settle".into()),
                line: 42,
            },
        }],
    };
    info.write_to(&gen_dir.join("panic.info")).unwrap();

    let pending = find_pending(&dir.path, 2);
    assert_eq!(pending.len(), 1);
    let report = build_report(&pending[0], 1_720_531_200_000);
    let crash: Value = serde_json::from_slice(&report.crash_json).expect("parse crash.json");

    // Managed variant: exception object, no top-level `signal`, `domain` null
    // (the SCREAMING inventory reserves domain for AppHang::*/AppExit::*/null),
    // and no off-contract `mechanism` key.
    assert_eq!(crash["handled"], json!(false));
    assert_eq!(crash["ndkCrash"], json!(false));
    assert_eq!(crash["exception_type"], json!("exception"));
    assert!(crash.get("signal").is_none(), "no signal object on managed");
    assert!(
        crash.get("mechanism").is_none(),
        "no off-contract mechanism"
    );
    assert_eq!(crash["exception"]["name"], json!("panic"));
    assert!(
        crash["exception"]["domain"].is_null(),
        "domain null (Signal::* is off the contract inventory)"
    );
    let reason = crash["exception"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("index out of bounds") && reason.contains("checkout.rs:42:9"),
        "reason carries message + panic location: {reason}"
    );
    assert_eq!(
        crash["exception"]["frames"][0]["trace"],
        json!("app::checkout::settle")
    );
    assert_eq!(
        crash["signatures"].as_array().expect("signatures").len(),
        1,
        "one dedup signature"
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

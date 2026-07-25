//
//  persistence.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Capture-window persistence: rotation, hard-link snapshot, timestamp-filtered
//! export, and byte/time eviction.

use std::path::PathBuf;

use bugsee_core::capture::{export_report, PartStore, WindowCaps};
use serde_json::Value;

/// A throwaway temp dir under the system temp, cleaned up on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "bugsee-test-{tag}-{}",
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

fn log_payload(msg: &str) -> Vec<u8> {
    format!(r#"{{"timestamp":1,"level":3,"source":1,"message":"{msg}"}}"#).into_bytes()
}

#[test]
fn snapshot_export_filters_by_timestamp() {
    let dir = TempDir::new("snap");
    let mut store = PartStore::new(&dir.path, 0, WindowCaps::default()).unwrap();

    // Three log entries at ts 100/200/300, one network entry at 250.
    store.append("log", 100, &log_payload("a")).unwrap();
    store.rotate().unwrap();
    store.append("log", 200, &log_payload("b")).unwrap();
    store
        .append("network", 250, br#"{"timestamp":250,"id":"x"}"#)
        .unwrap();
    store.append("log", 300, &log_payload("c")).unwrap();

    // Snapshot window [150, 260] should include log@200, network@250; drop 100 & 300.
    let report_dir = dir.path.join("reports").join("r1");
    store.snapshot_into(&report_dir, 150, 260).unwrap();

    let docs = export_report(&report_dir, 150, 260).unwrap();
    let by: std::collections::HashMap<_, _> =
        docs.into_iter().map(|d| (d.channel, d.bytes)).collect();

    let log: Value = serde_json::from_slice(&by["log"]).unwrap();
    assert_eq!(log["version"], 2);
    let msgs: Vec<&str> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        msgs,
        vec!["b"],
        "only the in-window log entry survives the filter"
    );

    let net: Value = serde_json::from_slice(&by["network"]).unwrap();
    assert_eq!(net["events"].as_array().unwrap().len(), 1);
    assert_eq!(net["events"][0]["id"], "x");
}

#[test]
fn export_preserves_order_across_parts() {
    let dir = TempDir::new("order");
    let mut store = PartStore::new(&dir.path, 0, WindowCaps::default()).unwrap();
    for (i, ts) in [10i64, 20, 30, 40].into_iter().enumerate() {
        store
            .append("log", ts, &log_payload(&format!("m{i}")))
            .unwrap();
        store.rotate().unwrap();
    }
    let report_dir = dir.path.join("reports").join("r");
    store.snapshot_into(&report_dir, 0, 1000).unwrap();
    let docs = export_report(&report_dir, 0, 1000).unwrap();
    let log: Value = serde_json::from_slice(&docs[0].bytes).unwrap();
    let msgs: Vec<&str> = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        msgs,
        vec!["m0", "m1", "m2", "m3"],
        "records stay timestamp-ordered"
    );
}

#[test]
fn byte_cap_evicts_oldest_parts() {
    let dir = TempDir::new("evict");
    let caps = WindowCaps {
        max_window_ms: i64::MAX, // isolate the byte cap
        max_bytes: 200,
        max_events: u64::MAX,
    };
    let mut store = PartStore::new(&dir.path, 0, caps).unwrap();
    // Each ~50-byte payload in its own part; after enough rotations the oldest
    // parts must be unlinked to stay under 200 bytes.
    let big = "x".repeat(50);
    for ts in 0..10 {
        store.append("log", ts, big.as_bytes()).unwrap();
        store.rotate().unwrap();
    }
    let parts_root = dir.path.join("parts").join("0");
    let remaining = std::fs::read_dir(&parts_root).unwrap().count();
    assert!(
        remaining < 10,
        "eviction removed old parts, {remaining} remain"
    );
    assert!(remaining >= 1, "at least the current part remains");
}

#[test]
fn time_cap_evicts_parts_beyond_the_window() {
    // Regression: time-based eviction previously never fired (it read the
    // freshly-rotated empty part's end_ts).
    let dir = TempDir::new("timecap");
    let caps = WindowCaps {
        max_window_ms: 100,  // 100 ms window
        max_bytes: u64::MAX, // isolate the time cap
        max_events: u64::MAX,
    };
    let mut store = PartStore::new(&dir.path, 0, caps).unwrap();
    // One entry per part, timestamps 0,50,100,...,500 — span 500 ms >> 100 ms.
    for i in 0..=10 {
        store
            .append("log", i * 50, &log_payload(&format!("m{i}")))
            .unwrap();
        store.rotate().unwrap();
    }
    let (start, end) = store.retained_span().expect("some retained span");
    assert!(
        end - start <= 150,
        "retained span {} ms should be near the 100 ms cap, not 500 ms",
        end - start
    );
    let parts_root = dir.path.join("parts").join("0");
    let remaining = std::fs::read_dir(&parts_root).unwrap().count();
    assert!(
        remaining < 11,
        "old parts evicted by the time cap ({remaining} remain)"
    );
}

#[test]
fn hard_link_snapshot_survives_eviction() {
    let dir = TempDir::new("refcount");
    let caps = WindowCaps {
        max_window_ms: i64::MAX,
        max_bytes: 60,
        max_events: u64::MAX,
    };
    let mut store = PartStore::new(&dir.path, 0, caps).unwrap();
    store.append("log", 10, &log_payload("keep-me")).unwrap();

    // Snapshot the first part before it is evicted.
    let report_dir = dir.path.join("reports").join("r");
    store.snapshot_into(&report_dir, 0, 1000).unwrap();

    // Now push data that forces eviction of the original part from the live pool.
    for ts in 1..8 {
        store.rotate().unwrap();
        store
            .append("log", ts * 100, &log_payload("filler-payload"))
            .unwrap();
    }

    // The snapshot's hard link keeps the data alive despite live eviction.
    let docs = export_report(&report_dir, 0, 50).unwrap();
    let log: Value = serde_json::from_slice(&docs[0].bytes).unwrap();
    assert_eq!(log["events"][0]["message"], "keep-me");
}

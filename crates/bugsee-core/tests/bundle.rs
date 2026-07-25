//
//  bundle.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Report-bundle ZIP conformance: flat layout, per-entry compression policy
//! (STORE request.json, Zstd/method-93 for JSON), and round-trippable content.

use std::io::{Cursor, Read};

use bugsee_core::bundle::{
    bundle_filename, compression_for, write_bundle, BundleEntry, EntryCompression,
};
use bugsee_core::model::enums::{IssueType, Severity, TriggerType};
use bugsee_core::model::environment::Environment;
use bugsee_core::model::report::{FileDescriptor, IssueRequest, Manifest, Source, TimeWindow};
use serde_json::{Map, Value};
use zip::{CompressionMethod, ZipArchive};

fn sample_request() -> IssueRequest {
    IssueRequest {
        issue_type: IssueType::Crash,
        summary: Some("NullPointer in checkout".into()),
        description: None,
        labels: vec![],
        severity: Severity::High,
        email: Some("user@example.com".into()),
        signatures: vec!["9f8e7d6c5b4a39281706f5e4d3c2b1a0ffeeddcc".into()],
        source: Source {
            trigger: TriggerType::Crash,
            origin: None,
        },
        created_on: "2026-07-25T14:23:51.117Z".into(),
        environment: Environment::detect("0.1.0"),
    }
}

#[test]
fn compression_policy_matches_mobile() {
    assert_eq!(compression_for("request.json"), EntryCompression::Store);
    assert_eq!(compression_for("clip.mov"), EntryCompression::Store);
    assert_eq!(compression_for("shot.PNG"), EntryCompression::Store);
    assert_eq!(compression_for("manifest.json"), EntryCompression::Zstd);
    assert_eq!(compression_for("abc.log.json"), EntryCompression::Zstd);
    assert_eq!(compression_for("crash.json"), EntryCompression::Zstd);
}

#[test]
fn bundle_filename_shape() {
    let name = bundle_filename();
    assert!(name.ends_with(".bundle.zip"));
    assert_eq!(
        name.len(),
        "xxxxxxxxxxxxxxxxxxxx".len() + ".bundle.zip".len()
    );
}

#[test]
fn bundle_roundtrip_layout_and_compression() {
    let log_file = "9f3c1a2b4d5e6f70.log.json";
    let files = vec![
        FileDescriptor::capture(log_file, "log"),
        FileDescriptor::capture("crash.json", "crash"),
    ];
    let manifest = Manifest::new(
        TimeWindow {
            start: 1_720_512_345_678,
            end: 1_720_512_405_678,
        },
        files,
        Map::new(),
    );

    let entries = vec![
        BundleEntry::new(".apptoken", b"APP_TOKEN_RAW".to_vec()),
        BundleEntry::new(
            "request.json",
            serde_json::to_vec(&sample_request()).unwrap(),
        ),
        BundleEntry::new("manifest.json", serde_json::to_vec(&manifest).unwrap()),
        BundleEntry::new(
            log_file,
            br#"{"version":2,"events":[{"timestamp":1,"level":3,"source":1,"message":"hi"}]}"#
                .to_vec(),
        ),
        BundleEntry::new(
            "crash.json",
            br#"{"timestamp":1,"handled":false,"ndkCrash":false}"#.to_vec(),
        ),
    ];

    let mut buf = Vec::new();
    write_bundle(Cursor::new(&mut buf), &entries).expect("write bundle");

    let mut zip = ZipArchive::new(Cursor::new(buf)).expect("open bundle");
    assert_eq!(zip.len(), 5);

    // Flat archive — no directory separators in any name.
    for i in 0..zip.len() {
        let f = zip.by_index(i).unwrap();
        assert!(
            !f.name().contains('/'),
            "archive must be flat: {}",
            f.name()
        );
    }

    // request.json must be STORED; JSON capture files must be Zstd (method 93).
    assert_eq!(
        zip.by_name("request.json").unwrap().compression(),
        CompressionMethod::Stored
    );
    assert_eq!(
        zip.by_name("manifest.json").unwrap().compression(),
        CompressionMethod::Zstd
    );
    assert_eq!(
        zip.by_name(log_file).unwrap().compression(),
        CompressionMethod::Zstd
    );

    // .apptoken round-trips as raw bytes.
    let mut token = String::new();
    zip.by_name(".apptoken")
        .unwrap()
        .read_to_string(&mut token)
        .unwrap();
    assert_eq!(token, "APP_TOKEN_RAW");

    // manifest.json parses back to the contract shape.
    let mut mbytes = Vec::new();
    zip.by_name("manifest.json")
        .unwrap()
        .read_to_end(&mut mbytes)
        .unwrap();
    let mv: Value = serde_json::from_slice(&mbytes).unwrap();
    assert_eq!(mv["version"], 1);
    assert_eq!(mv["time"]["start"], 1_720_512_345_678i64);
    assert_eq!(mv["files"].as_array().unwrap().len(), 2);
    assert_eq!(mv["files"][0]["type"], "log");

    // request.json parses back with the crash metadata.
    let mut rbytes = Vec::new();
    zip.by_name("request.json")
        .unwrap()
        .read_to_end(&mut rbytes)
        .unwrap();
    let rv: Value = serde_json::from_slice(&rbytes).unwrap();
    assert_eq!(rv["type"], "crash");
    assert_eq!(rv["severity"], 3);
    assert_eq!(rv["source"]["type"], "crash");
    assert_eq!(
        rv["environment"]["platform"]["type"],
        serde_json::json!(std::env::consts::OS)
    );
}

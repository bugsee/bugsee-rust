//
//  scrub_ratelimit_e2e.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! End-to-end for the default PII scrubber and report rate limiting: seeded
//! secrets in every report section must not reach the transport, and an error
//! storm is bounded while crashes still get through.

use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::model::entry::{Breadcrumb, CaptureEntry, EventEntry, LogEntry, TraceEntry};
use bugsee_core::model::enums::{BreadcrumbLevel, IssueType, LogLevel, LogSource};
use bugsee_core::ratelimit::{Rate, RateLimitConfig};
use bugsee_core::reporting::manual_upload_meta;
use bugsee_core::runtime::DropReason;
use bugsee_core::scrub::ScrubConfig;
use bugsee_core::util::epoch_ms;
use bugsee_core::{MockTransport, Recorder, RecorderConfig, ReportMeta};
use serde_json::{json, Map, Value};
use zip::ZipArchive;

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("bugsee-scrub-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// Secret-shaped values are assembled from halves so no whole literal sits in the
// source (GitHub push protection rejects those).
const GH: &str = concat!("gh", "p_abcdefghijklmnopqrstuvwxyz0123456789");
const SK: &str = concat!("sk_", "live_4eC39HqLyjWDarjtT1zdp7dc");
const AWS: &str = concat!("AKI", "AIOSFODNN7EXAMPLE");
const JWT: &str = concat!(
    "ey",
    "JhbGciOiJIUzI1NiJ9.",
    "ey",
    "JzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk"
);

fn secrets() -> Vec<String> {
    [
        "alice.smith@corp-example.com",
        SK,
        GH,
        AWS,
        "4111 1111 1111 1111",
        "hunter2-super-pw",
        "/Users/jdoe/",
        "/home/jdoe/",
        JWT,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// One string packed with every kind of secret.
fn dirty() -> String {
    format!(
        "mail alice.smith@corp-example.com key {SK} gh {GH} aws {AWS} \
         card 4111 1111 1111 1111 password=hunter2-super-pw at /Users/jdoe/proj/a.rs \
         and /home/jdoe/b.rs jwt {JWT}"
    )
}

fn unzip_all(zip: &[u8]) -> Vec<(String, String)> {
    let mut z = ZipArchive::new(Cursor::new(zip.to_vec())).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).unwrap();
            (
                f.name().to_string(),
                String::from_utf8_lossy(&buf).into_owned(),
            )
        })
        .collect()
}

fn config(dir: &TempDir) -> RecorderConfig {
    RecorderConfig::new(&dir.0, "TOKEN")
}

fn capture_all_kinds(r: &Recorder) {
    let now = epoch_ms();
    r.capture(CaptureEntry::Log(LogEntry {
        timestamp: now,
        level: LogLevel::Info,
        source: LogSource::StdOut,
        tag: Some("t".into()),
        message: Some(dirty()),
        custom: Map::new(),
    }));
    let mut params = Map::new();
    params.insert("note".into(), Value::from(dirty()));
    params.insert("api_key".into(), Value::from("hunter2-super-pw"));
    r.capture(CaptureEntry::UserEvent(EventEntry {
        timestamp: now,
        name: Some("evt".into()),
        params,
        custom: Map::new(),
    }));
    let mut data = Map::new();
    data.insert("path".into(), Value::from(dirty()));
    r.capture(CaptureEntry::Breadcrumb(Breadcrumb {
        timestamp: now,
        crumb_type: Some("nav".into()),
        category: Some("c".into()),
        level: BreadcrumbLevel::Info,
        message: Some(dirty()),
        data: Some(data),
    }));
    r.capture(CaptureEntry::UserTrace(TraceEntry {
        timestamp: now,
        display_id: None,
        name: Some("t".into()),
        value: json!(dirty()),
        custom: Map::new(),
    }));
}

fn dirty_meta(kind: IssueType) -> ReportMeta {
    let mut m = manual_upload_meta();
    m.issue_type = kind;
    m.summary = Some(dirty());
    m.description = Some(dirty());
    m.labels = vec!["alice.smith@corp-example.com".into()];
    m.signatures = vec!["a".repeat(40)];
    m.attrs.insert("note".into(), Value::from(dirty()));
    m.attrs
        .insert("password".into(), Value::from("hunter2-super-pw"));
    m
}

#[test]
fn seeded_secrets_in_every_report_section_do_not_survive_by_default() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let r = Recorder::launch(config(&dir), transport.clone()).unwrap();
    r.with_scope(|s| s.set_attribute("scope_attr", Value::from(dirty())));
    capture_all_kinds(&r);
    let crash = json!({"message": dirty(), "frames": [{"file": "/Users/jdoe/src/main.rs"}]});
    r.report(
        dirty_meta(IssueType::Crash),
        Some(serde_json::to_vec(&crash).unwrap()),
    );
    r.report(dirty_meta(IssueType::Bug), None);
    assert!(r.flush(Duration::from_secs(10)));

    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 2);
    for bundle in bundles.iter() {
        let files = unzip_all(bundle);
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        for want in [
            ".log.json",
            ".events.user.json",
            ".breadcrumbs.json",
            ".traces.user.json",
        ] {
            assert!(
                names.iter().any(|n| n.ends_with(want)),
                "{want} in {names:?}"
            );
        }
        for (name, body) in &files {
            if name == ".apptoken" {
                continue;
            }
            for secret in secrets() {
                assert!(
                    !body.contains(&secret),
                    "{secret:?} survived in {name}: {body}"
                );
            }
        }
        // Markers are present and the document still parses.
        let log = files.iter().find(|f| f.0.ends_with(".log.json")).unwrap();
        assert!(log.1.contains("[email]") && log.1.contains("[token]") && log.1.contains("[card]"));
        serde_json::from_str::<Value>(&log.1).unwrap();
    }
    assert!(
        unzip_all(&bundles[0]).iter().any(|f| f.0 == "crash.json"),
        "crash.json scrubbed, not dropped"
    );
}

#[test]
fn scrubbing_can_be_switched_off() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let mut cfg = config(&dir);
    cfg.scrub = ScrubConfig::disabled();
    let r = Recorder::launch(cfg, transport.clone()).unwrap();
    r.report(dirty_meta(IssueType::Bug), None);
    assert!(r.flush(Duration::from_secs(10)));
    let body = String::from_utf8_lossy(&transport.created_requests.lock().unwrap()[0]).into_owned();
    assert!(body.contains("alice.smith@corp-example.com"));
}

#[test]
fn reporter_email_identity_is_kept() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let r = Recorder::launch(config(&dir), transport.clone()).unwrap();
    let mut m = manual_upload_meta();
    m.email = Some("reporter@example.com".into());
    r.report(m, None);
    assert!(r.flush(Duration::from_secs(10)));
    let req: Value =
        serde_json::from_slice(&transport.created_requests.lock().unwrap()[0]).unwrap();
    assert_eq!(req["email"], "reporter@example.com");
}

#[test]
fn error_storm_is_bounded_crashes_survive_and_drops_are_summarised() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let dropped = Arc::new(AtomicUsize::new(0));
    let d2 = dropped.clone();
    let mut cfg = config(&dir);
    cfg.rate_limit = RateLimitConfig::default()
        .per_signature(Rate::new(3, Duration::from_millis(300)))
        .global(Rate::new(1000, Duration::from_secs(1)));
    cfg.on_report_dropped = Some(Box::new(move |reason| {
        if reason == DropReason::RateLimited {
            d2.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let r = Recorder::launch(cfg, transport.clone()).unwrap();

    let error = |sig: &str| {
        let mut m = manual_upload_meta();
        m.issue_type = IssueType::Error;
        m.signatures = vec![sig.into()];
        m
    };
    for _ in 0..200 {
        r.report(error("hot"), None);
    }
    // A different signature and a crash are unaffected by the hot loop.
    r.report(error("other"), None);
    let mut crash = manual_upload_meta();
    crash.issue_type = IssueType::Crash;
    crash.signatures = vec!["hot".into()];
    r.report(crash, Some(b"{}".to_vec()));
    assert!(r.flush(Duration::from_secs(20)));

    let created = transport.created_requests.lock().unwrap().len();
    // 3 hot errors + 1 other + 1 crash (+ at most a refill or two while the
    // storm was being processed).
    assert!((5..=7).contains(&created), "created {created}");
    let n_dropped = dropped.load(Ordering::SeqCst);
    assert!((190..=197).contains(&n_dropped), "dropped {n_dropped}");
    {
        let reqs = transport.created_requests.lock().unwrap();
        assert!(reqs
            .iter()
            .any(|b| { serde_json::from_slice::<Value>(b).unwrap()["type"] == "crash" }));
    }

    // After the bucket refills a hot error is admitted again.
    std::thread::sleep(Duration::from_millis(400));
    r.report(error("hot"), None);
    assert!(r.flush(Duration::from_secs(10)));

    // The suppressed count rode on the first report admitted after the storm
    // (and was then reset, so it is not repeated).
    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), created + 1);
    let counts: Vec<u64> = bundles
        .iter()
        .filter_map(|b| {
            let files = unzip_all(b);
            let m: Value =
                serde_json::from_str(&files.iter().find(|f| f.0 == "manifest.json")?.1).ok()?;
            m["attrs"]["bugsee.reports_suppressed"].as_u64()
        })
        .collect();
    assert_eq!(counts.len(), 1, "{counts:?}");
    assert!(counts[0] >= 190, "{counts:?}");
}

#[test]
fn rate_limiting_can_be_switched_off() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let mut cfg = config(&dir);
    cfg.rate_limit = RateLimitConfig::disabled();
    let r = Recorder::launch(cfg, transport.clone()).unwrap();
    for _ in 0..30 {
        let mut m = manual_upload_meta();
        m.issue_type = IssueType::Error;
        m.signatures = vec!["x".into()];
        r.report(m, None);
    }
    assert!(r.flush(Duration::from_secs(30)));
    assert_eq!(transport.created_requests.lock().unwrap().len(), 30);
}

#[test]
fn in_memory_mode_reports_are_scrubbed_too() {
    let dir = TempDir::new();
    let transport = Arc::new(MockTransport::default());
    let mut cfg = RecorderConfig::new(dir.0.join("must-stay-absent"), "T");
    cfg.memory_only = Some("test".to_string());
    let r = Recorder::launch(cfg, transport.clone()).unwrap();
    let crash = json!({"message": dirty(), "frames": [{"file": "/Users/jdoe/src/main.rs"}]});
    r.report(
        dirty_meta(IssueType::Crash),
        Some(serde_json::to_vec(&crash).unwrap()),
    );
    r.report(dirty_meta(IssueType::Error), None);
    assert!(r.flush(Duration::from_secs(10)));
    let bundles = transport.uploaded_bundles.lock().unwrap();
    assert_eq!(bundles.len(), 2);
    for b in bundles.iter() {
        for (name, body) in unzip_all(b) {
            if name == ".apptoken" {
                continue;
            }
            for secret in secrets() {
                assert!(!body.contains(&secret), "{secret:?} survived in {name}");
            }
        }
    }
    for req in transport.created_requests.lock().unwrap().iter() {
        let body = String::from_utf8_lossy(req);
        for secret in secrets() {
            assert!(!body.contains(&secret), "{secret:?} in request.json");
        }
    }
}

#[test]
fn structural_report_fields_survive_scrubbing_unchanged() {
    let dir = TempDir::new();
    let scrubbed = Arc::new(MockTransport::default());
    let plain = Arc::new(MockTransport::default());
    let crash = json!({
        "exception": {"name": "cancellation_token::CancellationToken", "message": "boom"},
        "signatures": ["0123456789abcdef0123456789abcdef01234567"],
        "threads": [{"frames": [{"trace": "my_app::auth::login::verify (src/auth.rs:42)"}]}],
        "modules": [{"filename": "/usr/lib/libsystem_kernel.dylib",
                      "code_id": "A1B2C3D4-1234-4567-89AB-0123456789AB",
                      "debug_id": "41111111-1111-1111-1111-111111111111"}]
    });
    for (transport, scrub) in [(&scrubbed, true), (&plain, false)] {
        let mut cfg = config(&dir);
        if !scrub {
            cfg.scrub = ScrubConfig::disabled();
        }
        let r = Recorder::launch(cfg, transport.clone()).unwrap();
        let mut m = manual_upload_meta();
        m.issue_type = IssueType::Crash;
        m.signatures = vec!["0123456789abcdef0123456789abcdef01234567".into()];
        m.summary = Some("my_app::auth::login::verify panicked".into());
        r.report(m, Some(serde_json::to_vec(&crash).unwrap()));
        assert!(r.flush(Duration::from_secs(10)));
        drop(r);
    }
    let get = |t: &MockTransport, name: &str| -> Value {
        let b = t.uploaded_bundles.lock().unwrap();
        let f = unzip_all(&b[0]);
        serde_json::from_str(&f.iter().find(|f| f.0 == name).unwrap().1).unwrap()
    };
    assert_eq!(get(&scrubbed, "crash.json"), get(&plain, "crash.json"));
    let strip = |mut v: Value| {
        for k in ["created_on", "environment"] {
            v.as_object_mut().unwrap().remove(k);
        }
        v
    };
    let req = |t: &MockTransport| -> Value {
        strip(serde_json::from_slice(&t.created_requests.lock().unwrap()[0]).unwrap())
    };
    assert_eq!(req(&scrubbed), req(&plain));
}

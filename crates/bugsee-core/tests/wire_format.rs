//
//  wire_format.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Golden conformance tests: every capture entry must serialize to the exact
//! JSON shape documented in the cross-platform report-bundle contract.

use bugsee_core::model::entry::{
    Breadcrumb, EventEntry, LogEntry, NetworkCustom, NetworkEntry, TraceEntry,
};
use bugsee_core::model::envelope::Envelope;
use bugsee_core::{BreadcrumbLevel, LogLevel, LogSource, NetworkStage, Severity};
use serde_json::{json, Map, Value};

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).expect("serialize")
}

#[test]
fn log_entry_with_tag() {
    let e = LogEntry {
        timestamp: 1720531200123,
        level: LogLevel::Info,
        source: LogSource::PlatformLog,
        tag: Some("MainScreen".into()),
        message: Some("screen shown".into()),
        custom: Map::new(),
    };
    assert_eq!(
        to_value(&e),
        json!({"timestamp":1720531200123i64,"level":3,"source":3,"tag":"MainScreen","message":"screen shown"})
    );
}

#[test]
fn log_entry_without_tag_omits_key() {
    let e = LogEntry {
        timestamp: 1720531200456,
        level: LogLevel::Error,
        source: LogSource::StdErr,
        tag: None,
        message: Some("NullPointerException in render()".into()),
        custom: Map::new(),
    };
    let v = to_value(&e);
    assert!(v.get("tag").is_none(), "tag must be omitted when empty");
    assert_eq!(
        v,
        json!({"timestamp":1720531200456i64,"level":1,"source":2,"message":"NullPointerException in render()"})
    );
}

#[test]
fn log_entry_flattens_custom_data() {
    let mut custom = Map::new();
    custom.insert("request_id".into(), json!("abc123"));
    let e = LogEntry {
        timestamp: 1,
        level: LogLevel::Debug,
        source: LogSource::Custom,
        tag: None,
        message: Some("x".into()),
        custom,
    };
    let v = to_value(&e);
    assert_eq!(v.get("request_id"), Some(&json!("abc123")), "custom flattens to top level");
}

#[test]
fn user_event_with_params() {
    let mut params = Map::new();
    params.insert("cart_total".into(), json!(42.5));
    params.insert("items".into(), json!(3));
    let e = EventEntry {
        timestamp: 1704067200500,
        name: Some("checkout_started".into()),
        params,
        custom: Map::new(),
    };
    assert_eq!(
        to_value(&e),
        json!({"timestamp":1704067200500i64,"name":"checkout_started","params":{"cart_total":42.5,"items":3}})
    );
}

#[test]
fn breadcrumb_nests_data_not_flattened() {
    let mut data = Map::new();
    data.insert("to".into(), json!("CheckoutScreen"));
    data.insert("from".into(), json!("MainScreen"));
    let e = Breadcrumb {
        timestamp: 1704067200000,
        crumb_type: Some("navigation".into()),
        category: Some("ui.activity".into()),
        level: BreadcrumbLevel::Info,
        message: None,
        data: Some(data),
    };
    let v = to_value(&e);
    assert_eq!(v.get("type"), Some(&json!("navigation")));
    assert_eq!(v.get("level"), Some(&json!("info")));
    assert!(v.get("message").is_none(), "message omitted when unset");
    assert_eq!(v["data"]["to"], json!("CheckoutScreen"), "custom nests under data");
}

#[test]
fn trace_entry_omits_display_id_headless() {
    let e = TraceEntry {
        timestamp: 1720000000000,
        display_id: None,
        name: Some("cart_total".into()),
        value: json!(129.95),
        custom: Map::new(),
    };
    let v = to_value(&e);
    assert!(v.get("displayId").is_none());
    assert_eq!(v, json!({"timestamp":1720000000000i64,"name":"cart_total","value":129.95}));
}

#[test]
fn network_entry_emits_all_keys_with_lowercase_stage() {
    let e = NetworkEntry {
        timestamp: 1720531200100,
        id: "a1b2c3".into(),
        mechanism: "reqwest".into(),
        url: "https://api.example.com/v1/users".into(),
        method: "POST".into(),
        stage: NetworkStage::Before,
        size: 42,
        redirect: false,
        status: 0,
        status_text: None,
        custom_error: None,
        event: None,
        custom: NetworkCustom {
            headers: Some(json!({"Content-Type":"application/json"})),
            body: Some("{\"name\":\"Jo\"}".into()),
            error: None,
            no_body_reason: None,
            timings: None,
        },
        is_override: false,
    };
    let v = to_value(&e);
    assert_eq!(v["type"], json!("before"), "stage is lowercase");
    // All contract keys present, null when N/A, timings absent when None.
    for k in ["statusText", "customError", "event"] {
        assert!(v.get(k).is_some(), "{k} must be present (null)");
        assert!(v[k].is_null(), "{k} should be null here");
    }
    assert!(v["custom"].get("timings").is_none(), "timings omitted when absent");
    assert_eq!(v["override"], json!(false));
}

#[test]
fn envelope_wraps_events_with_version_2() {
    let entries = vec![LogEntry {
        timestamp: 1,
        level: LogLevel::Info,
        source: LogSource::StdOut,
        tag: None,
        message: Some("hi".into()),
        custom: Map::new(),
    }];
    let env = Envelope::new(entries);
    let v = to_value(&env);
    assert_eq!(v["version"], json!(2));
    assert_eq!(v["events"][0]["message"], json!("hi"));
}

#[test]
fn severity_default_is_high_3() {
    assert_eq!(to_value(&Severity::default()), json!(3));
    assert_eq!(to_value(&Severity::Blocker), json!(5));
}

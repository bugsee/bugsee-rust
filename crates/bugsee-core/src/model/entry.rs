//
//  entry.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Capture entries — one typed struct per bundle capture-file. Every entry
//! begins with `timestamp` (epoch ms). An attached custom-data map is flattened
//! as additional top-level keys, except on breadcrumbs where it nests under
//! `data` (report-bundle-structure §3.3).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::enums::{BreadcrumbLevel, LogLevel, LogSource, NetworkStage};

/// A per-entry custom-data map.
pub type CustomData = Map<String, Value>;

fn is_empty_map(m: &CustomData) -> bool {
    m.is_empty()
}

/// Application/console log line (`log.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: i64,
    pub level: LogLevel,
    pub source: LogSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Always emitted (`null` when absent).
    pub message: Option<String>,
    #[serde(flatten, skip_serializing_if = "is_empty_map", default)]
    pub custom: CustomData,
}

/// Developer event from the public API (`events.user.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub timestamp: i64,
    /// Always emitted (`null` allowed); empty names are rejected before capture.
    pub name: Option<String>,
    #[serde(skip_serializing_if = "is_empty_map", default)]
    pub params: CustomData,
    #[serde(flatten, skip_serializing_if = "is_empty_map", default)]
    pub custom: CustomData,
}

/// OS/app/process lifecycle event observed by the SDK (`events.system.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemEvent {
    pub timestamp: i64,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "is_empty_map", default)]
    pub params: CustomData,
}

/// Navigation/action trail entry (`breadcrumbs.json`). Custom data nests under
/// `data` rather than being flattened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Breadcrumb {
    pub timestamp: i64,
    #[serde(rename = "type")]
    pub crumb_type: Option<String>,
    pub category: Option<String>,
    pub level: BreadcrumbLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<CustomData>,
}

/// Nested `custom` sub-object of a network entry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NetworkCustom {
    pub headers: Option<Value>,
    /// Body inline; `null` when dropped.
    pub body: Option<String>,
    pub error: Option<String>,
    #[serde(rename = "no_body_reason")]
    pub no_body_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<Value>,
}

/// HTTP/WebSocket capture entry (`network.json`). Every stage of one request is
/// a separate entry sharing the same `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub timestamp: i64,
    pub id: String,
    pub mechanism: String,
    pub url: String,
    pub method: String,
    #[serde(rename = "type")]
    pub stage: NetworkStage,
    pub size: i64,
    pub redirect: bool,
    pub status: i32,
    #[serde(rename = "statusText")]
    pub status_text: Option<String>,
    #[serde(rename = "customError")]
    pub custom_error: Option<Value>,
    /// WebSocket subtype; `null` for plain HTTP.
    pub event: Option<String>,
    pub custom: NetworkCustom,
    #[serde(rename = "override")]
    pub is_override: bool,
}

/// A named value observed over time (`traces.user.json` / `traces.system.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    pub timestamp: i64,
    #[serde(rename = "displayId", skip_serializing_if = "Option::is_none")]
    pub display_id: Option<i32>,
    pub name: Option<String>,
    pub value: Value,
    #[serde(flatten, skip_serializing_if = "is_empty_map", default)]
    pub custom: CustomData,
}

/// A captured entry tagged with the wire file it belongs to. Traces distinguish
/// system vs user because they share a schema but land in different files.
#[derive(Debug, Clone)]
pub enum CaptureEntry {
    Log(LogEntry),
    UserEvent(EventEntry),
    SystemEvent(SystemEvent),
    Breadcrumb(Breadcrumb),
    /// Boxed — `NetworkEntry` is much larger than the other variants.
    Network(Box<NetworkEntry>),
    SystemTrace(TraceEntry),
    UserTrace(TraceEntry),
    /// An APM transaction (exported to `performance.json`, not the event envelope).
    Performance(Box<super::perf::Transaction>),
}

impl CaptureEntry {
    /// The wire `type` string / file-name infix for this entry
    /// (report-bundle-structure §3.2).
    pub fn wire_type(&self) -> &'static str {
        match self {
            CaptureEntry::Log(_) => "log",
            CaptureEntry::UserEvent(_) => "events.user",
            CaptureEntry::SystemEvent(_) => "events.system",
            CaptureEntry::Breadcrumb(_) => "breadcrumbs",
            CaptureEntry::Network(_) => "network",
            CaptureEntry::SystemTrace(_) => "traces.system",
            CaptureEntry::UserTrace(_) => "traces.user",
            CaptureEntry::Performance(_) => "performance",
        }
    }

    /// The entry's capture timestamp (epoch ms).
    pub fn timestamp(&self) -> i64 {
        match self {
            CaptureEntry::Log(e) => e.timestamp,
            CaptureEntry::UserEvent(e) => e.timestamp,
            CaptureEntry::SystemEvent(e) => e.timestamp,
            CaptureEntry::Breadcrumb(e) => e.timestamp,
            CaptureEntry::Network(e) => e.timestamp,
            CaptureEntry::SystemTrace(e) => e.timestamp,
            CaptureEntry::UserTrace(e) => e.timestamp,
            CaptureEntry::Performance(e) => e.timestamp,
        }
    }

    /// Serialize just this entry's JSON object (the element that goes inside the
    /// envelope's `events` array).
    pub fn to_json(&self) -> Result<Value, serde_json::Error> {
        match self {
            CaptureEntry::Log(e) => serde_json::to_value(e),
            CaptureEntry::UserEvent(e) => serde_json::to_value(e),
            CaptureEntry::SystemEvent(e) => serde_json::to_value(e),
            CaptureEntry::Breadcrumb(e) => serde_json::to_value(e),
            CaptureEntry::Network(e) => serde_json::to_value(e),
            CaptureEntry::SystemTrace(e) => serde_json::to_value(e),
            CaptureEntry::UserTrace(e) => serde_json::to_value(e),
            CaptureEntry::Performance(e) => serde_json::to_value(e),
        }
    }
}

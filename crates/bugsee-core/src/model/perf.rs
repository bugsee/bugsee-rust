//
//  perf.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! APM transactions and spans (`performance.json`). Unlike other capture files,
//! the performance document uses a `{"transactions":[…]}` envelope. The span
//! list is flat; reconstruct the tree via `spanId`/`parentSpanId`
//! (report-bundle-structure §4.12).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Transaction / span status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    #[default]
    Ok,
    Error,
    Timeout,
    Cancelled,
    DeadlineExceeded,
    Unknown,
}

/// One span within a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: Status,
    pub start_timestamp_ms: i64,
    pub end_timestamp_ms: i64,
    pub duration_nanos: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_offset_ns: Option<i64>,
    pub finished: bool,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub attributes: Map<String, Value>,
}

/// A completed (or snapshotted) APM transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    pub timestamp: i64,
    pub trace_id: Option<String>,
    pub name: Option<String>,
    pub operation: Option<String>,
    pub status: Status,
    pub start_timestamp_ms: i64,
    pub end_timestamp_ms: i64,
    pub duration_nanos: i64,
    pub is_snapshot: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    pub app_build: i64,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub spans: Vec<Span>,
}

//
//  report.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `request.json` (issue metadata + environment) and `manifest.json` (file
//! inventory + capture window). `request.json` is byte-identical to the
//! issue-creation request body (report-bundle-structure §2.2 / §4.2).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::enums::{IssueType, Severity, TriggerType};
use super::environment::Environment;

/// The manifest format version (distinct from the capture envelope version `2`).
pub const MANIFEST_VERSION: u32 = 1;

/// What triggered a report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub trigger: TriggerType,
    /// `Class.method:line` digest for `code_upload`, else `null`.
    pub origin: Option<String>,
}

/// `request.json` — issue metadata + environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueRequest {
    #[serde(rename = "type")]
    pub issue_type: IssueType,
    /// Always emitted (`null` allowed).
    pub summary: Option<String>,
    /// Always emitted (`null` allowed).
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Crash dedup signatures — lowercase SHA-1 hex. Omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub signatures: Vec<String>,
    pub source: Source,
    /// ISO-8601 report creation time.
    pub created_on: String,
    pub environment: Environment,
}

/// A single entry in the manifest's `files[]` inventory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDescriptor {
    pub filename: String,
    #[serde(rename = "type")]
    pub file_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attrs: Option<Map<String, Value>>,
}

impl FileDescriptor {
    /// A plain capture file descriptor (no per-file attrs).
    pub fn capture(filename: impl Into<String>, file_type: impl Into<String>) -> Self {
        FileDescriptor {
            filename: filename.into(),
            file_type: file_type.into(),
            name: None,
            attrs: None,
        }
    }
}

/// The capture time window (`manifest.time`), epoch ms.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TimeWindow {
    pub start: i64,
    pub end: i64,
}

/// `manifest.json` — file inventory + capture window + report-level attrs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub time: TimeWindow,
    pub files: Vec<FileDescriptor>,
    pub attrs: Map<String, Value>,
}

impl Manifest {
    /// A manifest at the current format version.
    pub fn new(time: TimeWindow, files: Vec<FileDescriptor>, attrs: Map<String, Value>) -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            time,
            files,
            attrs,
        }
    }
}

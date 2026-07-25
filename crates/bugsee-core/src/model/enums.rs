//
//  enums.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Wire enums. Integer-valued enums serialize as their numeric discriminant
//! (via `serde_repr`); string enums serialize as the exact lowercase/snake_case
//! tokens the backend expects. Casing matters — see report-bundle-structure §5.

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

/// Issue severity. Serializes as an int 1–5; `High` (3) is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum Severity {
    VeryLow = 1,
    Medium = 2,
    #[default]
    High = 3,
    Critical = 4,
    Blocker = 5,
}

/// Log level. Serializes as an int (`log.json` `level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum LogLevel {
    Error = 1,
    Warning = 2,
    Info = 3,
    Debug = 4,
    Verbose = 5,
}

/// Where a log line was captured from (`log.json` `source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum LogSource {
    Unknown = 0,
    StdOut = 1,
    StdErr = 2,
    /// Platform log stream (logcat / OSLog / journald). Unused headless.
    PlatformLog = 3,
    Bugsee = 4,
    WebView = 5,
    Custom = 98,
    Internal = 99,
}

/// Issue type (`request.json` / `crash.json` discrimination). Lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueType {
    Bug,
    Crash,
    Error,
}

/// What triggered a report (`source.type`). snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerType {
    Unknown,
    Crash,
    Error,
    Assert,
    Shake,
    Broadcast,
    Screenshot,
    Notification,
    CodeDialog,
    CodeUpload,
}

/// Breadcrumb level. Lowercase; an unset level serializes as `info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BreadcrumbLevel {
    Debug,
    #[default]
    Info,
    Warning,
    Error,
    Fatal,
}

/// Network entry stage (`network.json` `type`). Lowercase since SDK 7.0.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkStage {
    Before,
    Complete,
    Redirect,
    Error,
    Abort,
    Timing,
    Websocket,
}

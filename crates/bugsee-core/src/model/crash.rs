//
//  crash.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `crash.json` — the exception payload. This is the managed-runtime variant
//! used for Rust panics and handled errors (`exception` + `frames`, optional
//! `cause` chain). The thin native variant (signal + minidump reference) is
//! added in Phase 3. See report-bundle-structure §4.14.

use serde::{Deserialize, Serialize};

/// Structured frame metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// Source line; `-1` unknown, `-2` native.
    pub line: i64,
}

/// One stack frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    /// `crate::module::function (file:line)` display form.
    pub trace: String,
    /// `true` marks leading SDK-internal frames.
    pub hidden: bool,
    pub data: FrameData,
}

/// A single exception in the (possibly nested) exception chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExceptionInfo {
    pub name: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub frames: Vec<Frame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<ExceptionInfo>>,
}

/// The `crash.json` document (managed variant).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrashReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub timestamp: i64,
    /// `false` = uncaught; `true` = logged/handled.
    pub handled: bool,
    /// Whether app symbols are obfuscated (affects server symbolication).
    pub obfuscated: bool,
    #[serde(rename = "ndkCrash")]
    pub ndk_crash: bool,
    /// `"exception"` | `"error"` | `"throwable"` for managed variants.
    pub exception_type: String,
    /// 0 or 1 lowercase SHA-1 hex dedup signature.
    pub signatures: Vec<String>,
    pub exception: ExceptionInfo,
}

impl CrashReport {
    /// Build a handled-error crash payload (`handled=true`, managed variant).
    pub fn handled_error(
        name: impl Into<String>,
        reason: impl Into<String>,
        frames: Vec<Frame>,
        signature: Option<String>,
        timestamp: i64,
    ) -> Self {
        CrashReport {
            uuid: None,
            timestamp,
            handled: true,
            obfuscated: false,
            ndk_crash: false,
            exception_type: "error".into(),
            signatures: signature.into_iter().collect(),
            exception: ExceptionInfo {
                name: name.into(),
                reason: reason.into(),
                domain: None,
                frames,
                cause: None,
            },
        }
    }

    /// Serialize to `crash.json` bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

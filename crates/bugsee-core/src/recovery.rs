//
//  recovery.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Next-launch recovery: find prior generations that ended abnormally (their
//! liveness marker survived) and build a crash report from their captured
//! window. A native minidump, when present, produces the thin native variant;
//! otherwise the exit is reported as an abnormal termination (DESIGN.md §5).

use std::path::{Path, PathBuf};

use serde_json::Map;

use crate::model::crash::{CrashReport, ExceptionInfo};
use crate::model::enums::{IssueType, Severity, TriggerType};
use crate::model::report::Source;
use crate::reporting::{ExtraFile, ReportMeta};
use crate::signature::panic_signature;

/// The native minidump file name written by the crash handler under a part dir.
pub const MINIDUMP_NAME: &str = "crash.minidump";
/// The async-signal-safe crash-info marker written by the native handler.
pub const CRASH_INFO_NAME: &str = "crash.info";

/// A prior generation awaiting recovery.
pub struct PendingSession {
    pub generation: u64,
    /// `<data>/parts/<gen>` — the crashed session's captured parts.
    pub parts_dir: PathBuf,
    /// Path to a native minidump, if the crash handler wrote one.
    pub minidump: Option<PathBuf>,
    /// Path to the native crash-info marker, if present.
    pub crash_info: Option<PathBuf>,
}

/// A report built from a pending session, ready to assemble + deliver.
pub struct RecoveredReport {
    pub meta: ReportMeta,
    pub crash_json: Vec<u8>,
    /// Native artifacts (e.g. minidump) to bundle alongside.
    pub extra_files: Vec<ExtraFile>,
}

/// Find prior generations (`< current_generation`) whose liveness marker
/// survived — i.e. that ended abnormally.
pub fn find_pending(data_dir: &Path, current_generation: u64) -> Vec<PendingSession> {
    let mut pending = Vec::new();
    let sessions = data_dir.join("sessions");
    let read = match std::fs::read_dir(&sessions) {
        Ok(r) => r,
        Err(_) => return pending,
    };
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(gen_str) = name.strip_suffix(".alive") else {
            continue;
        };
        let Ok(generation) = gen_str.parse::<u64>() else {
            continue;
        };
        if generation >= current_generation {
            continue;
        }
        let parts_dir = data_dir.join("parts").join(generation.to_string());
        let minidump = exists_opt(parts_dir.join(MINIDUMP_NAME));
        let crash_info = exists_opt(parts_dir.join(CRASH_INFO_NAME));
        pending.push(PendingSession {
            generation,
            parts_dir,
            minidump,
            crash_info,
        });
    }
    pending
}

fn exists_opt(p: PathBuf) -> Option<PathBuf> {
    p.exists().then_some(p)
}

/// Build the crash report for a pending session. A native crash (minidump or
/// crash-info marker) yields the thin native variant; anything else is an
/// abnormal termination.
pub fn build_report(pending: &PendingSession, timestamp: i64) -> RecoveredReport {
    if pending.minidump.is_some() || pending.crash_info.is_some() {
        build_native(pending, timestamp)
    } else {
        build_abnormal_exit(timestamp)
    }
}

/// Remove a recovered session's on-disk state (parts + marker).
pub fn discard(data_dir: &Path, pending: &PendingSession) {
    let _ = std::fs::remove_dir_all(&pending.parts_dir);
    let _ = std::fs::remove_file(
        data_dir
            .join("sessions")
            .join(format!("{}.alive", pending.generation)),
    );
}

fn build_abnormal_exit(timestamp: i64) -> RecoveredReport {
    let signature = panic_signature(
        "AppExit",
        "Application terminated abnormally",
        &[],
        true,
        Some("AppExit::Unknown"),
    );
    let crash = CrashReport {
        uuid: None,
        timestamp,
        handled: true,
        obfuscated: false,
        ndk_crash: false,
        exception_type: "exception".into(),
        signatures: vec![signature.clone()],
        exception: ExceptionInfo {
            name: "AppExit".into(),
            reason: "Application terminated abnormally".into(),
            domain: Some("AppExit::Unknown".into()),
            frames: Vec::new(),
            cause: None,
        },
    };
    RecoveredReport {
        meta: crash_meta(vec![signature], TriggerType::Crash),
        crash_json: crash.to_bytes().unwrap_or_default(),
        extra_files: Vec::new(),
    }
}

fn build_native(pending: &PendingSession, timestamp: i64) -> RecoveredReport {
    let signal = pending
        .crash_info
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| parse_crash_info(&s));

    // Thin native variant: frames/threads are reconstructed server-side from the
    // minidump; signatures are computed server-side too, so we send none.
    let mut crash = serde_json::json!({
        "uuid": serde_json::Value::Null,
        "timestamp": timestamp,
        "handled": false,
        "obfuscated": false,
        "ndkCrash": true,
        "exception_type": "native",
        "signatures": [],
    });
    if let Some(sig) = signal {
        crash["signal"] = serde_json::json!({
            "number": sig.number,
            "name": signal_name(sig.number),
            "code": sig.code,
            "addr": sig.addr,
        });
    }
    let crash_json = serde_json::to_vec(&crash).unwrap_or_default();

    let mut extra_files = Vec::new();
    if let Some(dump) = &pending.minidump {
        if let Ok(data) = std::fs::read(dump) {
            extra_files.push(ExtraFile {
                filename: format!("{}.minidump", crate::util::random_hex(10)),
                file_type: "minidump".into(),
                data,
            });
        }
    }

    RecoveredReport {
        meta: crash_meta(Vec::new(), TriggerType::Crash),
        crash_json,
        extra_files,
    }
}

/// Parsed native crash-info marker.
struct SignalInfo {
    number: i32,
    code: i32,
    addr: String,
}

/// Parse the async-signal-safe `key=value` crash-info marker.
fn parse_crash_info(text: &str) -> SignalInfo {
    let mut number = 0;
    let mut code = 0;
    let mut addr = "0x0".to_string();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "signal" => number = v.trim().parse().unwrap_or(0),
            "code" => code = v.trim().parse().unwrap_or(0),
            "address" | "addr" => addr = v.trim().to_string(),
            _ => {}
        }
    }
    SignalInfo { number, code, addr }
}

/// POSIX signal name for a number (common fatal signals).
fn signal_name(number: i32) -> &'static str {
    match number {
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        11 => "SIGSEGV",
        _ => "UNKNOWN",
    }
}

fn crash_meta(signatures: Vec<String>, trigger: TriggerType) -> ReportMeta {
    ReportMeta {
        issue_type: IssueType::Crash,
        summary: None,
        description: None,
        labels: Vec::new(),
        severity: Severity::default(),
        email: None,
        signatures,
        source: Source {
            trigger,
            origin: None,
        },
        attrs: Map::new(),
        attachments: Vec::new(),
    }
}

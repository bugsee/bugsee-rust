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
use crate::panic_info::{PanicInfo, PANIC_INFO_NAME};
use crate::reporting::{ExtraFile, ReportMeta};
use crate::signature::panic_signature;

/// The native minidump file name written by the crash handler under a part dir.
pub const MINIDUMP_NAME: &str = "crash.minidump";
/// The async-signal-safe crash-info marker written by the native handler.
pub const CRASH_INFO_NAME: &str = "crash.info";
/// POSIX `SIGABRT` — the signal an aborting Rust panic raises.
const SIGABRT: i32 = 6;

/// A prior generation awaiting recovery.
pub struct PendingSession {
    pub generation: u64,
    /// `<data>/parts/<gen>` — the crashed session's captured parts.
    pub parts_dir: PathBuf,
    /// Path to a native minidump, if the crash handler wrote one.
    pub minidump: Option<PathBuf>,
    /// Path to the native crash-info marker, if present.
    pub crash_info: Option<PathBuf>,
    /// Path to a persisted panic snapshot, if the observer wrote one.
    pub panic_info: Option<PathBuf>,
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
        let panic_info = exists_opt(parts_dir.join(PANIC_INFO_NAME));
        pending.push(PendingSession {
            generation,
            parts_dir,
            minidump,
            crash_info,
            panic_info,
        });
    }
    pending
}

fn exists_opt(p: PathBuf) -> Option<PathBuf> {
    p.exists().then_some(p)
}

/// Build the crash report for a pending session. Precedence: an aborting panic
/// (`SIGABRT` + a panic snapshot) correlates into one event; otherwise a native
/// crash yields the thin native variant, a lone panic snapshot yields a fatal
/// panic, and anything else is an abnormal termination.
pub fn build_report(pending: &PendingSession, timestamp: i64) -> RecoveredReport {
    let has_native = pending.minidump.is_some() || pending.crash_info.is_some();
    let signal = read_signal_info(pending);
    let panic = pending
        .panic_info
        .as_ref()
        .and_then(|p| PanicInfo::read_from(p));

    match (has_native, panic) {
        (true, Some(info)) if signal.as_ref().map(|s| s.number) == Some(SIGABRT) => {
            build_correlated(pending, info, signal, timestamp)
        }
        (true, _) => build_native(pending, timestamp),
        (false, Some(info)) => build_fatal_panic(info, timestamp),
        (false, None) => build_abnormal_exit(timestamp),
    }
}

fn read_signal_info(pending: &PendingSession) -> Option<SignalInfo> {
    let path = pending.crash_info.as_ref()?;
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_crash_info(&text))
}

/// A Rust panic that aborted into `SIGABRT` — one managed crash event carrying
/// the panic frames/message, with the signal recorded via the exception
/// `domain` (the contract reserves the `signal` object for the native variant,
/// so a managed variant must not emit it).
fn build_correlated(
    pending: &PendingSession,
    info: PanicInfo,
    signal: Option<SignalInfo>,
    timestamp: i64,
) -> RecoveredReport {
    let frame_sigs: Vec<String> = info
        .frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    let domain = signal.map(|s| format!("Signal::{}", signal_name(s.number)));
    let signature = panic_signature("panic", &info.reason, &frame_sigs, false, domain.as_deref());
    let crash = CrashReport {
        uuid: None,
        timestamp,
        handled: false,
        obfuscated: false,
        ndk_crash: false,
        exception_type: "exception".into(),
        signatures: vec![signature.clone()],
        exception: ExceptionInfo {
            name: "panic".into(),
            reason: reason_with_location(&info),
            domain,
            frames: info.frames,
            cause: None,
        },
    };
    RecoveredReport {
        meta: crash_meta(vec![signature], TriggerType::Crash),
        crash_json: crash.to_bytes().unwrap_or_default(),
        extra_files: read_minidump(pending),
    }
}

/// A Rust panic that ended the process without a native marker.
fn build_fatal_panic(info: PanicInfo, timestamp: i64) -> RecoveredReport {
    let frame_sigs: Vec<String> = info
        .frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    let signature = panic_signature("panic", &info.reason, &frame_sigs, false, None);
    let crash = CrashReport {
        uuid: None,
        timestamp,
        handled: false,
        obfuscated: false,
        ndk_crash: false,
        exception_type: "exception".into(),
        signatures: vec![signature.clone()],
        exception: ExceptionInfo {
            name: "panic".into(),
            reason: reason_with_location(&info),
            domain: None,
            frames: info.frames,
            cause: None,
        },
    };
    RecoveredReport {
        meta: crash_meta(vec![signature], TriggerType::Crash),
        crash_json: crash.to_bytes().unwrap_or_default(),
        extra_files: Vec::new(),
    }
}

fn reason_with_location(info: &PanicInfo) -> String {
    match &info.file {
        Some(file) => format!("{} ({}:{}:{})", info.reason, file, info.line, info.column),
        None => info.reason.clone(),
    }
}

fn read_minidump(pending: &PendingSession) -> Vec<ExtraFile> {
    let mut extra = Vec::new();
    if let Some(dump) = &pending.minidump {
        if let Ok(data) = std::fs::read(dump) {
            extra.push(ExtraFile {
                filename: format!("{}.minidump", crate::util::random_hex(10)),
                file_type: "minidump".into(),
                data,
            });
        }
    }
    extra
}

/// Remove a recovered session's on-disk state. The liveness marker is removed
/// **first**: a crash between the two steps then leaks a parts directory (later
/// GC'able) rather than leaving a marker that would re-recover a phantom empty
/// report or duplicate the already-delivered one.
pub fn discard(data_dir: &Path, pending: &PendingSession) {
    let _ = std::fs::remove_file(
        data_dir
            .join("sessions")
            .join(format!("{}.alive", pending.generation)),
    );
    let _ = std::fs::remove_dir_all(&pending.parts_dir);
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
    if let Some(sig) = read_signal_info(pending) {
        crash["signal"] = serde_json::json!({
            "number": sig.number,
            "name": signal_name(sig.number),
            "code": sig.code,
            "addr": sig.addr,
            "code_name": serde_json::Value::Null,
            "abort_message": serde_json::Value::Null,
            "cause": serde_json::Value::Null,
        });
    }
    let crash_json = serde_json::to_vec(&crash).unwrap_or_default();

    RecoveredReport {
        meta: crash_meta(Vec::new(), TriggerType::Crash),
        crash_json,
        extra_files: read_minidump(pending),
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

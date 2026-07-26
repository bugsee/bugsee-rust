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
/// The install-time module map (`base→name`) written by the native handler.
/// MUST match `bugsee_native`'s `MODULES_NAME`.
pub const MODULES_NAME: &str = "crash.modules";
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
///
/// A surviving marker whose owning process is **still alive** is skipped: with a
/// shared data dir, a concurrently-running peer (which claimed a lower generation
/// because it launched earlier) has a live marker that is *not* a crash. Its
/// marker is left in place for a future launch to recover once the peer is gone.
pub fn find_pending(data_dir: &Path, current_generation: u64) -> Vec<PendingSession> {
    let mut pending = Vec::new();
    let sessions = data_dir.join("sessions");
    let read = match std::fs::read_dir(&sessions) {
        Ok(r) => r,
        Err(_) => return pending,
    };
    let self_pid = std::process::id();
    for entry in read.flatten() {
        let marker = entry.path();
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
        // Decide from the marker's recorded owner pid whether this is a real
        // crash or a still-running shared-dir peer.
        let owner = std::fs::read_to_string(&marker).unwrap_or_default();
        let owner = owner.trim();
        if owner.is_empty() {
            // Present but not yet populated: `Session::begin` creates the marker
            // (O_EXCL) and writes the pid a moment later, so a peer launching
            // concurrently can observe this empty window. Skip only while the
            // marker is FRESH — a peer writes its pid within microseconds. An empty
            // marker older than the grace window is a session that died in the
            // create→write gap (or whose pid write silently failed on a full
            // disk); recover it so its crash is surfaced and its generation
            // reclaimed instead of being re-scanned and leaked on every future
            // launch (F28). No pid ⇒ no liveness check; fall through to recover.
            if marker_is_fresh(&marker) {
                continue;
            }
        } else if let Ok(pid) = owner.parse::<u32>() {
            // Skip a marker still owned by a live *other* process. Our own pid is
            // never skipped (a live process cannot share our pid, so a marker
            // bearing it is a dead prior incarnation — recover it).
            if pid != self_pid && process_is_alive(pid) {
                continue;
            }
        }
        // Non-empty but unparseable content ⇒ the owner is gone; recover.
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

/// Grace window for an empty (pid-not-yet-written) liveness marker: a launching
/// peer writes its pid within microseconds of the O_EXCL create, so a marker
/// still empty after this is a dead session, not a live peer mid-launch.
const EMPTY_MARKER_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether `marker` was last modified within [`EMPTY_MARKER_GRACE`] of now (so it
/// could still be a peer that just created it). An unreadable/absent or
/// future-dated mtime is treated as fresh — the conservative choice never
/// disturbs a possibly-live peer.
fn marker_is_fresh(marker: &Path) -> bool {
    std::fs::metadata(marker)
        .and_then(|m| m.modified())
        .map(|modified| {
            modified
                .elapsed()
                .map(|age| age < EMPTY_MARKER_GRACE)
                .unwrap_or(true)
        })
        .unwrap_or(true)
}

/// Whether a process with `pid` currently exists.
///
/// Uses `kill(pid, 0)`, which sends no signal and only probes existence: `0`
/// (permitted) or `EPERM` (exists, not permitted) both mean *alive*; `ESRCH`
/// means gone. A rare pid-reuse false-positive can defer recovering a real crash
/// to a later launch — acceptable vs. corrupting a running peer's live session.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only an error/permission check and
    // has no side effects; it is safe to call with any pid value.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    ret == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// No portable liveness probe off-unix: treat as dead so recovery still proceeds
/// (single-process is the common case; a live shared-dir peer is a unix concern).
#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    false
}

/// Build the crash report for a pending session. Precedence: a *fresh* aborting
/// panic (`SIGABRT` + a recent panic snapshot) correlates into one managed event;
/// any other native signal yields the thin native variant; and a session with no
/// native signal at all (including one carrying only a stale/contained panic
/// snapshot) is reported as an abnormal termination.
pub fn build_report(pending: &PendingSession, timestamp: i64) -> RecoveredReport {
    let has_native = pending.minidump.is_some() || pending.crash_info.is_some();
    let signal = read_signal_info(pending);
    let panic = pending
        .panic_info
        .as_ref()
        .and_then(|p| PanicInfo::read_from(p));

    match (has_native, panic) {
        // Correlate ONLY a fresh panic snapshot with a SIGABRT — a stale snapshot
        // left by a caught/foreign-contained panic must not be welded onto an
        // unrelated later crash.
        (true, Some(info))
            if signal.as_ref().map(|s| s.number) == Some(SIGABRT)
                && signal.as_ref().map(|s| is_fresh(s, &info)).unwrap_or(false) =>
        {
            build_correlated(pending, info, timestamp)
        }
        (true, _) => build_native(pending, timestamp),
        // A lone panic snapshot with NO native signal means the panic was
        // contained (the process did not die from it) — report the abnormal exit,
        // not a fabricated fatal panic.
        (false, _) => build_abnormal_exit(timestamp),
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
fn build_correlated(pending: &PendingSession, info: PanicInfo, timestamp: i64) -> RecoveredReport {
    let frame_sigs: Vec<String> = info
        .frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    // A plain managed crash: `domain` is null (matches the live `build_panic`
    // path and the contract; the SIGABRT is implied by the aborting panic).
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
        extra_files: read_minidump(pending),
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
/// **first**: a crash between the steps then leaks a parts directory (later
/// GC'able via [`gc_orphans`]) rather than leaving a marker that would re-recover
/// a phantom empty report or duplicate the already-delivered one.
pub fn discard(data_dir: &Path, pending: &PendingSession) {
    let sessions = data_dir.join("sessions");
    let _ = std::fs::remove_file(sessions.join(format!("{}.alive", pending.generation)));
    let _ = std::fs::remove_file(sessions.join(format!("{}.attempts", pending.generation)));
    let _ = std::fs::remove_dir_all(&pending.parts_dir);
}

/// The recovery-attempt-counter sidecar path for a generation.
fn attempts_path(data_dir: &Path, generation: u64) -> PathBuf {
    data_dir
        .join("sessions")
        .join(format!("{generation}.attempts"))
}

/// Increment and return a pending session's recovery-attempt count. Bumped
/// **before** each attempt so a pending that repeatedly panics the build/assemble
/// path (a corrupt-parts poison pill) still converges to the cap and is abandoned
/// instead of re-running — and aborting the whole recovery loop — every launch.
pub fn bump_attempts(data_dir: &Path, pending: &PendingSession) -> u32 {
    let path = attempts_path(data_dir, pending.generation);
    let next = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
        .saturating_add(1);
    let _ = std::fs::write(&path, next.to_string());
    next
}

/// Reap leftover on-disk state from generations that no longer have a liveness
/// marker — already recovered, or cleanly shut down: orphan `parts/<n>` dirs and
/// stray `.attempts` sidecars for `n < current_generation`. Bounds unbounded disk
/// growth from clean-exit part dirs and crash-interrupted discards (DESIGN.md §5).
///
/// Only marker-less generations are touched, so a live shared-dir peer's session
/// (marker present) and any pending awaiting recovery are never disturbed.
pub fn gc_orphans(data_dir: &Path, current_generation: u64) {
    let sessions = data_dir.join("sessions");
    let alive = |generation: u64| sessions.join(format!("{generation}.alive")).exists();

    if let Ok(read) = std::fs::read_dir(data_dir.join("parts")) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Ok(generation) = name.parse::<u64>() {
                if generation < current_generation && !alive(generation) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
    }
    if let Ok(read) = std::fs::read_dir(&sessions) {
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(gen_str) = name.strip_suffix(".attempts") {
                if let Ok(generation) = gen_str.parse::<u64>() {
                    if generation < current_generation && !alive(generation) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
    }
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
    // minidump. When the crash handler captured the crashing thread's frames, we
    // ALSO compute a stable client-side dedup signature from module-relative
    // offsets (mirroring the iOS/Android native feed) — this is what lets the
    // local blacklist suppress a native crash-on-launch loop.
    let signal = read_signal_info(pending);
    let signature = native_frame_signature(pending, signal.as_ref());
    let signatures: Vec<String> = signature.clone().into_iter().collect();

    let mut crash = serde_json::json!({
        "uuid": serde_json::Value::Null,
        "timestamp": timestamp,
        "handled": false,
        "obfuscated": false,
        "ndkCrash": true,
        "exception_type": "native",
        "signatures": signatures,
    });
    // The native variant always carries a `signal` object (part of its thin
    // shape). If no crash-info marker was written (e.g. a minidump-only session),
    // emit it with unknown/nulled fields rather than omitting it entirely.
    let number = signal.as_ref().map(|s| s.number).unwrap_or(0);
    crash["signal"] = serde_json::json!({
        "number": number,
        "name": signal_name(number),
        "code": signal.as_ref().map(|s| s.code).unwrap_or(0),
        "addr": signal.as_ref().map(|s| s.addr.clone()).unwrap_or_else(|| "0x0".into()),
        "code_name": serde_json::Value::Null,
        "abort_message": serde_json::Value::Null,
        "cause": serde_json::Value::Null,
    });
    let crash_json = serde_json::to_vec(&crash).unwrap_or_default();

    RecoveredReport {
        meta: crash_meta(signature.into_iter().collect(), TriggerType::Crash),
        crash_json,
        extra_files: read_minidump(pending),
    }
}

/// Compute a native crash dedup signature from the crashing thread's frame PCs
/// resolved to `(module, offset)` via the install-time module map. Returns `None`
/// when no frames or module map are available (recovery then sends no client
/// signature, leaving dedup to the server, rather than a too-coarse one).
fn native_frame_signature(pending: &PendingSession, signal: Option<&SignalInfo>) -> Option<String> {
    let signal = signal?;
    if signal.frames.is_empty() {
        return None;
    }
    let modules = read_modules(&pending.parts_dir.join(MODULES_NAME));
    if modules.is_empty() {
        return None;
    }
    // Absolute PC → (module, pc - base); frames in modules loaded after the
    // install-time snapshot resolve to nothing and are skipped.
    let frames: Vec<(String, u64)> = signal
        .frames
        .iter()
        .filter_map(|&pc| resolve_module_offset(pc, &modules))
        .collect();
    if frames.is_empty() {
        return None;
    }
    Some(crate::signature::native_signature(
        signal_name(signal.number),
        &frames,
    ))
}

/// The module whose loaded range `[base, base + size)` contains `pc` (the one
/// with the largest such base), and the offset within it. A PC outside every
/// module's range yields `None` — it is SKIPPED rather than misattributed to the
/// nearest-below module with a bogus, ASLR-unstable offset that would defeat the
/// crash-loop blacklist (F25).
fn resolve_module_offset(pc: usize, modules: &[(usize, usize, String)]) -> Option<(String, u64)> {
    modules
        .iter()
        .filter(|(base, size, _)| *base <= pc && pc - *base < *size)
        .max_by_key(|(base, _, _)| *base)
        .map(|(base, _, name)| (name.clone(), (pc - base) as u64))
}

/// Read the module map (`<base_hex>\t<size_hex>\t<name>` per line) written at
/// install time. A legacy 2-field line (`<base_hex>\t<name>`, from a marker that
/// survived an SDK upgrade) is read with an unbounded size, preserving the old
/// nearest-module behavior for that entry only.
fn read_modules(path: &Path) -> Vec<(usize, usize, String)> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let base = usize::from_str_radix(parts.next()?.trim(), 16).ok()?;
            let second = parts.next()?;
            match parts.next() {
                // New format: base \t size \t name.
                Some(name) => {
                    let size = usize::from_str_radix(second.trim(), 16).unwrap_or(usize::MAX);
                    Some((base, size, name.to_string()))
                }
                // Legacy format: base \t name (size unknown → unbounded).
                None => Some((base, usize::MAX, second.to_string())),
            }
        })
        .collect()
}

/// Parsed native crash-info marker.
struct SignalInfo {
    number: i32,
    code: i32,
    addr: String,
    /// Crash time (epoch ms) written by the native handler; `0` if unknown.
    time: i64,
    /// Absolute PCs of the crashing thread's frames (resolved to module offsets
    /// at signature time); empty if the handler captured none.
    frames: Vec<usize>,
}

/// Parse the async-signal-safe `key=value` crash-info marker.
fn parse_crash_info(text: &str) -> SignalInfo {
    let mut number = 0;
    let mut code = 0;
    let mut addr = "0x0".to_string();
    let mut time = 0;
    let mut frames = Vec::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "signal" => number = v.trim().parse().unwrap_or(0),
            "code" => code = v.trim().parse().unwrap_or(0),
            "address" | "addr" => addr = v.trim().to_string(),
            "time" => time = v.trim().parse().unwrap_or(0),
            "frame" => {
                let hex = v.trim().trim_start_matches("0x");
                if let Ok(pc) = usize::from_str_radix(hex, 16) {
                    frames.push(pc);
                }
            }
            _ => {}
        }
    }
    SignalInfo {
        number,
        code,
        addr,
        time,
        frames,
    }
}

/// Max gap (ms) between a native crash and a panic snapshot for them to be
/// treated as the same event. Prevents a stale `panic.info` (from a caught /
/// foreign-contained panic) from being welded onto an unrelated later crash.
const CORRELATION_WINDOW_MS: i64 = 10_000;

/// Whether a panic snapshot is fresh enough (vs. the crash time) to correlate.
fn is_fresh(signal: &SignalInfo, info: &PanicInfo) -> bool {
    signal.time != 0
        && info.timestamp != 0
        && (signal.time - info.timestamp).abs() <= CORRELATION_WINDOW_MS
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

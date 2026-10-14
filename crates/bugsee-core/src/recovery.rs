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

use crate::model::crash::{CrashReport, ExceptionInfo, SOURCE_SDK};
use crate::model::enums::{IssueType, Severity, TriggerType};
use crate::model::environment::{source_arch, source_platform};
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
        if !session_is_crashed(data_dir, generation, &marker, self_pid) {
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

/// Whether the session `generation`, whose marker survives, has ended abnormally.
///
/// A session whose `<gen>.lock` exists is arbitrated by that OS lock: held means a
/// live process (in any pid namespace), free means it died. The marker's pid is
/// not consulted, since it is only meaningful inside the writer's own pid
/// namespace and can be reused. Otherwise (no lock file, or the filesystem cannot
/// say) the marker is judged by [`legacy_marker_is_dead`].
fn session_is_crashed(data_dir: &Path, generation: u64, marker: &Path, self_pid: u32) -> bool {
    let lock = crate::session::Session::lock_path(data_dir, generation);
    if lock.exists() {
        match crate::claim::probe(&lock) {
            crate::claim::Probe::Held => return false,
            // A launching session locks BEFORE it writes its pid, and a marker
            // still empty and fresh may be one that is between creating the lock
            // file and locking it.
            crate::claim::Probe::Free => {
                return !(marker_is_empty(marker) && marker_is_fresh(marker));
            }
            crate::claim::Probe::Unknown => {}
        }
    }
    legacy_marker_is_dead(marker, self_pid)
}

/// How recently a lock-less session's heartbeat file must have been touched for
/// the session to count as alive. Generous because network filesystems cache file
/// attributes for up to about a minute.
const HEARTBEAT_STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// The heartbeat file beside a marker. Only a session that could not take a lock
/// (the filesystem rejects them) keeps one: it is touched every couple of seconds.
pub(crate) fn heartbeat_path(marker: &Path) -> PathBuf {
    marker.with_extension("beat")
}

/// Whether a marker with no usable lock beside it (written by an older SDK build,
/// or by a session on a filesystem without locks) is a crashed session rather
/// than a live peer.
///
/// A fresh heartbeat file means alive, whatever pid the marker names: that is the
/// only signal that works across pid namespaces when there are no locks. Without
/// one, the recorded pid is probed, which is only meaningful within one pid
/// namespace. A crash is therefore recovered only once the heartbeat has gone
/// quiet for [`HEARTBEAT_STALE`] (a restart faster than that defers recovery to a
/// later launch).
fn legacy_marker_is_dead(marker: &Path, self_pid: u32) -> bool {
    if std::fs::metadata(heartbeat_path(marker))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < HEARTBEAT_STALE)
    {
        return false;
    }
    let owner = std::fs::read_to_string(marker).unwrap_or_default();
    let owner = owner.trim();
    if owner.is_empty() {
        // Present but not yet populated: `Session::begin` creates the marker
        // (O_EXCL) and writes the pid a moment later, so a peer launching
        // concurrently can observe this empty window. Skip only while the
        // marker is FRESH. An empty marker older than the grace window is a
        // session that died in the create-to-write gap (or whose pid write failed
        // on a full disk); recover it so its crash is surfaced and its
        // generation reclaimed instead of being re-scanned on every launch.
        return !marker_is_fresh(marker);
    }
    match owner.parse::<u32>() {
        // Our own pid is never skipped (a live process cannot share our pid, so a
        // marker bearing it is a dead prior incarnation).
        Ok(pid) => pid == self_pid || !crate::process::is_alive(pid),
        // Non-empty but unparseable content: the owner is gone.
        Err(_) => true,
    }
}

fn marker_is_empty(marker: &Path) -> bool {
    std::fs::read_to_string(marker).is_ok_and(|s| s.trim().is_empty())
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

/// Build the crash report for a pending session. Precedence: a *fresh* aborting
/// panic (`SIGABRT` + a recent panic snapshot) correlates into one managed event;
/// any other native signal yields the thin native variant; a lone panic snapshot
/// is a fatal **unwinding** panic; and a session with neither is an abnormal
/// termination.
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
        // A lone panic snapshot with NO native signal is an uncaught UNWINDING
        // panic — the Rust default, and the case that previously went entirely
        // unreported. Two invariants make this unambiguous:
        //   1. `report_caught` DELETES the snapshot for a contained panic, so a
        //      surviving snapshot means nothing caught it;
        //   2. the session's liveness marker is only kept across a clean
        //      shutdown when `Recorder::drop` sees `thread::panicking()`, i.e.
        //      the process was unwinding out of `main` because of this panic.
        // So we report the panic itself rather than a generic abnormal exit.
        (false, Some(info)) => build_fatal_panic(info, timestamp),
        (false, None) => build_abnormal_exit(timestamp),
    }
}

/// A fatal uncaught **unwinding** panic, recovered on the next launch. Same
/// managed shape as the aborting-panic path ([`build_correlated`]) — the two
/// differ only in how the process died, which the contract does not encode for
/// a managed variant.
fn build_fatal_panic(info: PanicInfo, timestamp: i64) -> RecoveredReport {
    let frame_sigs: Vec<String> = info
        .frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    let signature = panic_signature("panic", &info.reason, &frame_sigs, false, None);
    let crash = CrashReport {
        source_sdk: SOURCE_SDK.to_string(),
        source_platform: source_platform().to_string(),
        source_arch: source_arch().to_string(),
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
        source_sdk: SOURCE_SDK.to_string(),
        source_platform: source_platform().to_string(),
        source_arch: source_arch().to_string(),
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
    let _ = std::fs::remove_file(sessions.join(format!("{}.beat", pending.generation)));
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

/// Claim a pending session for recovery, so that of several processes sharing the
/// data directory exactly one recovers it. The claim lives beside the liveness
/// marker and is released when the returned guard drops.
///
/// Lenient (see [`crate::claim::try_claim_lenient`]): a full or read-only
/// directory cannot take a claim file, and that is exactly when a stranded crash
/// most needs recovering. `None` if a peer holds the claim or has already
/// recovered the session; `Some(None)` means proceed without a claim.
pub fn claim_pending(
    data_dir: &Path,
    pending: &PendingSession,
) -> Option<Option<crate::claim::Claim>> {
    let claim = crate::claim::try_claim_lenient(
        &data_dir
            .join("sessions")
            .join(format!("{}.recover.lock", pending.generation)),
    )?;
    // The listing that produced `pending` may be stale: a peer that held this
    // claim before us can have recovered and discarded the session in between
    // (its claim released, the marker gone). Recovering it now would deliver a
    // phantom second report for it. The marker is removed first by `discard`, so
    // its absence, re-checked while holding the claim, is conclusive.
    //
    // It also re-evaluates liveness under the claim: between the scan and now a
    // launching session may have finished taking its lock (the scan saw it free,
    // then the pid appeared), and a live session must not be torn down.
    let marker = crate::session::Session::marker_path(data_dir, pending.generation);
    (marker.exists()
        && session_is_crashed(data_dir, pending.generation, &marker, std::process::id()))
    .then_some(claim)
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
            if let Some(gen_str) = name
                .strip_suffix(".attempts")
                .or_else(|| name.strip_suffix(".beat"))
            {
                if let Ok(generation) = gen_str.parse::<u64>() {
                    if generation < current_generation && !alive(generation) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            // Lock files of a generation with no marker: the session lock of a
            // crashed process (its marker already recovered) and the recovery
            // claim. Removed only if free, so a live holder's is left alone.
            let lock_gen = name
                .strip_suffix(".recover.lock")
                .or_else(|| name.strip_suffix(".lock"))
                .and_then(|g| g.parse::<u64>().ok());
            if let Some(generation) = lock_gen {
                if generation < current_generation && !alive(generation) {
                    crate::claim::remove_if_free(&entry.path());
                }
            }
            // Leftovers of an earlier claim scheme, once they are plainly dead.
            let old_gen = name
                .strip_suffix(".claim.takeover")
                .or_else(|| name.strip_suffix(".claim"))
                .and_then(|g| g.parse::<u64>().ok());
            if let Some(generation) = old_gen {
                if generation < current_generation
                    && !alive(generation)
                    && crate::queue::untouched_for(
                        &entry.path(),
                        std::time::Duration::from_secs(60),
                    )
                {
                    let _ = std::fs::remove_file(entry.path());
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
        source_sdk: SOURCE_SDK.to_string(),
        source_platform: source_platform().to_string(),
        source_arch: source_arch().to_string(),
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
        // Same self-describing marker the managed variants carry — this document
        // is hand-built rather than serialized from `CrashReport`, so it must be
        // set explicitly here too.
        "source_sdk": SOURCE_SDK,
        "source_platform": source_platform(),
        "source_arch": source_arch(),
        "uuid": serde_json::Value::Null,
        "timestamp": timestamp,
        "handled": false,
        "obfuscated": false,
        "ndkCrash": true,
        "exception_type": "native",
        "signatures": signatures,
    });

    // The loaded-module map + the crashing thread's module-relative frames. This
    // is what lets the backend symbolicate: it looks each module up in the
    // symbol store by `code_id` (Mach-O LC_UUID / GNU build-id) and resolves
    // `reladdr` within it. Modules without a code id are still reported (they
    // give the crash address context) but can never resolve — an ELF needs
    // `-Wl,--build-id` to be symbolicatable.
    let modules = read_modules(&pending.parts_dir.join(MODULES_NAME));
    if !modules.is_empty() {
        crash["modules"] = serde_json::Value::Array(
            modules
                .iter()
                .map(|m| {
                    let mut entry = serde_json::json!({
                        "base_addr": format!("0x{:x}", m.base),
                        "filename": m.name,
                    });
                    if m.size != usize::MAX {
                        entry["end_addr"] = serde_json::Value::from(format!(
                            "0x{:x}",
                            m.base.saturating_add(m.size)
                        ));
                    }
                    if !m.code_id.is_empty() {
                        entry["code_id"] = serde_json::Value::from(m.code_id.clone());
                    }
                    entry
                })
                .collect(),
        );
    }
    if let Some(info) = signal.as_ref() {
        if !info.frames.is_empty() {
            crash["frames"] = serde_json::Value::Array(
                info.frames
                    .iter()
                    .map(|&pc| {
                        let mut frame = serde_json::json!({ "addr": format!("0x{pc:x}") });
                        if let Some((name, offset)) = resolve_module_offset(pc, &modules) {
                            frame["module"] = serde_json::Value::from(name);
                            frame["reladdr"] = serde_json::Value::from(offset);
                        }
                        frame
                    })
                    .collect(),
            );
        }
    }
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

/// A module from the install-time map.
#[derive(Clone)]
pub struct ModuleEntry {
    pub base: usize,
    /// Loaded address extent; `usize::MAX` when unknown (legacy map).
    pub size: usize,
    /// Mach-O `LC_UUID` / GNU build-id, lowercase hex. Empty when the module
    /// carries none — the backend symbol store is keyed on this, so such a
    /// module can never be symbolicated.
    pub code_id: String,
    pub name: String,
}

/// The module whose loaded range `[base, base + size)` contains `pc` (the one
/// with the largest such base), and the offset within it. A PC outside every
/// module's range yields `None` — it is SKIPPED rather than misattributed to the
/// nearest-below module with a bogus, ASLR-unstable offset that would defeat the
/// crash-loop blacklist (F25).
fn resolve_module_offset(pc: usize, modules: &[ModuleEntry]) -> Option<(String, u64)> {
    modules
        .iter()
        .filter(|m| m.base <= pc && pc - m.base < m.size)
        .max_by_key(|m| m.base)
        .map(|m| (m.name.clone(), (pc - m.base) as u64))
}

/// Read the install-time module map. Current format is
/// `<base_hex>\t<size_hex>\t<code_id>\t<name>`; two older shapes are still
/// accepted so a marker written by a previous SDK build (and recovered after an
/// upgrade) still yields a report:
/// - `base \t size \t name` — no code id (pre-symbolication builds)
/// - `base \t name`         — no size either; treated as unbounded, preserving
///   the old nearest-module behavior for that entry only.
fn read_modules(path: &Path) -> Vec<ModuleEntry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(4, '\t').collect();
            let base = usize::from_str_radix(parts.first()?.trim(), 16).ok()?;
            let size_of = |s: &str| usize::from_str_radix(s.trim(), 16).unwrap_or(usize::MAX);
            match parts.len() {
                4 => Some(ModuleEntry {
                    base,
                    size: size_of(parts[1]),
                    code_id: parts[2].trim().to_string(),
                    name: parts[3].to_string(),
                }),
                3 => Some(ModuleEntry {
                    base,
                    size: size_of(parts[1]),
                    code_id: String::new(),
                    name: parts[2].to_string(),
                }),
                2 => Some(ModuleEntry {
                    base,
                    size: usize::MAX,
                    code_id: String::new(),
                    name: parts[1].to_string(),
                }),
                _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{try_claim, FORCE_UNSUPPORTED};
    use crate::session::Session;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("bugsee-rec-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(p.join("sessions")).unwrap();
        p
    }

    fn marker(dir: &Path, generation: u64, content: &str) -> PathBuf {
        let m = Session::marker_path(dir, generation);
        std::fs::write(&m, content).unwrap();
        m
    }

    fn gens(dir: &Path) -> Vec<u64> {
        find_pending(dir, u64::MAX)
            .iter()
            .map(|p| p.generation)
            .collect()
    }

    #[test]
    fn a_free_lock_means_the_session_died_whatever_pid_its_marker_names() {
        let dir = tmp();
        marker(&dir, 1, "1"); // a pid that is alive here
        std::fs::write(Session::lock_path(&dir, 1), "").unwrap();
        assert_eq!(gens(&dir), vec![1]);
    }

    #[test]
    fn a_held_lock_means_alive_whatever_pid_its_marker_names() {
        let dir = tmp();
        marker(&dir, 1, "4294967290"); // not a pid in this namespace
        let _held = try_claim(&Session::lock_path(&dir, 1)).unwrap();
        assert!(gens(&dir).is_empty());
    }

    #[test]
    fn a_launching_session_between_lock_file_and_lock_is_not_recovered() {
        // Created the lock file, not yet locked it, not yet written its pid.
        let dir = tmp();
        let m = marker(&dir, 1, "");
        std::fs::write(Session::lock_path(&dir, 1), "").unwrap();
        assert!(gens(&dir).is_empty(), "fresh and empty: still launching");
        let f = std::fs::OpenOptions::new().write(true).open(&m).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
            .unwrap();
        drop(f);
        assert_eq!(gens(&dir), vec![1], "an old empty marker is a dead launch");
    }

    #[test]
    fn without_locks_the_marker_pid_decides() {
        let dir = tmp();
        marker(&dir, 1, "4294967290");
        std::fs::write(Session::lock_path(&dir, 1), "").unwrap();
        #[cfg(unix)]
        {
            marker(&dir, 2, "1");
            std::fs::write(Session::lock_path(&dir, 2), "").unwrap();
        }
        FORCE_UNSUPPORTED.with(|f| f.set(true));
        let found = gens(&dir);
        FORCE_UNSUPPORTED.with(|f| f.set(false));
        // Dead pid: recovered. Live pid (unix): left alone.
        assert_eq!(found, vec![1]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unwritable_leftover_lock_file_does_not_hide_a_crash() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        marker(&dir, 1, "4294967290");
        let lock = Session::lock_path(&dir, 1);
        std::fs::write(&lock, "").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(gens(&dir), vec![1]);
    }

    #[test]
    fn a_fresh_heartbeat_keeps_a_lockless_session_alive_whatever_its_pid() {
        // No lock file (the filesystem has none): the marker's pid, even our own
        // (a peer in another pid namespace), must not condemn a beating session.
        let dir = tmp();
        let m = marker(&dir, 1, &std::process::id().to_string());
        let beat = heartbeat_path(&m);
        std::fs::write(&beat, "").unwrap();
        assert!(gens(&dir).is_empty(), "heartbeat is fresh: alive");
        let f = std::fs::OpenOptions::new().write(true).open(&beat).unwrap();
        f.set_modified(std::time::SystemTime::now() - HEARTBEAT_STALE * 2)
            .unwrap();
        drop(f);
        assert_eq!(gens(&dir), vec![1], "quiet heartbeat: crashed");
    }

    #[test]
    fn claim_pending_gives_up_if_the_session_took_its_lock_since_the_scan() {
        let dir = tmp();
        marker(&dir, 1, "4294967290");
        std::fs::write(Session::lock_path(&dir, 1), "").unwrap();
        let pending = find_pending(&dir, u64::MAX);
        assert_eq!(pending.len(), 1);
        // The launching session finishes locking after the scan.
        let _live = try_claim(&Session::lock_path(&dir, 1)).unwrap();
        assert!(claim_pending(&dir, &pending[0]).is_none());
    }

    #[test]
    fn old_scheme_claim_files_of_dead_sessions_are_swept() {
        let dir = tmp();
        let claim = dir.join("sessions").join("1.claim");
        let guard = dir.join("sessions").join("1.claim.takeover");
        for p in [&claim, &guard] {
            std::fs::write(p, "").unwrap();
            let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(300))
                .unwrap();
        }
        gc_orphans(&dir, 5);
        assert!(!claim.exists() && !guard.exists());
    }
}

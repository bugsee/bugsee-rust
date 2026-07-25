//
//  queue.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The durable outbound queue. Assembled report bundles are persisted here so
//! delivery survives network failures and process restarts: each entry is the
//! bundle ZIP plus a `.req` sidecar (the `request.json` body) and a `.meta`
//! sidecar (the retry counter). The uploader drains this directory with retry
//! caps; a fresh launch resumes it (DESIGN.md §10).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::reporting::AssembledReport;

/// Sub-directory holding queued bundles.
const QUEUE_DIR: &str = "queue";

/// A queued report on disk.
pub struct QueuedReport {
    /// Path to the `*.bundle.zip`.
    pub bundle: PathBuf,
}

impl QueuedReport {
    fn req_path(&self) -> PathBuf {
        sibling(&self.bundle, "req")
    }
    fn meta_path(&self) -> PathBuf {
        sibling(&self.bundle, "meta")
    }
}

fn sibling(bundle: &Path, ext: &str) -> PathBuf {
    let mut s = bundle.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// The queue directory under `data_dir`.
pub fn queue_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(QUEUE_DIR)
}

/// Persist an assembled report to the queue.
///
/// The bundle is written to a temp file and atomically `rename`d into place as
/// the final step, so a concurrent reader (the uploader) or a crash mid-write
/// never observes a partially-written `.bundle.zip`.
pub fn enqueue(data_dir: &Path, report: &AssembledReport) -> std::io::Result<()> {
    let dir = queue_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let bundle = dir.join(&report.bundle_name);
    std::fs::write(sibling(&bundle, "req"), &report.request_json)?;
    // meta: "<retry> <next_attempt_epoch_ms>".
    std::fs::write(sibling(&bundle, "meta"), "0 0")?;
    // Write to a temp path, then atomically rename into place last.
    let tmp = sibling(&bundle, "tmp");
    std::fs::write(&tmp, &report.zip)?;
    std::fs::rename(&tmp, &bundle)?;
    Ok(())
}

/// List queued bundles (entries missing their sidecars are skipped).
pub fn list_pending(data_dir: &Path) -> Vec<QueuedReport> {
    let mut out = Vec::new();
    let read = match std::fs::read_dir(queue_dir(data_dir)) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.to_string_lossy().ends_with(".bundle.zip") {
            let qr = QueuedReport { bundle: path };
            if qr.req_path().exists() {
                out.push(qr);
            }
        }
    }
    out
}

/// Load a queued report's bundle bytes, request body, retry count, and the
/// earliest next-attempt time (epoch ms).
pub fn load(report: &QueuedReport) -> std::io::Result<(Vec<u8>, Vec<u8>, u32, i64)> {
    let zip = std::fs::read(&report.bundle)?;
    let request = std::fs::read(report.req_path())?;
    let (retry, next_attempt_ms) = read_meta(report);
    Ok((zip, request, retry, next_attempt_ms))
}

fn read_meta(report: &QueuedReport) -> (u32, i64) {
    let text = std::fs::read_to_string(report.meta_path()).unwrap_or_default();
    let mut parts = text.split_whitespace();
    let retry = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let next = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (retry, next)
}

/// Record a report's retry count and next-attempt time.
pub fn set_meta(report: &QueuedReport, retry: u32, next_attempt_ms: i64) {
    let _ = std::fs::write(report.meta_path(), format!("{retry} {next_attempt_ms}"));
}

/// The earliest next-attempt time for a queued report (epoch ms), reading only
/// the small meta sidecar.
pub fn next_attempt(report: &QueuedReport) -> i64 {
    read_meta(report).1
}

/// Delete a queued report (all three files).
pub fn remove(report: &QueuedReport) {
    let _ = std::fs::remove_file(&report.bundle);
    let _ = std::fs::remove_file(report.req_path());
    let _ = std::fs::remove_file(report.meta_path());
}

/// The bundle file's base name (for reconstructing an [`AssembledReport`]).
pub fn bundle_name(report: &QueuedReport) -> String {
    report
        .bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The crash dedup signatures a report carries (parsed from its `request.json`).
pub fn signatures_of(report: &AssembledReport) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(&report.request_json)
        .ok()
        .and_then(|v| {
            v.get("signatures")
                .and_then(|s| s.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        })
        .unwrap_or_default()
}

fn blacklist_path(data_dir: &Path) -> PathBuf {
    queue_dir(data_dir).join(".blacklist")
}

fn load_blacklist(data_dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(blacklist_path(data_dir))
        .map(|s| s.lines().filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// Persist crash signatures the server told us to stop sending (`12004`).
pub fn blacklist_add(data_dir: &Path, sigs: &[String]) {
    if sigs.is_empty() {
        return;
    }
    let mut set = load_blacklist(data_dir);
    let mut changed = false;
    for s in sigs {
        changed |= set.insert(s.clone());
    }
    if changed {
        let _ = std::fs::create_dir_all(queue_dir(data_dir));
        let body = set.into_iter().collect::<Vec<_>>().join("\n");
        let _ = std::fs::write(blacklist_path(data_dir), body);
    }
}

/// Whether any of `sigs` is locally blacklisted (report should be suppressed).
pub fn any_blacklisted(data_dir: &Path, sigs: &[String]) -> bool {
    if sigs.is_empty() {
        return false;
    }
    let set = load_blacklist(data_dir);
    sigs.iter().any(|s| set.contains(s))
}

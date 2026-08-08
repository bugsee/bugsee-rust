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
use std::sync::Arc;

use bugsee_platform::Storage;

use crate::platform_io::{io_err, storage_path};
use crate::reporting::AssembledReport;

/// Sub-directory holding queued bundles.
const QUEUE_DIR: &str = "queue";

/// A queued report on disk (relative basename under `queue/`).
pub struct QueuedReport {
    /// Basename of the `*.bundle.zip` under the queue directory.
    pub bundle_name: String,
}

impl QueuedReport {
    fn rel(&self, ext: &str) -> String {
        format!("{QUEUE_DIR}/{}.{ext}", self.bundle_name)
    }

    fn bundle_rel(&self) -> String {
        format!("{QUEUE_DIR}/{}", self.bundle_name)
    }
}

/// Persist an assembled report to the queue.
///
/// The bundle is written to a temp file and atomically `rename`d into place as
/// the final step, so a concurrent reader (the uploader) or a crash mid-write
/// never observes a partially-written `.bundle.zip`.
pub fn enqueue(storage: &dyn Storage, report: &AssembledReport) -> std::io::Result<()> {
    storage
        .create_dir_all(&storage_path(QUEUE_DIR))
        .map_err(io_err)?;
    let qr = QueuedReport {
        bundle_name: report.bundle_name.clone(),
    };
    storage
        .write_file(&storage_path(qr.rel("req")), &report.request_json)
        .map_err(io_err)?;
    storage
        .write_file(&storage_path(qr.rel("meta")), b"0 0")
        .map_err(io_err)?;
    let tmp = qr.rel("tmp");
    storage
        .write_file(&storage_path(&tmp), &report.zip)
        .map_err(io_err)?;
    storage
        .rename(&storage_path(&tmp), &storage_path(qr.bundle_rel()))
        .map_err(io_err)?;
    Ok(())
}

/// List queued bundles (entries missing their sidecars are skipped).
pub fn list_pending(storage: &dyn Storage) -> Vec<QueuedReport> {
    let mut out = Vec::new();
    let read = match storage.read_dir(&storage_path(QUEUE_DIR)) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for name in read {
        if name.ends_with(".bundle.zip") {
            let qr = QueuedReport { bundle_name: name };
            if storage.exists(&storage_path(qr.rel("req"))) {
                out.push(qr);
            }
        }
    }
    out
}

/// Load a queued report's bundle bytes, request body, retry count, and the
/// earliest next-attempt time (epoch ms).
pub fn load(
    storage: &dyn Storage,
    report: &QueuedReport,
) -> std::io::Result<(Vec<u8>, Vec<u8>, u32, i64)> {
    let zip = storage
        .read_file(&storage_path(report.bundle_rel()))
        .map_err(io_err)?;
    let request = storage
        .read_file(&storage_path(report.rel("req")))
        .map_err(io_err)?;
    let (retry, next_attempt_ms) = read_meta(storage, report);
    Ok((zip, request, retry, next_attempt_ms))
}

fn read_meta(storage: &dyn Storage, report: &QueuedReport) -> (u32, i64) {
    let text = storage
        .read_file(&storage_path(report.rel("meta")))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();
    let mut parts = text.split_whitespace();
    let retry = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let next = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (retry, next)
}

/// Record a report's retry count and next-attempt time.
pub fn set_meta(storage: &dyn Storage, report: &QueuedReport, retry: u32, next_attempt_ms: i64) {
    let _ = storage.write_file(
        &storage_path(report.rel("meta")),
        format!("{retry} {next_attempt_ms}").as_bytes(),
    );
}

/// The earliest next-attempt time for a queued report (epoch ms).
pub fn next_attempt(storage: &dyn Storage, report: &QueuedReport) -> i64 {
    read_meta(storage, report).1
}

/// The cached presigned upload endpoint for a report, if any.
pub fn endpoint(storage: &dyn Storage, report: &QueuedReport) -> Option<String> {
    storage
        .read_file(&storage_path(report.rel("endpoint")))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Cache the presigned upload endpoint so the next attempt resumes at the PUT.
pub fn set_endpoint(storage: &dyn Storage, report: &QueuedReport, endpoint: &str) {
    let _ = storage.write_file(&storage_path(report.rel("endpoint")), endpoint.as_bytes());
}

/// Drop any cached presigned endpoint.
pub fn clear_endpoint(storage: &dyn Storage, report: &QueuedReport) {
    let _ = storage.remove_file(&storage_path(report.rel("endpoint")));
}

/// Delete a queued report (bundle + all sidecars).
pub fn remove(storage: &dyn Storage, report: &QueuedReport) {
    let _ = storage.remove_file(&storage_path(report.bundle_rel()));
    let _ = storage.remove_file(&storage_path(report.rel("req")));
    let _ = storage.remove_file(&storage_path(report.rel("meta")));
    let _ = storage.remove_file(&storage_path(report.rel("endpoint")));
}

/// The bundle file's base name (for reconstructing an [`AssembledReport`]).
pub fn bundle_name(report: &QueuedReport) -> String {
    report.bundle_name.clone()
}

/// The crash dedup signatures a report carries (parsed from its `request.json`).
pub fn signatures_of(report: &AssembledReport) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(&report.request_json)
        .ok()
        .and_then(|v| {
            v.get("signatures").and_then(|s| s.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

fn load_blacklist(storage: &dyn Storage) -> BTreeSet<String> {
    storage
        .read_file(&storage_path(format!("{QUEUE_DIR}/.blacklist")))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| {
            s.lines()
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist crash signatures the server told us to stop sending (`12004`).
pub fn blacklist_add(storage: &dyn Storage, sigs: &[String]) {
    if sigs.is_empty() {
        return;
    }
    let existing = load_blacklist(storage);
    let fresh: Vec<&String> = sigs.iter().filter(|s| !existing.contains(*s)).collect();
    if fresh.is_empty() {
        return;
    }
    let _ = storage.create_dir_all(&storage_path(QUEUE_DIR));
    // One write_append per signature so POSIX O_APPEND atomicity holds for
    // concurrent shared-data-dir peers (a batched buffer can exceed PIPE_BUF).
    let path = storage_path(format!("{QUEUE_DIR}/.blacklist"));
    for s in fresh {
        let mut line = Vec::with_capacity(s.len() + 1);
        line.extend_from_slice(s.as_bytes());
        line.push(b'\n');
        let _ = storage.write_append(&path, &line);
    }
}

/// Reap orphaned queue sidecars.
pub fn gc_orphans(storage: &dyn Storage) {
    let read = match storage.read_dir(&storage_path(QUEUE_DIR)) {
        Ok(r) => r,
        Err(_) => return,
    };
    for name in read {
        let bundle_name = name
            .strip_suffix(".req")
            .or_else(|| name.strip_suffix(".meta"))
            .or_else(|| name.strip_suffix(".endpoint"))
            .or_else(|| name.strip_suffix(".tmp"));
        if let Some(bundle_name) = bundle_name {
            if !storage.exists(&storage_path(format!("{QUEUE_DIR}/{bundle_name}"))) {
                let _ = storage.remove_file(&storage_path(format!("{QUEUE_DIR}/{name}")));
            }
        }
    }
}

/// Whether any of `sigs` is locally blacklisted.
pub fn any_blacklisted(storage: &dyn Storage, sigs: &[String]) -> bool {
    if sigs.is_empty() {
        return false;
    }
    let set = load_blacklist(storage);
    sigs.iter().any(|s| set.contains(s))
}

/// Arc convenience wrappers used by the runtime worker threads.
pub fn enqueue_arc(storage: &Arc<dyn Storage>, report: &AssembledReport) -> std::io::Result<()> {
    enqueue(storage.as_ref(), report)
}

//
//  export.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Snapshot export: read a report's hard-linked parts, filter entries to the
//! window `[start, end]`, and emit one envelope-wrapped JSON document per
//! channel. Because payloads are already JSON, the envelope is assembled by
//! concatenation — no re-serialization.

use std::collections::BTreeMap;
use std::sync::Arc;

use bugsee_platform::Storage;

use crate::platform_io::{io_err, storage_path};

use super::record::RecordIter;

/// One exported per-type capture document.
pub struct ChannelDocument {
    /// The wire `type` (e.g. `log`, `network`, `events.user`).
    pub channel: String,
    /// The full `{"version":2,"events":[…]}` document bytes.
    pub bytes: Vec<u8>,
}

const ENVELOPE_PREFIX: &[u8] = br#"{"version":2,"events":["#;
const ENVELOPE_SUFFIX: &[u8] = b"]}";
/// APM uses a `{"transactions":[…]}` envelope instead of the event envelope.
const PERF_PREFIX: &[u8] = br#"{"transactions":["#;

/// Read the hard-linked parts under `report_prefix` (relative to `storage`),
/// keep entries whose timestamp is within `[start, end]`, and return one
/// document per non-empty channel. Documents are ordered by channel name.
pub fn export_report(
    storage: &dyn Storage,
    report_prefix: &str,
    start: i64,
    end: i64,
) -> std::io::Result<Vec<ChannelDocument>> {
    let mut channels: BTreeMap<String, (Vec<u8>, usize)> = BTreeMap::new();

    for part_rel in part_dirs_sorted(storage, report_prefix)? {
        let part = storage_path(&part_rel);
        let entries = match storage.read_dir(&part) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for fname in entries {
            let channel = match fname.strip_suffix(".part") {
                Some(c) => c.to_string(),
                None => continue,
            };
            let file = part
                .join(&fname)
                .ok_or_else(|| std::io::Error::other("bad join"))?;
            let data = storage.read_file(&file).map_err(io_err)?;
            let slot = channels.entry(channel).or_insert_with(|| (Vec::new(), 0));
            for (ts, payload) in RecordIter::new(&data) {
                if ts < start || ts > end {
                    continue;
                }
                if slot.1 > 0 {
                    slot.0.push(b',');
                }
                slot.0.extend_from_slice(payload);
                slot.1 += 1;
            }
        }
    }

    let mut docs = Vec::new();
    for (channel, (events, count)) in channels {
        if count == 0 {
            continue;
        }
        let prefix: &[u8] = if channel == "performance" {
            PERF_PREFIX
        } else {
            ENVELOPE_PREFIX
        };
        let mut bytes = Vec::with_capacity(prefix.len() + events.len() + ENVELOPE_SUFFIX.len());
        bytes.extend_from_slice(prefix);
        bytes.extend_from_slice(&events);
        bytes.extend_from_slice(ENVELOPE_SUFFIX);
        docs.push(ChannelDocument { channel, bytes });
    }
    Ok(docs)
}

/// The `(min, max)` entry timestamp span across all parts under `report_prefix`,
/// or `None` if there are no records.
pub fn report_span(
    storage: &dyn Storage,
    report_prefix: &str,
) -> std::io::Result<Option<(i64, i64)>> {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    let mut any = false;
    for part_rel in part_dirs_sorted(storage, report_prefix)? {
        let part = storage_path(&part_rel);
        let entries = match storage.read_dir(&part) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for fname in entries {
            if !fname.ends_with(".part") {
                continue;
            }
            let file = part
                .join(&fname)
                .ok_or_else(|| std::io::Error::other("bad join"))?;
            let data = storage.read_file(&file).map_err(io_err)?;
            for (ts, _) in RecordIter::new(&data) {
                any = true;
                min = min.min(ts);
                max = max.max(ts);
            }
        }
    }
    Ok(any.then_some((min, max)))
}

/// Convenience when the caller already holds an `Arc`.
pub fn export_report_arc(
    storage: &Arc<dyn Storage>,
    report_prefix: &str,
    start: i64,
    end: i64,
) -> std::io::Result<Vec<ChannelDocument>> {
    export_report(storage.as_ref(), report_prefix, start, end)
}

/// Relative paths of numeric part directories under `report_prefix`, sorted.
fn part_dirs_sorted(storage: &dyn Storage, report_prefix: &str) -> std::io::Result<Vec<String>> {
    let root = storage_path(report_prefix);
    let read = match storage.read_dir(&root) {
        Ok(r) => r,
        Err(bugsee_platform::StorageError::NotFound) => return Ok(Vec::new()),
        Err(e) => return Err(io_err(e)),
    };
    let mut parts: Vec<(u64, String)> = Vec::new();
    for name in read {
        if let Ok(num) = name.parse::<u64>() {
            parts.push((num, format!("{report_prefix}/{name}")));
        }
    }
    parts.sort_by_key(|(n, _)| *n);
    Ok(parts.into_iter().map(|(_, p)| p).collect())
}

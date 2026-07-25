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
use std::path::Path;

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

/// Read the hard-linked parts under `report_dir`, keep entries whose timestamp
/// is within `[start, end]`, and return one document per non-empty channel.
/// Documents are ordered by channel name for determinism.
pub fn export_report(
    report_dir: &Path,
    start: i64,
    end: i64,
) -> std::io::Result<Vec<ChannelDocument>> {
    // channel -> (events buffer, count)
    let mut channels: BTreeMap<String, (Vec<u8>, usize)> = BTreeMap::new();

    for part in part_dirs_sorted(report_dir)? {
        let entries = match std::fs::read_dir(&part) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for file in entries.flatten() {
            let fname = file.file_name();
            let fname = fname.to_string_lossy();
            let channel = match fname.strip_suffix(".part") {
                Some(c) => c.to_string(),
                None => continue,
            };
            let data = std::fs::read(file.path())?;
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

/// The `(min, max)` entry timestamp span across all parts under `report_dir`,
/// or `None` if there are no records. Used to set the manifest window for a
/// recovered session where the live span is not known in memory.
pub fn report_span(report_dir: &Path) -> std::io::Result<Option<(i64, i64)>> {
    let mut min = i64::MAX;
    let mut max = i64::MIN;
    let mut any = false;
    for part in part_dirs_sorted(report_dir)? {
        let entries = match std::fs::read_dir(&part) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for file in entries.flatten() {
            if !file.file_name().to_string_lossy().ends_with(".part") {
                continue;
            }
            let data = std::fs::read(file.path())?;
            for (ts, _) in RecordIter::new(&data) {
                any = true;
                min = min.min(ts);
                max = max.max(ts);
            }
        }
    }
    Ok(any.then_some((min, max)))
}

/// The numeric part sub-directories of `report_dir`, sorted ascending.
fn part_dirs_sorted(report_dir: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut parts: Vec<(u64, std::path::PathBuf)> = Vec::new();
    let read = match std::fs::read_dir(report_dir) {
        Ok(r) => r,
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    for entry in read.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if let Ok(num) = entry.file_name().to_string_lossy().parse::<u64>() {
            parts.push((num, entry.path()));
        }
    }
    parts.sort_by_key(|(n, _)| *n);
    Ok(parts.into_iter().map(|(_, p)| p).collect())
}

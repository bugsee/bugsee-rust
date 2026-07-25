//
//  store.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The disk-backed part store: append streams, ~time/byte-bounded eviction, and
//! hard-link snapshots.
//!
//! Layout: `<parts_root>/<part_num>/<channel>.part`, where `<parts_root>` is
//! `<data>/parts/<gen>`. Each part is a directory of per-channel append files.
//! Rotation starts a new part; eviction `unlink`s whole old parts (refcount-safe
//! against live snapshots). A snapshot hard-links the parts intersecting a
//! window into a report directory without copying payload bytes.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use super::record;

/// Bounds on the sliding capture window. Eviction runs after each rotation and
/// removes the oldest parts while any cap is exceeded.
#[derive(Debug, Clone, Copy)]
pub struct WindowCaps {
    /// Max wall-time span retained, ms.
    pub max_window_ms: i64,
    /// Max total on-disk bytes across retained parts.
    pub max_bytes: u64,
    /// Max total entry count retained across parts.
    pub max_events: u64,
}

impl Default for WindowCaps {
    fn default() -> Self {
        WindowCaps {
            max_window_ms: 60_000,
            max_bytes: 8 << 20,
            max_events: 100_000,
        }
    }
}

/// In-memory metadata for one part.
#[derive(Debug, Clone)]
struct PartMeta {
    num: u64,
    start_ts: i64,
    end_ts: i64,
    bytes: u64,
    /// Number of appended entries in this part (for the event-count cap).
    count: u64,
}

/// A generation's worth of capture parts.
pub struct PartStore {
    parts_root: PathBuf,
    current: u64,
    open: HashMap<String, File>,
    parts: Vec<PartMeta>,
    caps: WindowCaps,
    total_bytes: u64,
    total_events: u64,
}

impl PartStore {
    /// Open (creating) the parts root for `generation` under `data_dir`.
    pub fn new(data_dir: &Path, generation: u64, caps: WindowCaps) -> std::io::Result<Self> {
        let parts_root = data_dir.join("parts").join(generation.to_string());
        let mut store = PartStore {
            parts_root,
            current: 0,
            open: HashMap::new(),
            parts: Vec::new(),
            caps,
            total_bytes: 0,
            total_events: 0,
        };
        store.begin_part(0)?;
        Ok(store)
    }

    fn begin_part(&mut self, num: u64) -> std::io::Result<()> {
        std::fs::create_dir_all(self.part_dir(num))?;
        self.parts.push(PartMeta {
            num,
            start_ts: i64::MAX,
            end_ts: i64::MIN,
            bytes: 0,
            count: 0,
        });
        self.current = num;
        Ok(())
    }

    fn part_dir(&self, num: u64) -> PathBuf {
        self.parts_root.join(num.to_string())
    }

    /// Append a framed `payload` for `channel`, stamped `ts` (epoch ms).
    pub fn append(&mut self, channel: &str, ts: i64, payload: &[u8]) -> std::io::Result<()> {
        if !self.open.contains_key(channel) {
            let path = self.part_dir(self.current).join(format!("{channel}.part"));
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.open.insert(channel.to_string(), file);
        }
        let mut framed = Vec::with_capacity(record::framed_len(payload.len()));
        record::frame(ts, payload, &mut framed);
        self.open.get_mut(channel).unwrap().write_all(&framed)?;

        let n = framed.len() as u64;
        self.total_bytes += n;
        self.total_events += 1;
        let cur = self.parts.last_mut().expect("current part exists");
        cur.start_ts = cur.start_ts.min(ts);
        cur.end_ts = cur.end_ts.max(ts);
        cur.bytes += n;
        cur.count += 1;
        Ok(())
    }

    /// Finalize the active part and start a new one, then evict.
    pub fn rotate(&mut self) -> std::io::Result<()> {
        for (_, mut f) in self.open.drain() {
            let _ = f.flush();
        }
        let next = self.current + 1;
        self.begin_part(next)?;
        self.evict()?;
        Ok(())
    }

    /// Remove oldest parts while a cap is exceeded (never the current part).
    fn evict(&mut self) -> std::io::Result<()> {
        loop {
            if self.parts.len() <= 1 {
                break;
            }
            // The newest real timestamp across all parts. `rotate()` calls
            // `evict()` right after pushing a fresh empty part (end_ts = MIN), so
            // `parts.last()` would always be that empty part — using it made the
            // time cap dead. Exclude empty parts here.
            let newest_end = self
                .parts
                .iter()
                .map(|p| p.end_ts)
                .filter(|&t| t != i64::MIN)
                .max()
                .unwrap_or(i64::MIN);
            let oldest = &self.parts[0];
            let span_exceeded = newest_end != i64::MIN
                && oldest.start_ts != i64::MAX
                && newest_end - oldest.start_ts > self.caps.max_window_ms;
            let bytes_exceeded = self.total_bytes > self.caps.max_bytes;
            let events_exceeded = self.total_events > self.caps.max_events;
            if !span_exceeded && !bytes_exceeded && !events_exceeded {
                break;
            }
            let victim = self.parts.remove(0);
            let _ = std::fs::remove_dir_all(self.part_dir(victim.num));
            self.total_bytes = self.total_bytes.saturating_sub(victim.bytes);
            self.total_events = self.total_events.saturating_sub(victim.count);
        }
        Ok(())
    }

    /// Flush all open part files to the OS.
    pub fn flush(&mut self) -> std::io::Result<()> {
        for f in self.open.values_mut() {
            f.flush()?;
        }
        Ok(())
    }

    /// The `[start, end]` timestamp span currently retained across parts.
    pub fn retained_span(&self) -> Option<(i64, i64)> {
        let start = self
            .parts
            .iter()
            .map(|p| p.start_ts)
            .filter(|&t| t != i64::MAX)
            .min()?;
        let end = self
            .parts
            .iter()
            .map(|p| p.end_ts)
            .filter(|&t| t != i64::MIN)
            .max()?;
        Some((start, end))
    }

    /// Hard-link the parts intersecting `[start, end]` into `dest_dir`, so the
    /// snapshot shares storage with the live window. Falls back to copy when
    /// hard links are unsupported (cross-volume / FAT).
    pub fn snapshot_into(&mut self, dest_dir: &Path, start: i64, end: i64) -> std::io::Result<()> {
        // Flush so the linked inodes carry the latest committed records.
        self.flush()?;
        for meta in &self.parts {
            let intersects = meta.start_ts == i64::MAX // empty current part — include, may fill
                || (meta.start_ts <= end && meta.end_ts >= start);
            if !intersects {
                continue;
            }
            let src_dir = self.part_dir(meta.num);
            let dst_dir = dest_dir.join(meta.num.to_string());
            std::fs::create_dir_all(&dst_dir)?;
            let entries = match std::fs::read_dir(&src_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let src = src_dir.join(&name);
                let dst = dst_dir.join(&name);
                if std::fs::hard_link(&src, &dst).is_err() {
                    std::fs::copy(&src, &dst)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bugsee-store-{}", crate::util::random_hex(8)));
        p
    }

    #[test]
    fn event_count_cap_evicts_oldest_parts() {
        let dir = tmp_dir();
        // Isolate the event-count cap: time and byte caps are effectively
        // unbounded so only the count trigger can fire.
        let caps = WindowCaps {
            max_window_ms: i64::MAX,
            max_bytes: u64::MAX,
            max_events: 2,
        };
        let mut store = PartStore::new(&dir, 0, caps).unwrap();

        // One entry per part, rotating after each so eviction has a chance to run.
        for i in 0..6i64 {
            store.append("log", 1000 + i, b"x").unwrap();
            store.rotate().unwrap();
        }

        // The running count must stay at or below the cap after eviction.
        assert!(
            store.total_events <= caps.max_events,
            "total_events={} exceeded cap={}",
            store.total_events,
            caps.max_events
        );
        // Eviction actually happened: far fewer parts than the 7 created.
        assert!(
            store.parts.len() < 7,
            "expected eviction, still have {} parts",
            store.parts.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

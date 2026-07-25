//
//  session.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session generations and liveness markers.
//!
//! Each launch claims a fresh generation by **atomically** creating its
//! `<gen>.alive` marker with `O_EXCL`, so two processes sharing a data dir never
//! collide on a generation (which would corrupt the append streams). The
//! generation floor is derived from the on-disk state (existing markers/parts +
//! the `gen` hint), so a torn/lost `gen` file after power loss cannot make a new
//! session reuse a lower number and hide prior crashes. A clean shutdown removes
//! the marker; a marker surviving into the next launch means an abnormal exit.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The name of the generation counter hint file under the data dir.
const GEN_FILE: &str = "gen";
/// Sub-directory holding per-generation liveness markers.
const SESSIONS_DIR: &str = "sessions";

/// A launched session: owns its generation and liveness marker.
pub struct Session {
    data_dir: PathBuf,
    generation: u64,
}

impl Session {
    /// Begin a session: atomically claim the next free generation and write its
    /// liveness marker.
    pub fn begin(data_dir: &Path) -> std::io::Result<Session> {
        std::fs::create_dir_all(data_dir)?;
        let markers = data_dir.join(SESSIONS_DIR);
        std::fs::create_dir_all(&markers)?;

        let mut candidate = floor_generation(data_dir) + 1;
        let generation = loop {
            let marker = markers.join(format!("{candidate}.alive"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
            {
                Ok(mut f) => {
                    let _ = write!(f, "{}", std::process::id());
                    break candidate;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    candidate += 1;
                }
                Err(e) => return Err(e),
            }
        };

        // Advance the hint (best-effort; the floor scan is the source of truth).
        let _ = std::fs::write(data_dir.join(GEN_FILE), generation.to_string());

        Ok(Session {
            data_dir: data_dir.to_path_buf(),
            generation,
        })
    }

    /// This session's generation number.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The liveness-marker path for `generation` under `data_dir`.
    pub fn marker_path(data_dir: &Path, generation: u64) -> PathBuf {
        data_dir
            .join(SESSIONS_DIR)
            .join(format!("{generation}.alive"))
    }

    /// Mark a clean shutdown: remove this session's liveness marker.
    pub fn end(&self) {
        let _ = std::fs::remove_file(Session::marker_path(&self.data_dir, self.generation));
    }
}

/// The highest generation number implied by on-disk state: the `gen` hint, any
/// `sessions/<n>.alive` marker, and any `parts/<n>` directory. Deriving the
/// floor from reality (not just the hint) prevents a lost `gen` file from
/// resetting to a lower generation and hiding un-recovered crashes.
fn floor_generation(data_dir: &Path) -> u64 {
    let mut floor = std::fs::read_to_string(data_dir.join(GEN_FILE))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0u64);

    floor = floor.max(max_numeric_entry(
        &data_dir.join(SESSIONS_DIR),
        Some(".alive"),
    ));
    floor = floor.max(max_numeric_entry(&data_dir.join("parts"), None));
    floor
}

/// The largest `u64` parsed from directory-entry names (optionally stripping a
/// suffix like `.alive`), or 0 if none.
fn max_numeric_entry(dir: &Path, strip_suffix: Option<&str>) -> u64 {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut max = 0;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let stem = match strip_suffix {
            Some(sfx) => name.strip_suffix(sfx),
            None => Some(name.as_str()),
        };
        if let Some(n) = stem.and_then(|s| s.parse::<u64>().ok()) {
            max = max.max(n);
        }
    }
    max
}

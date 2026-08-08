//
//  session.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session generations and liveness markers.
//!
//! Each launch claims a fresh generation by **atomically** creating its
//! `<gen>.alive` marker with exclusive create, so two processes sharing a data
//! dir never collide on a generation (which would corrupt the append streams).
//! The generation floor is derived from on-disk state (existing markers/parts +
//! the `gen` hint), so a torn/lost `gen` file after power loss cannot make a new
//! session reuse a lower number and hide prior crashes. A clean shutdown removes
//! the marker; a marker surviving into the next launch means an abnormal exit.

use std::sync::Arc;

use bugsee_platform::{Storage, StorageError};

use crate::platform_io::{io_err, storage_path};

/// The name of the generation counter hint file under the data dir.
const GEN_FILE: &str = "gen";
/// Sub-directory holding per-generation liveness markers.
const SESSIONS_DIR: &str = "sessions";

/// A launched session: owns its generation and liveness marker.
pub struct Session {
    storage: Arc<dyn Storage>,
    generation: u64,
}

impl Session {
    /// Begin a session: atomically claim the next free generation and write its
    /// liveness marker.
    pub fn begin(storage: Arc<dyn Storage>) -> std::io::Result<Session> {
        storage
            .create_dir_all(&storage_path(SESSIONS_DIR))
            .map_err(io_err)?;

        let mut candidate = floor_generation(storage.as_ref()) + 1;
        let generation = loop {
            let marker = storage_path(format!("{SESSIONS_DIR}/{candidate}.alive"));
            let pid = std::process::id().to_string();
            match storage.create_file_exclusive(&marker, pid.as_bytes()) {
                Ok(()) => break candidate,
                Err(StorageError::AlreadyExists) => {
                    candidate += 1;
                }
                Err(e) => return Err(io_err(e)),
            }
        };

        let _ = storage.write_file(
            &storage_path(GEN_FILE),
            generation.to_string().as_bytes(),
        );

        Ok(Session {
            storage,
            generation,
        })
    }

    /// This session's generation number.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Relative marker path for `generation`.
    pub fn marker_rel(generation: u64) -> String {
        format!("{SESSIONS_DIR}/{generation}.alive")
    }

    /// Mark a clean shutdown: remove this session's liveness marker.
    pub fn end(&self) {
        let _ = self
            .storage
            .remove_file(&storage_path(Self::marker_rel(self.generation)));
    }
}

fn floor_generation(storage: &dyn Storage) -> u64 {
    let mut floor = storage
        .read_file(&storage_path(GEN_FILE))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0u64);

    floor = floor.max(max_numeric_entry(storage, SESSIONS_DIR, Some(".alive")));
    floor = floor.max(max_numeric_entry(storage, "parts", None));
    floor
}

fn max_numeric_entry(storage: &dyn Storage, dir: &str, strip_suffix: Option<&str>) -> u64 {
    let read = match storage.read_dir(&storage_path(dir)) {
        Ok(r) => r,
        Err(_) => return 0,
    };
    let mut max = 0;
    for name in read {
        let stem = match strip_suffix {
            Some(sfx) => name.strip_suffix(sfx).map(|s| s.to_string()),
            None => Some(name),
        };
        if let Some(n) = stem.and_then(|s| s.parse::<u64>().ok()) {
            max = max.max(n);
        }
    }
    max
}

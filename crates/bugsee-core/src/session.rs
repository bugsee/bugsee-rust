//
//  session.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Session generations and liveness markers.
//!
//! Each launch bumps a persistent generation counter and writes a `.alive`
//! marker. A clean shutdown removes the marker; a marker that survives into the
//! next launch means the prior session ended abnormally (crash / OS-kill / OOM)
//! and its captured window should be recovered and delivered (DESIGN.md §5).

use std::path::{Path, PathBuf};

/// The name of the generation counter file under the data dir.
const GEN_FILE: &str = "gen";
/// Sub-directory holding per-generation liveness markers.
const SESSIONS_DIR: &str = "sessions";

/// A launched session: owns its generation and liveness marker.
pub struct Session {
    data_dir: PathBuf,
    generation: u64,
}

impl Session {
    /// Begin a session: bump the generation counter and write a liveness marker.
    pub fn begin(data_dir: &Path) -> std::io::Result<Session> {
        std::fs::create_dir_all(data_dir)?;
        let gen_path = data_dir.join(GEN_FILE);
        let last: u64 = std::fs::read_to_string(&gen_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let generation = last + 1;
        std::fs::write(&gen_path, generation.to_string())?;

        let markers = data_dir.join(SESSIONS_DIR);
        std::fs::create_dir_all(&markers)?;
        std::fs::write(
            markers.join(format!("{generation}.alive")),
            std::process::id().to_string(),
        )?;

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
        data_dir.join(SESSIONS_DIR).join(format!("{generation}.alive"))
    }

    /// Mark a clean shutdown: remove this session's liveness marker.
    pub fn end(&self) {
        let _ = std::fs::remove_file(Session::marker_path(&self.data_dir, self.generation));
    }
}

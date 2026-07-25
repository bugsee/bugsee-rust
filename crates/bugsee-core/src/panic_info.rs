//
//  panic_info.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The panic snapshot persisted by the panic observer before an aborting panic
//! destroys the stack, and read back by next-launch recovery to correlate a
//! Rust panic with the `SIGABRT` it produced (DESIGN.md §20).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::crash::Frame;

/// File name of the persisted panic snapshot under a session's part dir.
pub const PANIC_INFO_NAME: &str = "panic.info";

/// A captured panic, persisted for next-launch correlation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanicInfo {
    pub reason: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: u32,
    #[serde(default)]
    pub column: u32,
    pub timestamp: i64,
    #[serde(default)]
    pub frames: Vec<Frame>,
}

impl PanicInfo {
    /// Persist the snapshot as JSON at `path`.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, bytes)
    }

    /// Read a persisted snapshot, or `None` if absent/unparseable.
    pub fn read_from(path: &Path) -> Option<PanicInfo> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

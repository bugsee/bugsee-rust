//
//  bundle.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The report-bundle ZIP writer. A bundle is a single flat ZIP
//! (`<random20>.bundle.zip`, no directories) with per-entry compression:
//! `request.json` and already-compressed media are STORED; JSON/text use
//! Zstandard (ZIP method 93), which the backend ingests natively.

use std::io::{Seek, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Per-entry ZIP compression choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryCompression {
    /// ZIP method 0 — no compression.
    Store,
    /// ZIP method 93 — Zstandard.
    Zstd,
}

/// One file to place in the bundle.
pub struct BundleEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub compression: EntryCompression,
}

impl BundleEntry {
    /// A bundle entry whose compression is chosen by the standard policy.
    pub fn new(name: impl Into<String>, data: Vec<u8>) -> Self {
        let name = name.into();
        let compression = compression_for(&name);
        BundleEntry {
            name,
            data,
            compression,
        }
    }
}

/// The mobile-parity compression policy: STORE `request.json` and already-
/// compressed media; Zstd (method 93) everything else.
pub fn compression_for(name: &str) -> EntryCompression {
    if name == "request.json" {
        return EntryCompression::Store;
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mov" | "mp4" | "m4v" | "png" | "jpg" | "jpeg" | "zip" | "gz" => EntryCompression::Store,
        _ => EntryCompression::Zstd,
    }
}

/// Generate the bundle file name (`<random20>.bundle.zip`).
pub fn bundle_filename() -> String {
    format!("{}.bundle.zip", crate::util::random_hex(10))
}

/// Write `entries` as a flat report-bundle ZIP into `writer`, returning the
/// inner writer once the central directory is finalized.
pub fn write_bundle<W: Write + Seek>(
    writer: W,
    entries: &[BundleEntry],
) -> zip::result::ZipResult<W> {
    let mut zip = ZipWriter::new(writer);
    for entry in entries {
        let method = match entry.compression {
            EntryCompression::Store => CompressionMethod::Stored,
            EntryCompression::Zstd => CompressionMethod::Zstd,
        };
        let options = SimpleFileOptions::default().compression_method(method);
        zip.start_file(entry.name.as_str(), options)?;
        zip.write_all(&entry.data)?;
    }
    zip.finish()
}

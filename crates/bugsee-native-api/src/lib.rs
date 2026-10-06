//
//  lib.rs
//  bugsee-native-api
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The abstraction every native fatal-crash backend implements.
//!
//! The SDK does not care HOW a fatal crash is caught — an in-process handler
//! written in Rust, PLCrashReporter on Apple platforms, Crashpad on Android —
//! only that, once installed, a crash leaves behind what next-launch recovery
//! needs. A backend is therefore a small thing: it can be [installed](CrashBackend::install)
//! for the current session, and installing returns a guard that keeps it active.
//!
//! This crate holds only that contract, with no dependencies, so that backend
//! crates (which may wrap large third-party C/C++ code and carry its licence)
//! depend on the contract rather than on the SDK or on each other.

use std::io;
use std::path::{Path, PathBuf};

/// What a backend needs to know to install itself for one session.
///
/// `#[non_exhaustive]`: backends need more knobs over time (a storage directory
/// that must not collide with the host app's own crash reporter, for one), and
/// adding a field must not break every backend. Build one with [`BackendConfig::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BackendConfig {
    /// Where this session's crash marker is written. Its parent directory is the
    /// session's capture directory, which backends may also use for sidecar files.
    pub crash_info_path: PathBuf,
}

impl BackendConfig {
    /// A configuration for the session whose crash marker lives at `crash_info_path`.
    pub fn new(crash_info_path: impl Into<PathBuf>) -> Self {
        Self {
            crash_info_path: crash_info_path.into(),
        }
    }

    /// The session's capture directory (the marker's parent), if it has one.
    pub fn session_dir(&self) -> Option<&Path> {
        self.crash_info_path.parent()
    }
}

/// Keeps an installed backend active. Dropping it uninstalls the backend.
///
/// `Send` because the SDK stores the guard in a process-wide slot that is
/// replaced and dropped from whichever thread relaunches or shuts down.
pub trait BackendGuard: Send {}

/// A native fatal-crash backend.
pub trait CrashBackend {
    /// A short, stable identifier (`"in-process"`, `"plcrashreporter"`, ...),
    /// used for diagnostics and for choosing a backend explicitly.
    fn name(&self) -> &'static str;

    /// Install the backend for the session described by `config`.
    ///
    /// Failing is not fatal to the SDK: the caller treats an error as "no native
    /// capture this run" and carries on. Return [`io::ErrorKind::Unsupported`]
    /// for a backend that cannot work on this target.
    fn install(&self, config: &BackendConfig) -> io::Result<Box<dyn BackendGuard>>;
}

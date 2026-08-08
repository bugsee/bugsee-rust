//! Typed platform errors (`DESIGN_PLATFORM.md` §6).

use std::fmt;

/// Durable storage failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    NotFound,
    AlreadyExists,
    Full,
    Permission,
    /// Operation not supported on this volume/backend (e.g. hardlink on FAT).
    Unsupported,
    Other(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::NotFound => write!(f, "not found"),
            StorageError::AlreadyExists => write!(f, "already exists"),
            StorageError::Full => write!(f, "storage full"),
            StorageError::Permission => write!(f, "permission denied"),
            StorageError::Unsupported => write!(f, "unsupported"),
            StorageError::Other(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for StorageError {}

/// Low-level HTTP failure — uploader must honor NotReady/Suspended without
/// burning the durable abandon budget (`DESIGN_PLATFORM.md` §6.2 / P14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    NotReady,
    Suspended,
    Transient(String),
    Permanent(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::NotReady => write!(f, "network not ready"),
            HttpError::Suspended => write!(f, "process suspended"),
            HttpError::Transient(s) => write!(f, "transient: {s}"),
            HttpError::Permanent(s) => write!(f, "permanent: {s}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Entropy / RNG failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntropyError {
    Unavailable(String),
}

impl fmt::Display for EntropyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EntropyError::Unavailable(s) => write!(f, "entropy unavailable: {s}"),
        }
    }
}

impl std::error::Error for EntropyError {}

//! Thin helpers mapping [`bugsee_platform`] errors into `std::io` for call-site
//! continuity during the Phase 1a Storage threading.

use bugsee_platform::{StorageError, StoragePath};

pub fn storage_path(s: impl AsRef<str>) -> StoragePath {
    StoragePath::new(s.as_ref())
}

pub fn io_err(err: StorageError) -> std::io::Error {
    use std::io::{Error, ErrorKind};
    match err {
        StorageError::NotFound => Error::new(ErrorKind::NotFound, err),
        StorageError::AlreadyExists => Error::new(ErrorKind::AlreadyExists, err),
        StorageError::Permission => Error::new(ErrorKind::PermissionDenied, err),
        StorageError::Full => Error::new(ErrorKind::OutOfMemory, err),
        StorageError::Unsupported => Error::new(ErrorKind::Unsupported, err),
        StorageError::Other(s) => Error::other(s),
    }
}

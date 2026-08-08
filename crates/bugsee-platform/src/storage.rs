//! Durable byte storage under an SDK data root (`DESIGN_PLATFORM.md` §6.1).

use crate::error::StorageError;
use crate::path::StoragePath;

/// Capability probes cached per data volume after first probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StorageCaps {
    pub hardlink: bool,
    pub reflink: bool,
    pub symlink: bool,
}

/// Metadata for a single file (not directories).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageFileMeta {
    pub len: u64,
    pub is_file: bool,
    /// Last modification time as Unix epoch milliseconds, when the backend
    /// can provide it (used by recovery empty-marker grace).
    pub modified_unix_ms: Option<i64>,
}

/// Open append handle (`DESIGN_PLATFORM.md` §6.1 open_create_append / write / flush).
pub trait AppendSink: Send {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), StorageError>;
    fn flush(&mut self) -> Result<(), StorageError>;
}

/// Platform-agnostic durable storage.
///
/// Paths are [`StoragePath`] values relative to the backend's data root.
/// Implementations must treat `rename` as the atomic durability primitive on
/// the same volume.
pub trait Storage: Send + Sync {
    fn create_dir_all(&self, path: &StoragePath) -> Result<(), StorageError>;

    fn remove_file(&self, path: &StoragePath) -> Result<(), StorageError>;

    fn remove_dir_all(&self, path: &StoragePath) -> Result<(), StorageError>;

    /// Atomic on the same volume (temp → final).
    fn rename(&self, from: &StoragePath, to: &StoragePath) -> Result<(), StorageError>;

    /// Create or append; returns bytes written in this call when useful.
    fn write_append(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError>;

    /// Open for create/append; caller holds the sink across many writes.
    fn open_append(&self, path: &StoragePath) -> Result<Box<dyn AppendSink>, StorageError>;

    /// Overwrite-create full file contents (for small sidecars / markers).
    fn write_file(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError>;

    /// Create a new file exclusively (`O_EXCL` / `create_new`). Fails with
    /// [`StorageError::AlreadyExists`] if the path is already present.
    fn create_file_exclusive(
        &self,
        path: &StoragePath,
        bytes: &[u8],
    ) -> Result<(), StorageError>;

    fn read_file(&self, path: &StoragePath) -> Result<Vec<u8>, StorageError>;

    fn metadata(&self, path: &StoragePath) -> Result<StorageFileMeta, StorageError>;

    /// List immediate child names (file or directory basenames).
    fn read_dir(&self, path: &StoragePath) -> Result<Vec<String>, StorageError>;

    /// Probe link capabilities once; backends may cache internally.
    fn probe_caps(&self) -> StorageCaps;

    fn hard_link(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError>;

    fn reflink_or_clone(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError>;

    fn symlink(&self, target: &StoragePath, link: &StoragePath) -> Result<(), StorageError>;

    /// Exists as file or directory.
    fn exists(&self, path: &StoragePath) -> bool;

    /// Byte-copy helper used when hardlink/reflink are unavailable.
    fn copy_file(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        let bytes = self.read_file(src)?;
        self.write_file(dst, &bytes)
    }
}

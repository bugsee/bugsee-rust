//! `std::fs` [`Storage`] rooted at a data directory.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;

use bugsee_platform::{
    AppendSink, Storage, StorageCaps, StorageError, StorageFileMeta, StoragePath,
};

#[cfg(test)]
thread_local! {
    /// When true, the next [`FsStorage::create_file_exclusive`] write fails after
    /// `create_new` (mutation-kill for empty-marker rollback).
    static FAIL_NEXT_EXCLUSIVE_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_exclusive_write() {
    FAIL_NEXT_EXCLUSIVE_WRITE.with(|c| c.set(true));
}

/// Filesystem storage under `root`.
pub struct FsStorage {
    root: PathBuf,
    caps: OnceLock<StorageCaps>,
}

impl FsStorage {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(FsStorage {
            root,
            caps: OnceLock::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn abs(&self, path: &StoragePath) -> PathBuf {
        self.root.join(path.as_path())
    }

    pub(crate) fn map_io(err: std::io::Error) -> StorageError {
        use std::io::ErrorKind;
        match err.kind() {
            ErrorKind::NotFound => StorageError::NotFound,
            ErrorKind::AlreadyExists => StorageError::AlreadyExists,
            ErrorKind::PermissionDenied => StorageError::Permission,
            ErrorKind::OutOfMemory => StorageError::Full,
            ErrorKind::Unsupported => StorageError::Unsupported,
            _ => {
                // ENOSPC often surfaces as Other on some platforms.
                let raw = err.raw_os_error();
                if raw == Some(28) /* ENOSPC unix */ || raw == Some(112) /* ERROR_DISK_FULL win */ {
                    StorageError::Full
                } else {
                    StorageError::Other(err.to_string())
                }
            }
        }
    }

    fn probe_once(root: &Path) -> StorageCaps {
        let mut caps = StorageCaps::default();
        let probe = root.join(".bugsee-cap-probe");
        let _ = fs::create_dir_all(&probe);
        let src = probe.join("src");
        let _ = fs::write(&src, b"x");

        let hl = probe.join("hl");
        let _ = fs::remove_file(&hl);
        caps.hardlink = fs::hard_link(&src, &hl).is_ok();

        let rl = probe.join("rl");
        let _ = fs::remove_file(&rl);
        caps.reflink = try_reflink(&src, &rl);

        let sl = probe.join("sl");
        let _ = fs::remove_file(&sl);
        caps.symlink = make_symlink(&src, &sl).is_ok();

        let _ = fs::remove_dir_all(&probe);
        caps
    }
}

#[cfg(unix)]
fn try_reflink(src: &Path, dst: &Path) -> bool {
    // Best-effort: clonefile on macOS via libc isn't exposed in std; Linux
    // FICLONE needs ioctl. For phase 1a we report reflink=false unless we
    // successfully copy via a platform-specific path. Keep probe honest —
    // false means SnapshotMaterializer skips tier 2.
    let _ = (src, dst);
    false
}

#[cfg(windows)]
fn try_reflink(src: &Path, dst: &Path) -> bool {
    let _ = (src, dst);
    false
}

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn make_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

impl Storage for FsStorage {
    fn create_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        fs::create_dir_all(self.abs(path)).map_err(Self::map_io)
    }

    fn remove_file(&self, path: &StoragePath) -> Result<(), StorageError> {
        fs::remove_file(self.abs(path)).map_err(Self::map_io)
    }

    fn remove_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        fs::remove_dir_all(self.abs(path)).map_err(Self::map_io)
    }

    fn rename(&self, from: &StoragePath, to: &StoragePath) -> Result<(), StorageError> {
        if let Some(parent) = to.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        fs::rename(self.abs(from), self.abs(to)).map_err(Self::map_io)
    }

    fn write_append(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        let mut sink = self.open_append(path)?;
        sink.write_all(bytes)
    }

    fn open_append(&self, path: &StoragePath) -> Result<Box<dyn AppendSink>, StorageError> {
        if let Some(parent) = path.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.abs(path))
            .map_err(Self::map_io)?;
        Ok(Box::new(FsAppend(f)))
    }

    fn write_file(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        if let Some(parent) = path.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        fs::write(self.abs(path), bytes).map_err(Self::map_io)
    }

    fn create_file_exclusive(
        &self,
        path: &StoragePath,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        if let Some(parent) = path.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        let abs = self.abs(path);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&abs)
            .map_err(Self::map_io)?;
        let write_err = {
            #[cfg(test)]
            {
                if FAIL_NEXT_EXCLUSIVE_WRITE.with(|c| c.replace(false)) {
                    Some(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "injected exclusive write failure",
                    ))
                } else {
                    f.write_all(bytes).err()
                }
            }
            #[cfg(not(test))]
            {
                f.write_all(bytes).err()
            }
        };
        if let Some(e) = write_err {
            // Don't leave an empty exclusive marker that Session/recovery would
            // treat as a claimed generation / mid-launch peer.
            let _ = fs::remove_file(&abs);
            return Err(Self::map_io(e));
        }
        Ok(())
    }

    fn read_file(&self, path: &StoragePath) -> Result<Vec<u8>, StorageError> {
        let mut f = File::open(self.abs(path)).map_err(Self::map_io)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(Self::map_io)?;
        Ok(buf)
    }

    fn metadata(&self, path: &StoragePath) -> Result<StorageFileMeta, StorageError> {
        let meta = fs::metadata(self.abs(path)).map_err(Self::map_io)?;
        let modified_unix_ms = meta.modified().ok().and_then(|t| {
            t.duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        });
        Ok(StorageFileMeta {
            len: meta.len(),
            is_file: meta.is_file(),
            modified_unix_ms,
        })
    }

    fn read_dir(&self, path: &StoragePath) -> Result<Vec<String>, StorageError> {
        let abs = if path.as_path().as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.abs(path)
        };
        let rd = fs::read_dir(abs).map_err(Self::map_io)?;
        let mut out = Vec::new();
        for ent in rd {
            let ent = ent.map_err(Self::map_io)?;
            out.push(ent.file_name().to_string_lossy().into_owned());
        }
        Ok(out)
    }

    fn probe_caps(&self) -> StorageCaps {
        *self.caps.get_or_init(|| Self::probe_once(&self.root))
    }

    fn hard_link(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        if let Some(parent) = dst.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        fs::hard_link(self.abs(src), self.abs(dst)).map_err(Self::map_io)
    }

    fn reflink_or_clone(&self, _src: &StoragePath, _dst: &StoragePath) -> Result<(), StorageError> {
        Err(StorageError::Unsupported)
    }

    fn symlink(&self, target: &StoragePath, link: &StoragePath) -> Result<(), StorageError> {
        if let Some(parent) = link.as_path().parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(self.root.join(parent)).map_err(Self::map_io)?;
            }
        }
        // Store relative target from link's parent when possible for portability.
        make_symlink(&self.abs(target), &self.abs(link)).map_err(Self::map_io)
    }

    fn exists(&self, path: &StoragePath) -> bool {
        self.abs(path).exists()
    }
}

struct FsAppend(File);

impl AppendSink for FsAppend {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.0.write_all(bytes).map_err(FsStorage::map_io)
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        self.0.flush().map_err(FsStorage::map_io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bugsee_platform::Storage;

    #[test]
    fn exclusive_create_removes_file_when_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStorage::new(dir.path()).unwrap();
        let path = StoragePath::new("sessions/1.alive");
        fail_next_exclusive_write();
        let err = store
            .create_file_exclusive(&path, b"pid")
            .expect_err("injected write failure");
        assert!(matches!(
            err,
            StorageError::Other(_) | StorageError::Full | StorageError::Permission
        ));
        assert!(
            !store.exists(&path),
            "empty exclusive marker must not survive a failed write"
        );
    }
}

//! Paths relative to an SDK data root.

use std::path::{Component, Path, PathBuf};

/// A path under the storage root. Rejects absolute paths and `..` escapes so
/// console/host backends never receive traversal outside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StoragePath {
    inner: PathBuf,
}

impl StoragePath {
    /// Build from a relative path. Returns `None` if absolute or contains `..`.
    pub fn try_new(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref();
        if path.is_absolute() {
            return None;
        }
        for c in path.components() {
            match c {
                Component::Normal(_) | Component::CurDir => {}
                Component::RootDir | Component::Prefix(_) | Component::ParentDir => {
                    return None;
                }
            }
        }
        Some(StoragePath {
            inner: path.to_path_buf(),
        })
    }

    /// Like [`try_new`] but panics on invalid input (test helpers).
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self::try_new(path).expect("StoragePath must be relative without '..'")
    }

    pub fn as_path(&self) -> &Path {
        &self.inner
    }

    pub fn join(&self, child: impl AsRef<Path>) -> Option<Self> {
        let mut p = self.inner.clone();
        p.push(child);
        Self::try_new(p)
    }

    pub fn parent(&self) -> Option<Self> {
        self.inner.parent().and_then(Self::try_new)
    }
}

impl AsRef<Path> for StoragePath {
    fn as_ref(&self) -> &Path {
        &self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_absolute_and_parent() {
        assert!(StoragePath::try_new("/etc/passwd").is_none());
        assert!(StoragePath::try_new("../escape").is_none());
        assert!(StoragePath::try_new("a/../../b").is_none());
        assert!(StoragePath::try_new("ok/nested").is_some());
    }
}

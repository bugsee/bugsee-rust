//! In-memory [`Storage`] for contract tests and pure unit tests.
//!
//! Simulates hardlinks via shared refcounted blobs. Reflink matches hardlink.
//! Symlink stores a path alias resolved on read.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::clock::{Clock, SystemClock};
use crate::error::StorageError;
use crate::path::StoragePath;
use crate::storage::{AppendSink, Storage, StorageCaps, StorageFileMeta};

#[derive(Debug, Default)]
struct State {
    /// Directory markers (path → ()).
    dirs: HashMap<String, ()>,
    /// File path → shared blob.
    files: HashMap<String, Arc<Mutex<Vec<u8>>>>,
    /// Symlink path → target path string.
    symlinks: HashMap<String, String>,
    /// File path → last modified unix ms.
    mtimes: HashMap<String, i64>,
}

/// Process-local durable store (lost when dropped).
#[derive(Debug, Clone, Default)]
pub struct MemoryStorage {
    state: Arc<Mutex<State>>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                dirs: HashMap::from([("".into(), ())]),
                files: HashMap::new(),
                symlinks: HashMap::new(),
                mtimes: HashMap::new(),
            })),
        }
    }

    fn key(path: &StoragePath) -> String {
        path.as_path()
            .to_string_lossy()
            .trim_start_matches("./")
            .replace('\\', "/")
    }

    fn now_ms() -> i64 {
        SystemClock.unix_time_ms()
    }

    fn touch(state: &mut State, key: &str) {
        state.mtimes.insert(key.to_string(), Self::now_ms());
    }

    fn ensure_parents(state: &mut State, path: &StoragePath) -> Result<(), StorageError> {
        let parent = match path.as_path().parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => return Ok(()),
        };
        let mut acc = PathAccum::default();
        for c in parent.components() {
            if let std::path::Component::Normal(s) = c {
                acc.push(s.to_string_lossy().as_ref());
                let k = acc.as_str();
                if state.files.contains_key(&k) || state.symlinks.contains_key(&k) {
                    return Err(StorageError::Other(format!("parent {k} is a file")));
                }
                state.dirs.insert(k, ());
            }
        }
        Ok(())
    }

    fn resolve_file_arc(
        state: &State,
        path: &StoragePath,
    ) -> Result<Arc<Mutex<Vec<u8>>>, StorageError> {
        let mut key = Self::key(path);
        if let Some(target) = state.symlinks.get(&key) {
            key = target.clone();
        }
        state
            .files
            .get(&key)
            .cloned()
            .ok_or(StorageError::NotFound)
    }
}

#[derive(Default)]
struct PathAccum {
    parts: Vec<String>,
}

impl PathAccum {
    fn push(&mut self, s: &str) {
        self.parts.push(s.to_string());
    }
    fn as_str(&self) -> String {
        self.parts.join("/")
    }
}

struct MemoryAppend {
    state: Arc<Mutex<State>>,
    key: String,
    blob: Arc<Mutex<Vec<u8>>>,
}

impl AppendSink for MemoryAppend {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.blob.lock().expect("blob").extend_from_slice(bytes);
        let mut state = self.state.lock().expect("memory storage lock");
        MemoryStorage::touch(&mut state, &self.key);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), StorageError> {
        Ok(())
    }
}

impl Storage for MemoryStorage {
    fn create_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let key = Self::key(path);
        if key.is_empty() {
            return Ok(());
        }
        if state.files.contains_key(&key) || state.symlinks.contains_key(&key) {
            return Err(StorageError::Other("path is a file".into()));
        }
        Self::ensure_parents(&mut state, path)?;
        state.dirs.insert(key, ());
        Ok(())
    }

    fn remove_file(&self, path: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let key = Self::key(path);
        if state.symlinks.remove(&key).is_some() {
            state.mtimes.remove(&key);
            return Ok(());
        }
        if state.files.remove(&key).is_some() {
            state.mtimes.remove(&key);
            return Ok(());
        }
        Err(StorageError::NotFound)
    }

    fn remove_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let prefix = Self::key(path);
        let dir_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        if !prefix.is_empty() && !state.dirs.contains_key(&prefix) {
            return Err(StorageError::NotFound);
        }
        let under = |k: &str| {
            if prefix.is_empty() {
                true
            } else {
                k == prefix || k.starts_with(&dir_prefix)
            }
        };
        state.files.retain(|k, _| !under(k));
        state.symlinks.retain(|k, _| !under(k));
        state.mtimes.retain(|k, _| !under(k));
        state.dirs.retain(|k, _| {
            if prefix.is_empty() {
                k.is_empty()
            } else {
                !(k == &prefix || k.starts_with(&dir_prefix))
            }
        });
        Ok(())
    }

    fn rename(&self, from: &StoragePath, to: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let from_k = Self::key(from);
        let to_k = Self::key(to);
        Self::ensure_parents(&mut state, to)?;
        if let Some(blob) = state.files.remove(&from_k) {
            let mt = state.mtimes.remove(&from_k).unwrap_or_else(Self::now_ms);
            state.files.insert(to_k.clone(), blob);
            state.mtimes.insert(to_k, mt);
            return Ok(());
        }
        if let Some(target) = state.symlinks.remove(&from_k) {
            state.mtimes.remove(&from_k);
            state.symlinks.insert(to_k, target);
            return Ok(());
        }
        Err(StorageError::NotFound)
    }

    fn write_append(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        let mut sink = self.open_append(path)?;
        sink.write_all(bytes)
    }

    fn open_append(&self, path: &StoragePath) -> Result<Box<dyn AppendSink>, StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        Self::ensure_parents(&mut state, path)?;
        let key = Self::key(path);
        let blob = state
            .files
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .clone();
        Self::touch(&mut state, &key);
        Ok(Box::new(MemoryAppend {
            state: Arc::clone(&self.state),
            key,
            blob,
        }))
    }

    fn write_file(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        Self::ensure_parents(&mut state, path)?;
        let key = Self::key(path);
        if let Some(existing) = state.files.get(&key).cloned() {
            // Preserve hardlink sharing: overwrite the shared blob in place.
            *existing.lock().expect("blob") = bytes.to_vec();
            Self::touch(&mut state, &key);
            return Ok(());
        }
        state
            .files
            .insert(key.clone(), Arc::new(Mutex::new(bytes.to_vec())));
        Self::touch(&mut state, &key);
        Ok(())
    }

    fn create_file_exclusive(
        &self,
        path: &StoragePath,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        Self::ensure_parents(&mut state, path)?;
        let key = Self::key(path);
        if state.files.contains_key(&key)
            || state.symlinks.contains_key(&key)
            || state.dirs.contains_key(&key)
        {
            return Err(StorageError::AlreadyExists);
        }
        state
            .files
            .insert(key.clone(), Arc::new(Mutex::new(bytes.to_vec())));
        Self::touch(&mut state, &key);
        Ok(())
    }

    fn read_file(&self, path: &StoragePath) -> Result<Vec<u8>, StorageError> {
        let state = self.state.lock().expect("memory storage lock");
        let arc = Self::resolve_file_arc(&state, path)?;
        drop(state);
        let bytes = arc.lock().expect("blob").clone();
        Ok(bytes)
    }

    fn metadata(&self, path: &StoragePath) -> Result<StorageFileMeta, StorageError> {
        let state = self.state.lock().expect("memory storage lock");
        let key = Self::key(path);
        if state.dirs.contains_key(&key) {
            return Ok(StorageFileMeta {
                len: 0,
                is_file: false,
                modified_unix_ms: None,
            });
        }
        let resolved_key = if let Some(t) = state.symlinks.get(&key) {
            t.clone()
        } else {
            key
        };
        let arc = state
            .files
            .get(&resolved_key)
            .cloned()
            .ok_or(StorageError::NotFound)?;
        let mt = state.mtimes.get(&resolved_key).copied();
        drop(state);
        let len = arc.lock().expect("blob").len() as u64;
        Ok(StorageFileMeta {
            len,
            is_file: true,
            modified_unix_ms: mt,
        })
    }

    fn read_dir(&self, path: &StoragePath) -> Result<Vec<String>, StorageError> {
        let state = self.state.lock().expect("memory storage lock");
        let prefix = Self::key(path);
        if !prefix.is_empty() && !state.dirs.contains_key(&prefix) {
            return Err(StorageError::NotFound);
        }
        let dir_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut names = std::collections::BTreeSet::new();
        let consider = |k: &str, names: &mut std::collections::BTreeSet<String>| {
            let rest = if prefix.is_empty() {
                k
            } else if let Some(r) = k.strip_prefix(&dir_prefix) {
                r
            } else {
                return;
            };
            if rest.is_empty() {
                return;
            }
            let name = rest.split('/').next().unwrap_or(rest);
            names.insert(name.to_string());
        };
        for k in state.dirs.keys() {
            consider(k, &mut names);
        }
        for k in state.files.keys() {
            consider(k, &mut names);
        }
        for k in state.symlinks.keys() {
            consider(k, &mut names);
        }
        Ok(names.into_iter().collect())
    }

    fn probe_caps(&self) -> StorageCaps {
        StorageCaps {
            hardlink: true,
            reflink: true,
            symlink: true,
        }
    }

    fn hard_link(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let src_arc = Self::resolve_file_arc(&state, src)?;
        Self::ensure_parents(&mut state, dst)?;
        let dst_k = Self::key(dst);
        if state.files.contains_key(&dst_k) || state.symlinks.contains_key(&dst_k) {
            return Err(StorageError::AlreadyExists);
        }
        state.files.insert(dst_k.clone(), src_arc);
        let src_k = Self::key(src);
        let mt = state
            .mtimes
            .get(&src_k)
            .copied()
            .unwrap_or_else(Self::now_ms);
        state.mtimes.insert(dst_k, mt);
        Ok(())
    }

    fn reflink_or_clone(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        self.hard_link(src, dst)
    }

    fn symlink(&self, target: &StoragePath, link: &StoragePath) -> Result<(), StorageError> {
        let mut state = self.state.lock().expect("memory storage lock");
        let _ = Self::resolve_file_arc(&state, target)?;
        Self::ensure_parents(&mut state, link)?;
        let link_k = Self::key(link);
        if state.files.contains_key(&link_k) || state.symlinks.contains_key(&link_k) {
            return Err(StorageError::AlreadyExists);
        }
        state.symlinks.insert(link_k, Self::key(target));
        Ok(())
    }

    fn exists(&self, path: &StoragePath) -> bool {
        let state = self.state.lock().expect("memory storage lock");
        let key = Self::key(path);
        state.dirs.contains_key(&key)
            || state.files.contains_key(&key)
            || state.symlinks.contains_key(&key)
    }
}

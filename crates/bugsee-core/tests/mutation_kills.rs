//! Kill tests for the Phase 1a mutation campaign.
//!
//! **Mutator pattern (process):**
//! 1. Keep these tests green against the pure implementation.
//! 2. Mutate the targeted production code (subtle defect).
//! 3. Re-run this suite (+ platform storage contracts / desktop lib tests).
//! 4. If the suite stays green the mutant *escaped* → strengthen tests here.
//! 5. Confirm the mutant is now killed, then restore production code to pure.
//! 6. Cap: 10 mutants per campaign. Always leave production code pure.
//!
//! Latest campaign (10/10 killed, 0 escapes): hardlink `write_file`, batched
//! blacklist append, exclusive-create rollback, `StoragePath` `..`, exclusive
//! payload drop, hardlink copy-not-share, append truncate, missing mtime,
//! rename-without-remove, util ignoring installed clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bugsee_platform::memory::MemoryStorage;
use bugsee_platform::{
    AppendSink, Clock, Entropy, Storage, StorageCaps, StorageError, StorageFileMeta, StoragePath,
    SystemClock, SystemEntropy,
};

fn p(s: &str) -> StoragePath {
    StoragePath::new(s)
}

/// Counts `write_append` calls; forwards everything else to an inner store.
struct CountingAppendStorage {
    inner: MemoryStorage,
    appends: AtomicUsize,
}

impl CountingAppendStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            appends: AtomicUsize::new(0),
        }
    }
}

impl Storage for CountingAppendStorage {
    fn create_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        self.inner.create_dir_all(path)
    }
    fn remove_file(&self, path: &StoragePath) -> Result<(), StorageError> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &StoragePath) -> Result<(), StorageError> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &StoragePath, to: &StoragePath) -> Result<(), StorageError> {
        self.inner.rename(from, to)
    }
    fn write_append(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        self.inner.write_append(path, bytes)
    }
    fn open_append(&self, path: &StoragePath) -> Result<Box<dyn AppendSink>, StorageError> {
        self.inner.open_append(path)
    }
    fn write_file(&self, path: &StoragePath, bytes: &[u8]) -> Result<(), StorageError> {
        self.inner.write_file(path, bytes)
    }
    fn create_file_exclusive(
        &self,
        path: &StoragePath,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        self.inner.create_file_exclusive(path, bytes)
    }
    fn read_file(&self, path: &StoragePath) -> Result<Vec<u8>, StorageError> {
        self.inner.read_file(path)
    }
    fn metadata(&self, path: &StoragePath) -> Result<StorageFileMeta, StorageError> {
        self.inner.metadata(path)
    }
    fn read_dir(&self, path: &StoragePath) -> Result<Vec<String>, StorageError> {
        self.inner.read_dir(path)
    }
    fn probe_caps(&self) -> StorageCaps {
        self.inner.probe_caps()
    }
    fn hard_link(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        self.inner.hard_link(src, dst)
    }
    fn reflink_or_clone(&self, src: &StoragePath, dst: &StoragePath) -> Result<(), StorageError> {
        self.inner.reflink_or_clone(src, dst)
    }
    fn symlink(&self, target: &StoragePath, link: &StoragePath) -> Result<(), StorageError> {
        self.inner.symlink(target, link)
    }
    fn exists(&self, path: &StoragePath) -> bool {
        self.inner.exists(path)
    }
}

/// Mutant kill: batched blacklist append must not collapse to one write_append.
#[test]
fn blacklist_add_issues_one_append_per_fresh_signature() {
    let store = CountingAppendStorage::new();
    let sigs = vec!["a".into(), "b".into(), "c".into()];
    bugsee_core::queue::blacklist_add(&store, &sigs);
    assert_eq!(
        store.appends.load(Ordering::SeqCst),
        3,
        "each fresh signature must be its own write_append (PIPE_BUF / shared-dir peers)"
    );
    assert!(bugsee_core::queue::any_blacklisted(&store, &sigs));
    // Re-add: no fresh → no further appends.
    bugsee_core::queue::blacklist_add(&store, &sigs);
    assert_eq!(store.appends.load(Ordering::SeqCst), 3);
}

struct FixedClock(i64);
impl Clock for FixedClock {
    fn unix_time_ms(&self) -> i64 {
        self.0
    }
    fn mono_time_ms(&self) -> i64 {
        self.0
    }
}

struct ZeroEntropy;
impl Entropy for ZeroEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), bugsee_platform::EntropyError> {
        buf.fill(0);
        Ok(())
    }
}

/// Mutant kill: util must honor installed Clock/Entropy, not hard-coded OS sources.
#[test]
fn util_honors_installed_clock_and_entropy() {
    bugsee_core::platform_services::install(Arc::new(FixedClock(1_700_000_000_000)), Arc::new(ZeroEntropy));
    assert_eq!(bugsee_core::util::epoch_ms(), 1_700_000_000_000);
    let hex = bugsee_core::util::random_hex(4);
    assert_eq!(hex, "00000000");
    // Restore defaults so other tests in this process are not poisoned.
    bugsee_core::platform_services::install(Arc::new(SystemClock), Arc::new(SystemEntropy));
}

/// Mutant kill: exclusive create must persist the provided bytes (not empty/wrong).
#[test]
fn exclusive_create_persists_payload() {
    let store = MemoryStorage::new();
    store
        .create_file_exclusive(&p("sessions/1.alive"), b"4242")
        .unwrap();
    assert_eq!(store.read_file(&p("sessions/1.alive")).unwrap(), b"4242");
}

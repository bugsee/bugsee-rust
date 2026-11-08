//
//  scope.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Report-enriching scope: user identity, labels, and report-level attributes
//! that apply to any report created while they are set.

use std::path::Path;

use serde_json::{Map, Value};

/// Ambient enrichment applied to reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    /// User identifier → `request.json.email`.
    pub email: Option<String>,
    /// Issue labels → `request.json.labels`.
    pub labels: Vec<String>,
    /// Report-level attributes → `manifest.json.attrs`.
    pub attributes: Map<String, Value>,
    /// Additional structured context.
    pub contexts: Map<String, Value>,
}

/// A scope written to a temp file, awaiting its rename (see [`Scope::stage`]).
#[derive(Debug)]
pub enum Staged {
    /// The scope is empty: committing removes the file.
    Remove,
    /// The temp file holding the new scope.
    Ready(std::path::PathBuf),
    /// The new scope can never be persisted (it does not serialise or exceeds
    /// [`MAX_PERSISTED_BYTES`]): committing removes the stale file, and retrying
    /// the same scope is pointless.
    TooLarge,
    /// The new scope could not be staged this time (e.g. disk full, directory
    /// gone): committing removes the stale file, and a retry may succeed.
    Failed,
}

impl Staged {
    /// Make the staged scope the persisted one.
    pub fn commit(self, path: &Path) -> std::io::Result<()> {
        let remove = || match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        match self {
            Staged::Remove => remove(),
            Staged::Ready(tmp) => std::fs::rename(&tmp, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
                let _ = remove();
            }),
            Staged::TooLarge | Staged::Failed => {
                let _ = remove();
                Err(std::io::Error::other("scope could not be persisted"))
            }
        }
    }

    /// Drop a staged scope that a newer one superseded.
    pub fn discard(self) {
        if let Staged::Ready(tmp) = self {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

/// File name of the persisted scope inside a session's parts directory.
pub const SCOPE_FILE: &str = "scope.json";
/// Largest persisted scope that recovery will read.
const MAX_PERSISTED_BYTES: u64 = 1 << 20;

impl Scope {
    /// True when nothing is set (email, labels, attributes).
    pub fn is_empty(&self) -> bool {
        self.email.is_none() && self.labels.is_empty() && self.attributes.is_empty()
    }

    /// Persist the identity, labels and attributes to `path` (temp file + rename,
    /// so a crash mid-write leaves the previous copy). An empty scope removes the
    /// file. Called on change, never on the crash path.
    ///
    /// Best effort at not leaving a stale copy: when the new scope cannot be
    /// written (or is larger than recovery will read), the old file is removed, so
    /// a crash reports no identity rather than a previous one.
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        self.stage(path, 0).commit(path)
    }

    /// First half of [`Scope::persist`]: do the slow part (serialise, write a
    /// temp file named by `tag`) so a caller can take a lock for only the
    /// [`Staged::commit`].
    pub fn stage(&self, path: &Path, tag: u64) -> Staged {
        if self.is_empty() {
            return Staged::Remove;
        }
        let Ok(bytes) = serde_json::to_vec(&serde_json::json!({
            "email": self.email,
            "labels": self.labels,
            "attributes": self.attributes,
        })) else {
            return Staged::TooLarge;
        };
        if bytes.len() as u64 > MAX_PERSISTED_BYTES {
            return Staged::TooLarge;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".{tag}.tmp"));
        let tmp = std::path::PathBuf::from(tmp);
        match std::fs::write(&tmp, bytes) {
            Ok(()) => Staged::Ready(tmp),
            Err(_) => {
                let _ = std::fs::remove_file(&tmp);
                Staged::Failed
            }
        }
    }

    /// Read the freshest usable scope a crashed session left at `path`.
    ///
    /// A write is a temp file (`<name>.<version>.tmp`) renamed over `path`, so a
    /// session that died between the two leaves the newest scope in the temp
    /// file and one change older in `path`. Candidates are tried newest first
    /// (highest version, then `path` itself); the first that parses within the
    /// size cap wins. A temp file torn by a crash mid-write fails to parse and
    /// the next older candidate is used. `None` when nothing is usable.
    pub fn load_recovering(path: &Path) -> Option<Scope> {
        let mut temps = Self::temp_files(path);
        temps.sort_by_key(|t| std::cmp::Reverse(t.0));
        temps
            .into_iter()
            .map(|(_, p)| p)
            .chain(std::iter::once(path.to_path_buf()))
            .find_map(|p| Self::load(&p))
    }

    /// Temp files `stage` may have left next to `path`, with their versions.
    fn temp_files(path: &Path) -> Vec<(u64, std::path::PathBuf)> {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
        else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let file = e.file_name().into_string().ok()?;
                let version = file
                    .strip_prefix(name)?
                    .strip_prefix('.')?
                    .strip_suffix(".tmp")?
                    .parse::<u64>()
                    .ok()?;
                Some((version, e.path()))
            })
            .collect()
    }

    /// Read a scope written by [`Scope::persist`]. `None` when absent, oversized
    /// or unparseable (a torn or foreign file is ignored, never fatal).
    pub fn load(path: &Path) -> Option<Scope> {
        if std::fs::metadata(path).ok()?.len() > MAX_PERSISTED_BYTES {
            return None;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let obj = value.as_object()?;
        Some(Scope {
            email: obj.get("email").and_then(Value::as_str).map(str::to_owned),
            labels: obj
                .get("labels")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            attributes: obj
                .get("attributes")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            contexts: Map::new(),
        })
    }

    /// Set (or clear, with `None`) the user identifier.
    pub fn set_email(&mut self, email: Option<String>) {
        self.email = email;
    }

    /// Set a report-level attribute.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), value);
    }

    /// Remove a report-level attribute.
    pub fn clear_attribute(&mut self, key: &str) {
        self.attributes.remove(key);
    }

    /// Remove all report-level attributes.
    pub fn clear_all_attributes(&mut self) {
        self.attributes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("bugsee-scope-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        p.join(SCOPE_FILE)
    }

    #[test]
    fn persist_then_load_round_trips() {
        let path = tmp();
        let mut s = Scope::default();
        s.set_email(Some("a@b.c".into()));
        s.labels.push("beta".into());
        s.set_attribute("k", Value::from(1));
        s.persist(&path).unwrap();
        let back = Scope::load(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("a@b.c"));
        assert_eq!(back.labels, vec!["beta".to_string()]);
        assert_eq!(back.attributes.get("k"), Some(&Value::from(1)));
    }

    #[test]
    fn an_oversized_or_unwritable_scope_leaves_no_stale_file() {
        let path = tmp();
        let mut s = Scope::default();
        s.set_email(Some("old@b.c".into()));
        s.persist(&path).unwrap();
        s.set_attribute("big", Value::from("x".repeat(MAX_PERSISTED_BYTES as usize)));
        assert!(s.persist(&path).is_err());
        assert!(!path.exists(), "the previous scope must not survive");
        // Unwritable target (missing directory): error, nothing left behind.
        let missing = path.with_file_name("nope").join(SCOPE_FILE);
        let mut small = Scope::default();
        small.set_email(Some("a@b.c".into()));
        assert!(small.persist(&missing).is_err());
        assert!(!missing.exists());
    }

    fn scope_with_email(email: &str) -> Scope {
        let mut s = Scope::default();
        s.set_email(Some(email.into()));
        s
    }

    /// The on-disk state of a process that died after `stage` wrote the temp
    /// file and before `commit` renamed it: `Staged` has no `Drop`, so letting it
    /// go leaves the temp file exactly as a crash would.
    fn die_before_rename(scope: &Scope, path: &Path, tag: u64) -> std::path::PathBuf {
        match scope.stage(path, tag) {
            Staged::Ready(tmp) => tmp,
            other => panic!("expected a staged temp file, got {other:?}"),
        }
    }

    #[test]
    fn staging_tells_a_hopeless_scope_from_a_transient_failure() {
        let path = tmp();
        let mut big = scope_with_email("a@b.c");
        big.set_attribute("big", Value::from("x".repeat(MAX_PERSISTED_BYTES as usize)));
        assert!(matches!(big.stage(&path, 1), Staged::TooLarge));
        // A directory that is gone is worth retrying.
        let missing = path.with_file_name("nope").join(SCOPE_FILE);
        assert!(matches!(
            scope_with_email("a@b.c").stage(&missing, 1),
            Staged::Failed
        ));
        assert!(matches!(Scope::default().stage(&path, 1), Staged::Remove));
    }

    #[test]
    fn a_crash_between_write_and_rename_recovers_the_newer_scope() {
        let path = tmp();
        scope_with_email("old@b.c").persist(&path).unwrap();
        let tmp_file = die_before_rename(&scope_with_email("new@b.c"), &path, 7);
        assert!(tmp_file.exists());
        // The renamed file is one change behind; the temp file holds the newest.
        assert_eq!(
            Scope::load(&path).unwrap().email.as_deref(),
            Some("old@b.c")
        );
        let back = Scope::load_recovering(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("new@b.c"));
    }

    #[test]
    fn a_crash_between_write_and_rename_recovers_when_no_file_was_renamed_yet() {
        // First ever scope change of the session: nothing to fall back to.
        let path = tmp();
        die_before_rename(&scope_with_email("first@b.c"), &path, 1);
        assert!(!path.exists());
        let back = Scope::load_recovering(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("first@b.c"));
    }

    #[test]
    fn a_crash_mid_write_falls_back_to_the_last_complete_scope() {
        let path = tmp();
        scope_with_email("old@b.c").persist(&path).unwrap();
        // The process died while `write` was filling the temp file.
        let tmp_file = die_before_rename(&scope_with_email("new@b.c"), &path, 7);
        let full = std::fs::read(&tmp_file).unwrap();
        std::fs::write(&tmp_file, &full[..full.len() / 2]).unwrap();
        let back = Scope::load_recovering(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("old@b.c"));
        // Same when the temp file is empty (power loss after an unsynced write).
        std::fs::write(&tmp_file, b"").unwrap();
        assert_eq!(
            Scope::load_recovering(&path).unwrap().email.as_deref(),
            Some("old@b.c")
        );
    }

    #[test]
    fn a_torn_temp_file_with_nothing_to_fall_back_to_recovers_no_scope() {
        let path = tmp();
        let tmp_file = die_before_rename(&scope_with_email("new@b.c"), &path, 3);
        let full = std::fs::read(&tmp_file).unwrap();
        std::fs::write(&tmp_file, &full[..full.len() - 4]).unwrap();
        assert!(Scope::load_recovering(&path).is_none());
    }

    #[test]
    fn the_newest_valid_temp_file_wins_and_unrelated_files_are_ignored() {
        let path = tmp();
        scope_with_email("v0@b.c").persist(&path).unwrap();
        die_before_rename(&scope_with_email("v2@b.c"), &path, 2);
        die_before_rename(&scope_with_email("v10@b.c"), &path, 10); // 10 > 2 numerically
        let torn = die_before_rename(&scope_with_email("v11@b.c"), &path, 11);
        std::fs::write(&torn, b"{\"email\":\"v11").unwrap();
        let dir = path.parent().unwrap();
        std::fs::write(dir.join("scope.json.bak"), b"{\"email\":\"x@b.c\"}").unwrap();
        std::fs::write(dir.join("other.json.99.tmp"), b"{\"email\":\"y@b.c\"}").unwrap();
        let back = Scope::load_recovering(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("v10@b.c"));
    }

    #[test]
    fn empty_scope_removes_the_file_and_garbage_is_ignored() {
        let path = tmp();
        let mut s = Scope::default();
        s.set_email(Some("a@b.c".into()));
        s.persist(&path).unwrap();
        Scope::default().persist(&path).unwrap();
        assert!(!path.exists());
        Scope::default().persist(&path).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(Scope::load(&path).is_none());
    }
}

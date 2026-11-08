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
    /// [`MAX_PERSISTED_BYTES`]): retrying the same scope is pointless.
    TooLarge,
    /// The new scope could not be staged this time (e.g. disk full, directory
    /// gone): a retry may succeed.
    Failed,
}

impl Staged {
    /// Make the staged scope the persisted one. Cheap enough to run under an
    /// ordering lock (a rename); follow it with [`Scope::sweep_older`] outside
    /// the lock so a crash cannot find a temp file that this commit superseded.
    ///
    /// When the new scope cannot be persisted the previous file is removed, so a
    /// crash reports no identity rather than a stale one, unless `keep_stale`:
    /// the caller knows the old file is merely missing things the new scope has
    /// (see [`Scope::extends`]), so nothing in it is wrong.
    pub fn commit(self, path: &Path, _version: u64, keep_stale: bool) -> std::io::Result<()> {
        let remove = || match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        match self {
            Staged::Remove => remove(),
            // A failed rename keeps the temp file: it is a complete snapshot of the
            // newest scope, which `load_recovering` prefers over a stale
            // `scope.json`. A later commit's `sweep_older` removes it.
            Staged::Ready(tmp) => std::fs::rename(&tmp, path).inspect_err(|_| {
                if !keep_stale {
                    let _ = remove();
                }
            }),
            Staged::TooLarge | Staged::Failed => {
                if !keep_stale {
                    let _ = remove();
                }
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
    /// True when `self` only adds to `old`: same email (or `old` had none), and
    /// every label and attribute of `old` is still there with the same value. An
    /// older file of such a scope is merely missing things, never wrong.
    pub fn extends(&self, old: &Scope) -> bool {
        (old.email.is_none() || old.email == self.email)
            && old.labels.iter().all(|l| self.labels.contains(l))
            && old
                .attributes
                .iter()
                .all(|(k, v)| self.attributes.get(k) == Some(v))
    }

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
    /// a crash reports no identity rather than a previous one (see
    /// [`Staged::commit`] for the one case where it is kept).
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        self.stage(path, 0).commit(path, 0, false)
    }

    /// First half of [`Scope::persist`]: do the slow part (serialise, write a
    /// temp file named by `tag`) so a caller can take a lock for only the
    /// [`Staged::commit`].
    pub fn stage(&self, path: &Path, tag: u64) -> Staged {
        if self.is_empty() {
            return Staged::Remove;
        }
        let Ok(bytes) = serde_json::to_vec(&serde_json::json!({
            "v": tag,
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
    /// file and one change older in `path`. Every candidate carries a version
    /// (a temp file in its name, `path` in its `"v"` field; absent is 0) and the
    /// highest version that parses within the size cap wins, so a temp file left
    /// by a writer that lost the race never beats a newer committed scope. A temp
    /// file torn by a crash mid-write fails to parse and the next candidate is
    /// used. `None` when nothing is usable.
    pub fn load_recovering(path: &Path) -> Option<Scope> {
        let mut candidates: Vec<(u64, Scope)> = Self::temp_files(path)
            .into_iter()
            .filter_map(|(v, p)| Self::load(&p).map(|s| (v, s)))
            .collect();
        let committed = Self::load(path).map(|s| (Self::stored_version(path), s));
        candidates.extend(committed);
        // Highest version. `max_by_key` keeps the last of equal maxima, and the
        // committed file is last, so it wins a tie.
        candidates
            .into_iter()
            .max_by_key(|(v, _)| *v)
            .map(|(_, s)| s)
    }

    /// The `"v"` a committed scope file was staged under (0 if absent).
    fn stored_version(path: &Path) -> u64 {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v.get("v").and_then(Value::as_u64))
            .unwrap_or(0)
    }

    /// Remove the temp files of versions older than `version` (a directory scan:
    /// call without holding an ordering lock).
    pub fn sweep_older(path: &Path, version: u64) {
        for (v, p) in Self::temp_files(path) {
            if v < version {
                let _ = std::fs::remove_file(p);
            }
        }
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

    /// A scratch scope path whose directory is removed when dropped.
    struct TmpScope {
        dir: std::path::PathBuf,
        file: std::path::PathBuf,
    }

    impl Drop for TmpScope {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl AsRef<Path> for TmpScope {
        fn as_ref(&self) -> &Path {
            &self.file
        }
    }

    impl std::ops::Deref for TmpScope {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.file
        }
    }

    fn tmp() -> TmpScope {
        let dir = std::env::temp_dir().join(format!("bugsee-scope-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(SCOPE_FILE);
        TmpScope { dir, file }
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
    fn a_stale_lower_version_temp_file_never_beats_the_committed_scope() {
        // Thread B committed version 2; thread A's older version 1 was still
        // staged (it lost the version check) when the process died.
        let path = tmp();
        scope_with_email("b@x")
            .stage(&path, 2)
            .commit(&path, 2, false)
            .unwrap();
        die_before_rename(&scope_with_email("a@x"), &path, 1);
        let back = Scope::load_recovering(&path).unwrap();
        assert_eq!(back.email.as_deref(), Some("b@x"));
        // A genuinely newer temp file still wins over the committed one.
        die_before_rename(&scope_with_email("c@x"), &path, 3);
        assert_eq!(
            Scope::load_recovering(&path).unwrap().email.as_deref(),
            Some("c@x")
        );
    }

    #[test]
    fn a_stale_temp_file_after_a_removal_recovers_no_scope() {
        // Logout (version 2) removed scope.json; version 1's temp file survives.
        let path = tmp();
        die_before_rename(&scope_with_email("a@x"), &path, 1);
        Scope::default()
            .stage(&path, 2)
            .commit(&path, 2, false)
            .unwrap();
        Scope::sweep_older(&path, 2);
        assert!(Scope::load_recovering(&path).is_none());
    }

    #[test]
    fn a_sweep_removes_the_temp_files_of_older_versions() {
        let path = tmp();
        let older = die_before_rename(&scope_with_email("a@x"), &path, 1);
        let newer = die_before_rename(&scope_with_email("z@x"), &path, 9);
        scope_with_email("b@x")
            .stage(&path, 5)
            .commit(&path, 5, false)
            .unwrap();
        assert!(older.exists(), "commit itself does no directory scan");
        Scope::sweep_older(&path, 5);
        assert!(!older.exists(), "an older version's temp file is swept");
        assert!(newer.exists(), "a newer in-flight temp file is left alone");
        assert_eq!(Scope::load(&path).unwrap().email.as_deref(), Some("b@x"));
    }

    #[test]
    fn a_failed_write_keeps_the_old_file_only_when_asked_to() {
        let path = tmp();
        scope_with_email("a@x").persist(&path).unwrap();
        let mut big = scope_with_email("a@x");
        big.set_attribute("big", Value::from("x".repeat(MAX_PERSISTED_BYTES as usize)));
        // Same identity: the old file is still right about who the user is.
        assert!(big.stage(&path, 1).commit(&path, 1, true).is_err());
        assert_eq!(Scope::load(&path).unwrap().email.as_deref(), Some("a@x"));
        // Changed identity: never leave the previous one behind.
        assert!(big.stage(&path, 2).commit(&path, 2, false).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn on_equal_versions_the_committed_scope_wins() {
        // No `"v"` (read as 0) next to a `.0.tmp`: a tie.
        let path = tmp();
        std::fs::write(&path.file, br#"{"email":"committed@x"}"#).unwrap();
        std::fs::write(
            path.with_file_name("scope.json.0.tmp"),
            br#"{"email":"temp@x"}"#,
        )
        .unwrap();
        assert_eq!(
            Scope::load_recovering(&path).unwrap().email.as_deref(),
            Some("committed@x")
        );
    }

    #[test]
    fn extends_is_true_only_when_the_old_scope_is_merely_missing_things() {
        let mut old = Scope::default();
        old.set_email(Some("a@x".into()));
        old.labels.push("beta".into());
        old.set_attribute("plan", Value::from("pro"));
        let mut more = old.clone();
        more.labels.push("new".into());
        more.set_attribute("extra", Value::from(1));
        assert!(more.extends(&old), "pure additions");
        assert!(old.extends(&old), "unchanged");
        assert!(old.extends(&Scope::default()), "anything extends nothing");

        let mut changed_attr = old.clone();
        changed_attr.set_attribute("plan", Value::from("free"));
        assert!(
            !changed_attr.extends(&old),
            "a changed value is not an addition"
        );
        let mut removed_attr = old.clone();
        removed_attr.clear_attribute("plan");
        assert!(!removed_attr.extends(&old), "a removed attribute");
        let mut removed_label = old.clone();
        removed_label.labels.clear();
        assert!(!removed_label.extends(&old), "a removed label");
        let mut other_email = old.clone();
        other_email.set_email(Some("b@x".into()));
        assert!(!other_email.extends(&old), "a changed email");
        let mut no_email = old.clone();
        no_email.set_email(None);
        assert!(!no_email.extends(&old), "a cleared email");
        let mut gained_email = Scope::default();
        gained_email.set_email(Some("a@x".into()));
        assert!(gained_email.extends(&Scope::default()), "a gained email");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_rename_leaves_the_complete_temp_file_for_recovery() {
        // A directory where `scope.json` should go makes the rename fail (and the
        // old file impossible to remove), standing in for a dest held open.
        let path = tmp();
        std::fs::create_dir(&path.file).unwrap();
        let staged = scope_with_email("new@x").stage(&path, 5);
        let Staged::Ready(tmp_file) = &staged else {
            panic!("expected a staged temp file")
        };
        let tmp_file = tmp_file.clone();
        assert!(staged.commit(&path, 5, false).is_err());
        assert!(
            tmp_file.exists(),
            "the fully written scope is not thrown away"
        );
        assert_eq!(
            Scope::load_recovering(&path).unwrap().email.as_deref(),
            Some("new@x")
        );
        // A later, higher version sweeps it.
        Scope::sweep_older(&path, 6);
        assert!(!tmp_file.exists());
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

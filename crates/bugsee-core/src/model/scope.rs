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
#[derive(Debug, Clone, Default)]
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
    /// Never leaves a stale copy: when the new scope cannot be written (or is
    /// larger than recovery will read), the old file is removed, so a crash
    /// reports no identity rather than a previous one.
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        let remove = || match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        if self.is_empty() {
            return remove();
        }
        let written = self.write_file(path);
        if written.is_err() {
            let _ = remove();
        }
        written
    }

    fn write_file(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "email": self.email,
            "labels": self.labels,
            "attributes": self.attributes,
        }))
        .map_err(std::io::Error::other)?;
        if bytes.len() as u64 > MAX_PERSISTED_BYTES {
            return Err(std::io::Error::other("scope too large to persist"));
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let result = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
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

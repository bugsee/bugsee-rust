//
//  scope.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Report-enriching scope: user identity, labels, and report-level attributes
//! that apply to any report created while they are set.

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

impl Scope {
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

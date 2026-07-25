//
//  report.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The deferred manual report — the Rust rendering of mobile `createReport`.
//! Snapshot the window now (bounds pinned at creation), populate, then upload.

use bugsee_core::model::enums::{IssueType, Severity, TriggerType};
use bugsee_core::model::report::Source;
use bugsee_core::reporting::{Attachment, ReportMeta};
use bugsee_core::SnapshotHandle;
use serde_json::{Map, Value};

/// A report captured at a point in time, awaiting population and upload.
pub struct Report {
    pub(crate) meta: ReportMeta,
    /// The window snapshot taken at creation (hard-linked on disk).
    snapshot: Option<SnapshotHandle>,
    /// Fallback window end, used only when no snapshot could be taken.
    window_end: i64,
}

impl Report {
    pub(crate) fn new(snapshot: Option<SnapshotHandle>, window_end: i64) -> Self {
        Report {
            meta: ReportMeta {
                issue_type: IssueType::Bug,
                summary: None,
                description: None,
                labels: Vec::new(),
                severity: Severity::default(),
                email: None,
                signatures: Vec::new(),
                source: Source {
                    trigger: TriggerType::CodeUpload,
                    origin: None,
                },
                attrs: Map::new(),
                attachments: Vec::new(),
            },
            snapshot,
            window_end,
        }
    }

    /// Set the short title.
    pub fn set_summary(&mut self, summary: impl Into<String>) -> &mut Self {
        self.meta.summary = Some(summary.into());
        self
    }

    /// Set the long description.
    pub fn set_description(&mut self, description: impl Into<String>) -> &mut Self {
        self.meta.description = Some(description.into());
        self
    }

    /// Set the severity.
    pub fn set_severity(&mut self, severity: Severity) -> &mut Self {
        self.meta.severity = severity;
        self
    }

    /// Replace the issue labels.
    pub fn set_labels<I, S>(&mut self, labels: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.meta.labels = labels.into_iter().map(Into::into).collect();
        self
    }

    /// Append one label.
    pub fn add_label(&mut self, label: impl Into<String>) -> &mut Self {
        self.meta.labels.push(label.into());
        self
    }

    /// Set a report-level attribute.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<Value>) -> &mut Self {
        self.meta.attrs.insert(key.into(), value.into());
        self
    }

    /// Attach a file to the report.
    pub fn add_attachment(
        &mut self,
        data: Vec<u8>,
        name: impl Into<String>,
        mime: impl Into<String>,
    ) -> &mut Self {
        self.meta.attachments.push(Attachment {
            data,
            name: name.into(),
            mime: mime.into(),
        });
        self
    }

    /// Submit the report. Consumes the builder.
    pub fn upload(mut self) {
        let meta = self.meta.clone();
        match self.snapshot.take() {
            // Deliver from the snapshot taken at create time.
            Some(handle) => crate::api::submit_snapshot(handle, meta),
            // No snapshot was taken — fall back to pinning the window now.
            None => crate::api::submit_report(meta, self.window_end),
        }
    }

    /// Discard the report without uploading (releases the snapshot's links).
    pub fn discard(mut self) {
        if let Some(handle) = self.snapshot.take() {
            crate::api::discard_snapshot(handle);
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        // A report dropped without upload/discard must still release its
        // snapshot's on-disk hard links.
        if let Some(handle) = self.snapshot.take() {
            crate::api::discard_snapshot(handle);
        }
    }
}

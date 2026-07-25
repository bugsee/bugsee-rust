//
//  reporting.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Report assembly: turn a snapshot directory + issue metadata into a finished
//! report bundle (the ZIP) plus the `request.json` body used to create the
//! issue server-side.

use std::path::Path;

use serde_json::{Map, Value};

use crate::bundle::{bundle_filename, write_bundle, BundleEntry};
use crate::capture::export_report;
use crate::model::enums::{IssueType, Severity};
use crate::model::environment::Environment;
use crate::model::report::{FileDescriptor, IssueRequest, Manifest, Source, TimeWindow};
use crate::util::{iso8601_ms, random_hex};

/// A user attachment carried by a report.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub data: Vec<u8>,
    pub name: String,
    pub mime: String,
}

/// Issue metadata supplied by the trigger / `Report` builder.
#[derive(Debug, Clone)]
pub struct ReportMeta {
    pub issue_type: IssueType,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub labels: Vec<String>,
    pub severity: Severity,
    pub email: Option<String>,
    pub signatures: Vec<String>,
    pub source: Source,
    /// Report-level attributes → `manifest.attrs`.
    pub attrs: Map<String, Value>,
    /// User attachments → `<random>.attachment.bgsfile` + manifest descriptors.
    pub attachments: Vec<Attachment>,
}

/// An extra file to place in the bundle (e.g. a native minidump), with its
/// manifest `type`.
pub struct ExtraFile {
    pub filename: String,
    pub file_type: String,
    pub data: Vec<u8>,
}

/// A finished, ready-to-deliver report.
pub struct AssembledReport {
    /// `<random20>.bundle.zip` file name.
    pub bundle_name: String,
    /// The bundle ZIP bytes.
    pub zip: Vec<u8>,
    /// The `request.json` body (byte-identical to the issue-create request).
    pub request_json: Vec<u8>,
}

/// Assemble a report bundle from the snapshot at `report_dir` covering
/// `window`, plus optional pre-serialized `crash.json` bytes.
pub fn assemble(
    report_dir: &Path,
    window: TimeWindow,
    meta: &ReportMeta,
    env: &Environment,
    crash_json: Option<Vec<u8>>,
    app_token: &str,
    created_on_ms: i64,
) -> std::io::Result<AssembledReport> {
    assemble_with_extras(
        report_dir,
        window,
        meta,
        env,
        crash_json,
        &[],
        app_token,
        created_on_ms,
    )
}

/// Like [`assemble`] but also bundles `extra_files` (e.g. a native minidump).
#[allow(clippy::too_many_arguments)]
pub fn assemble_with_extras(
    report_dir: &Path,
    window: TimeWindow,
    meta: &ReportMeta,
    env: &Environment,
    crash_json: Option<Vec<u8>>,
    extra_files: &[ExtraFile],
    app_token: &str,
    created_on_ms: i64,
) -> std::io::Result<AssembledReport> {
    let docs = export_report(report_dir, window.start, window.end)?;

    let mut files: Vec<FileDescriptor> = Vec::new();
    let mut entries: Vec<BundleEntry> = Vec::new();

    for doc in docs {
        let filename = format!("{}.{}.json", random_hex(8), doc.channel);
        files.push(FileDescriptor::capture(&filename, &doc.channel));
        entries.push(BundleEntry::new(filename, doc.bytes));
    }

    if let Some(bytes) = crash_json {
        files.push(FileDescriptor::capture("crash.json", "crash"));
        entries.push(BundleEntry::new("crash.json", bytes));
    }

    for extra in extra_files {
        files.push(FileDescriptor::capture(&extra.filename, &extra.file_type));
        entries.push(BundleEntry::new(extra.filename.clone(), extra.data.clone()));
    }

    for att in &meta.attachments {
        let filename = format!("{}.attachment.bgsfile", random_hex(10));
        let mut attrs = Map::new();
        attrs.insert("mimeType".into(), Value::from(att.mime.clone()));
        files.push(FileDescriptor {
            filename: filename.clone(),
            file_type: "attachment".into(),
            name: Some(att.name.clone()),
            attrs: Some(attrs),
        });
        entries.push(BundleEntry::new(filename, att.data.clone()));
    }

    let manifest = Manifest::new(window, files, meta.attrs.clone());
    let manifest_bytes = serde_json::to_vec(&manifest)?;

    let request = IssueRequest {
        issue_type: meta.issue_type,
        summary: meta.summary.clone(),
        description: meta.description.clone(),
        labels: meta.labels.clone(),
        severity: meta.severity,
        email: meta.email.clone(),
        signatures: meta.signatures.clone(),
        source: Source {
            trigger: meta.source.trigger,
            origin: meta.source.origin.clone(),
        },
        created_on: iso8601_ms(created_on_ms),
        environment: env.clone(),
    };
    let request_json = serde_json::to_vec(&request)?;

    entries.push(BundleEntry::new(".apptoken", app_token.as_bytes().to_vec()));
    entries.push(BundleEntry::new("request.json", request_json.clone()));
    entries.push(BundleEntry::new("manifest.json", manifest_bytes));

    let bundle_name = bundle_filename();
    let mut zip = Vec::new();
    write_bundle(std::io::Cursor::new(&mut zip), &entries).map_err(std::io::Error::other)?;

    Ok(AssembledReport {
        bundle_name,
        zip,
        request_json,
    })
}

/// A default `ReportMeta` for a manually-triggered (`code_upload`) bug report.
pub fn manual_upload_meta() -> ReportMeta {
    use crate::model::enums::TriggerType;
    ReportMeta {
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
    }
}

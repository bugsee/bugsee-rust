//
//  lib.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Core capture, persistence, and export engine for the Bugsee Rust SDK.
//!
//! This crate is pure `std` with no host-integration dependencies. It owns the
//! wire-format data model, the disk-backed capture window, report export, the
//! report-bundle ZIP writer, crash-signature computation, and the transport
//! contract. Higher layers (`bugsee`, `bugsee-panic`, integrations) build on it.
//!
//! See `DESIGN.md` at the repository root for the full architecture.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod bundle;
pub mod capture;
pub mod errors;
pub mod model;
pub mod reporting;
pub mod runtime;
pub mod signature;
pub mod transport;
pub mod util;

pub use model::{
    entry::{Breadcrumb, CaptureEntry, EventEntry, LogEntry, NetworkEntry, SystemEvent, TraceEntry},
    enums::{BreadcrumbLevel, IssueType, LogLevel, LogSource, NetworkStage, Severity, TriggerType},
    envelope::Envelope,
    environment::Environment,
    report::{FileDescriptor, IssueRequest, Manifest, Source, TimeWindow},
    scope::Scope,
};
pub use reporting::{AssembledReport, ReportMeta};
pub use runtime::{Recorder, RecorderConfig};
pub use transport::{deliver, MockTransport, Transport, TransportError};

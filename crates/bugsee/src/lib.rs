//
//  lib.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Bugsee SDK for Rust — the public facade.
//!
//! Crash, panic, and handled-error reporting that produces backend-compatible
//! report bundles. This umbrella crate re-exports the capture engine from
//! [`bugsee_core`] and (behind features) the panic observer, tracing/log
//! adapters, network capture, and native-crash backend.
//!
//! ```no_run
//! use bugsee::{Bugsee, LaunchOptions};
//! use std::time::Duration;
//!
//! let _guard = Bugsee::launch("APP_TOKEN").unwrap();
//! Bugsee::event("app_started");
//! Bugsee::set_attribute("tier", "premium");
//! Bugsee::upload();
//! Bugsee::flush(Duration::from_secs(5));
//! ```
//!
//! See `DESIGN.md` at the repository root for the full architecture.

mod api;
mod frames;
mod options;
mod report;
#[cfg(feature = "net")]
pub mod transport;

pub use api::{Bugsee, LaunchGuard, ResultExt};
pub use options::LaunchOptions;
pub use report::Report;

/// Panic boundary guards. Wrap SDK-owned execution roots (FFI entry points,
/// spawned threads, task roots) so a Rust panic is contained and reported
/// instead of aborting the host.
#[cfg(feature = "panic")]
pub use bugsee_panic::{guard, spawn_guarded};

/// Re-exported core types (event model, enums, transport contract).
pub use bugsee_core as core;
pub use bugsee_core::model::enums::{
    BreadcrumbLevel, IssueType, LogLevel, LogSource, Severity, TriggerType,
};
pub use bugsee_core::reporting::{Attachment, ReportMeta};
pub use bugsee_core::transport::{Transport, TransportError};

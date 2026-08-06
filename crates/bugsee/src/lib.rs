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
mod perf;
mod report;
#[cfg(feature = "telemetry")]
mod telemetry;
#[cfg(feature = "net")]
pub mod transport;

/// Your application's version, read from `CARGO_PKG_VERSION` at the call site.
///
/// The SDK cannot read it itself: `env!("CARGO_PKG_VERSION")` compiled inside
/// this crate yields the SDK's version, not yours. A `macro_rules!` macro
/// expands in the CALLING crate, so `env!` here sees your `Cargo.toml`.
///
/// ```no_run
/// use bugsee::{Bugsee, LaunchOptions};
/// let _guard = Bugsee::launch_with(
///     LaunchOptions::new("APP_TOKEN").app_version(bugsee::app_version!()),
/// );
/// ```
#[macro_export]
macro_rules! app_version {
    () => {
        env!("CARGO_PKG_VERSION")
    };
}

/// Your crate's name, for use as the application identifier.
///
/// Same call-site expansion as [`app_version!`]. Without it the identifier
/// falls back to the executable's file name, which is usually the same thing
/// but is whatever the binary was renamed to.
#[macro_export]
macro_rules! app_package_id {
    () => {
        env!("CARGO_PKG_NAME")
    };
}

pub use api::{Bugsee, LaunchGuard, ResultExt};
pub use options::LaunchOptions;
pub use perf::{Span, Transaction};
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
pub use bugsee_core::model::perf::Status;
pub use bugsee_core::reporting::{Attachment, ReportMeta};
pub use bugsee_core::runtime::DropReason;
pub use bugsee_core::transport::{Transport, TransportError};

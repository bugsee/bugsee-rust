//
//  options.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Typed launch configuration. Mirrors the mobile SDK option surface, adapted
//! to a builder rather than a string-keyed registry.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::capture::WindowCaps;
use bugsee_core::model::entry::Breadcrumb;
use bugsee_core::reporting::ReportMeta;
use bugsee_core::runtime::{BeforeBreadcrumb, BeforeSend};
use bugsee_core::transport::Transport;

/// Configuration passed to [`crate::Bugsee::launch_with`].
pub struct LaunchOptions {
    pub(crate) app_token: String,
    pub(crate) app_version: Option<String>,
    pub(crate) app_build: Option<String>,
    pub(crate) app_package_id: Option<String>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) max_window: Duration,
    pub(crate) max_bytes: u64,
    pub(crate) max_events: u64,
    pub(crate) rotate_interval: Duration,
    pub(crate) endpoint: Option<String>,
    pub(crate) transport: Option<Arc<dyn Transport>>,
    pub(crate) native_crash_capture: bool,
    pub(crate) report_panics_from_hook: bool,
    pub(crate) system_telemetry: bool,
    pub(crate) before_send: Option<BeforeSend>,
    pub(crate) before_breadcrumb: Option<BeforeBreadcrumb>,
    pub(crate) sample_rate: f64,
}

impl LaunchOptions {
    /// Options for `app_token` with sensible defaults.
    pub fn new(app_token: impl Into<String>) -> Self {
        LaunchOptions {
            app_token: app_token.into(),
            app_version: None,
            app_build: None,
            app_package_id: None,
            data_dir: None,
            max_window: Duration::from_secs(60),
            max_bytes: 8 << 20,
            max_events: 100_000,
            rotate_interval: Duration::from_secs(1),
            endpoint: None,
            transport: None,
            native_crash_capture: true,
            report_panics_from_hook: false,
            system_telemetry: true,
            before_send: None,
            before_breadcrumb: None,
            sample_rate: 1.0,
        }
    }

    /// Fraction of non-fatal (`error`) reports to keep, in `[0.0, 1.0]`. Crashes
    /// are never sampled out. Default `1.0` (keep everything).
    pub fn sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Register a callback run on every report before delivery. Mutate the
    /// metadata (summary/severity/labels/attributes/…) in place; return `false`
    /// to drop the report.
    pub fn before_send(
        mut self,
        f: impl Fn(&mut ReportMeta) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.before_send = Some(Box::new(f));
        self
    }

    /// Enable/disable installing the native fatal-crash handler (default `true`).
    /// Disable when the host already owns native crash handling.
    pub fn native_crash_capture(mut self, enabled: bool) -> Self {
        self.native_crash_capture = enabled;
        self
    }

    /// Report panics **immediately from the panic hook** instead of the default
    /// mark-and-recover behaviour (default `false`).
    ///
    /// By default the hook only captures: a panic caught at an SDK boundary is
    /// reported there as `handled`, and an uncaught panic that terminates the
    /// process is reported on the **next launch** by recovery. That keeps the
    /// fault path minimal — no allocation, disk or network work on the panicking
    /// thread.
    ///
    /// Enable this to deliver the report during the panic instead. Two reasons
    /// to want it:
    /// - **immediacy** — the report does not wait for a next launch, which
    ///   matters for short-lived processes that may never restart;
    /// - **thread panics** — a panic that kills a non-main thread never unwinds
    ///   out of `main`, so there is no process death for recovery to observe and
    ///   the default mode will not report it.
    ///
    /// The cost is that reporting runs on the panicking thread. Pair it with a
    /// `flush` in your own hook/shutdown path if you need delivery guaranteed
    /// before the process dies.
    pub fn report_panics_from_hook(mut self, enabled: bool) -> Self {
        self.report_panics_from_hook = enabled;
        self
    }

    /// Enable/disable periodic system/process telemetry sampling (default `true`).
    pub fn system_telemetry(mut self, enabled: bool) -> Self {
        self.system_telemetry = enabled;
        self
    }

    /// Override the on-disk data directory (defaults to a per-app temp dir).
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Maximum wall-time span of captured context retained.
    pub fn max_window(mut self, window: Duration) -> Self {
        self.max_window = window;
        self
    }

    /// Maximum on-disk bytes retained across the capture window.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Maximum total entry count retained across the capture window.
    pub fn max_events(mut self, events: u64) -> Self {
        self.max_events = events;
        self
    }

    /// Register a callback run on every breadcrumb before it is captured. Return
    /// the (possibly-mutated) breadcrumb to keep it, or `None` to drop it.
    pub fn before_breadcrumb(
        mut self,
        f: impl Fn(Breadcrumb) -> Option<Breadcrumb> + Send + Sync + 'static,
    ) -> Self {
        self.before_breadcrumb = Some(Box::new(f));
        self
    }

    /// Override the API base URL (defaults to `https://api.bugsee.com/v2`).
    /// The application's version, e.g. `"1.4.2"`.
    ///
    /// Without it an issue carries no version at all: it cannot be filtered by
    /// release, and regressions cannot be tracked across builds. The SDK cannot
    /// infer it — `env!("CARGO_PKG_VERSION")` inside this crate is the SDK's own
    /// version — so pass [`crate::app_version!`], which expands in YOUR crate.
    pub fn app_version(mut self, version: impl Into<String>) -> Self {
        self.app_version = Some(version.into());
        self
    }

    /// The application's build number, if it has one distinct from the version.
    pub fn app_build(mut self, build: impl Into<String>) -> Self {
        self.app_build = Some(build.into());
        self
    }

    /// Overrides the application identifier, which otherwise defaults to the
    /// executable's file name. Use a bundle-id-style string
    /// (`"com.example.app"`) to match how the mobile SDKs report it.
    pub fn app_package_id(mut self, package_id: impl Into<String>) -> Self {
        self.app_package_id = Some(package_id.into());
        self
    }

    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Inject a custom transport (e.g. for tests), bypassing the default HTTP one.
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    pub(crate) fn caps(&self) -> WindowCaps {
        WindowCaps {
            max_window_ms: self.max_window.as_millis() as i64,
            max_bytes: self.max_bytes,
            max_events: self.max_events,
        }
    }

    pub(crate) fn resolved_data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            let mut dir = std::env::temp_dir();
            dir.push("bugsee");
            // Isolate by app token so multiple apps don't collide.
            dir.push(short_token(&self.app_token));
            dir
        })
    }
}

fn short_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

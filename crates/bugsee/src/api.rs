//
//  api.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The global `Bugsee` facade — the public entry point. A process-wide
//! singleton recorder is installed by `launch` and torn down by `stop` (or the
//! returned guard's `Drop`). All capture methods are safe no-ops before launch
//! and after stop.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use bugsee_core::errors::{self, Cause};
use bugsee_core::model::entry::{CaptureEntry, EventEntry, LogEntry, TraceEntry};
use bugsee_core::model::enums::{LogLevel, LogSource, Severity};
use bugsee_core::reporting::{manual_upload_meta, ReportMeta};
use bugsee_core::util::epoch_ms;
use bugsee_core::{Recorder, RecorderConfig};
use serde_json::{Map, Value};

use crate::options::LaunchOptions;
use crate::report::Report;

// A `Mutex` (not `RwLock`) because `mpsc::Sender` inside `Recorder` is `Send`
// but not `Sync`; the lock is held only long enough to enqueue.
static RECORDER: Mutex<Option<Recorder>> = Mutex::new(None);
static PAUSED: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "native")]
static NATIVE: Mutex<Option<bugsee_native::NativeHandler>> = Mutex::new(None);

/// The Bugsee SDK facade.
pub struct Bugsee;

/// Returned by `launch`; tears the SDK down when dropped.
pub struct LaunchGuard {
    _private: (),
}

impl Drop for LaunchGuard {
    fn drop(&mut self) {
        Bugsee::stop();
    }
}

impl Bugsee {
    /// Launch with default options for `app_token`.
    pub fn launch(app_token: &str) -> io::Result<LaunchGuard> {
        Self::launch_with(LaunchOptions::new(app_token))
    }

    /// Launch with explicit [`LaunchOptions`].
    pub fn launch_with(options: LaunchOptions) -> io::Result<LaunchGuard> {
        let data_dir = options.resolved_data_dir();
        std::fs::create_dir_all(&data_dir)?;

        let transport = match options.transport.clone() {
            Some(t) => t,
            None => {
                #[cfg(feature = "net")]
                {
                    std::sync::Arc::new(crate::transport::HttpTransport::new(options.endpoint.clone()))
                }
                #[cfg(not(feature = "net"))]
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "no transport: enable the `net` feature or inject one via LaunchOptions::with_transport",
                    ));
                }
            }
        };

        let mut config = RecorderConfig::new(data_dir, options.app_token.clone());
        config.caps = options.caps();
        config.rotate_interval = options.rotate_interval;

        #[cfg(feature = "telemetry")]
        if options.system_telemetry {
            config.sampler = Some(Box::new(crate::telemetry::SysinfoSampler::new()));
        }

        let recorder = Recorder::launch(config, transport)?;

        #[cfg(feature = "panic")]
        let panic_info_path = recorder.panic_info_path();

        // Install the native crash handler pointing at this generation's marker.
        #[cfg(feature = "native")]
        if options.native_crash_capture {
            if let Ok(handler) = bugsee_native::install(recorder.crash_info_path()) {
                *NATIVE.lock().unwrap() = Some(handler);
            }
        }

        *RECORDER.lock().unwrap() = Some(recorder);
        PAUSED.store(false, Ordering::SeqCst);

        #[cfg(feature = "panic")]
        {
            bugsee_panic::install(std::sync::Arc::new(PanicSink));
            // Persist panic snapshots so an aborting panic correlates with its
            // SIGABRT on the next launch.
            bugsee_panic::set_snapshot_path(panic_info_path);
        }

        Ok(LaunchGuard { _private: () })
    }

    /// Stop the SDK and flush pending work (dropping the recorder joins the worker).
    pub fn stop() {
        #[cfg(feature = "native")]
        {
            let _ = NATIVE.lock().unwrap().take();
        }
        let _ = RECORDER.lock().unwrap().take();
    }

    /// Whether the SDK is launched and not paused.
    pub fn is_active() -> bool {
        RECORDER.lock().unwrap().is_some() && !PAUSED.load(Ordering::SeqCst)
    }

    /// Pause capture (events are dropped until [`Bugsee::resume`]).
    pub fn pause() {
        PAUSED.store(true, Ordering::SeqCst);
    }

    /// Resume capture.
    pub fn resume() {
        PAUSED.store(false, Ordering::SeqCst);
    }

    /// Capture a log line.
    pub fn log(level: LogLevel, message: impl Into<String>) {
        Self::capture(CaptureEntry::Log(LogEntry {
            timestamp: epoch_ms(),
            level,
            source: LogSource::Custom,
            tag: None,
            message: Some(message.into()),
            custom: Map::new(),
        }));
    }

    /// Record a developer event.
    pub fn event(name: impl Into<String>) {
        Self::event_with(name, Map::new());
    }

    /// Record a developer event with parameters.
    pub fn event_with(name: impl Into<String>, params: Map<String, Value>) {
        Self::capture(CaptureEntry::UserEvent(EventEntry {
            timestamp: epoch_ms(),
            name: Some(name.into()),
            params,
            custom: Map::new(),
        }));
    }

    /// Record a captured network entry (used by the network-capture integration).
    pub fn capture_network(entry: bugsee_core::model::entry::NetworkEntry) {
        Self::capture(CaptureEntry::Network(Box::new(entry)));
    }

    /// Record a named value trace.
    pub fn trace(name: impl Into<String>, value: impl Into<Value>) {
        Self::capture(CaptureEntry::UserTrace(TraceEntry {
            timestamp: epoch_ms(),
            display_id: None,
            name: Some(name.into()),
            value: value.into(),
            custom: Map::new(),
        }));
    }

    /// Set the user identifier (`request.json.email`).
    pub fn set_email(email: impl Into<String>) {
        let email = email.into();
        Self::with_recorder(|r| r.with_scope(|s| s.set_email(Some(email.clone()))));
    }

    /// Set a report-level attribute (`manifest.attrs`).
    pub fn set_attribute(key: impl Into<String>, value: impl Into<Value>) {
        let key = key.into();
        let value = value.into();
        Self::with_recorder(|r| r.with_scope(|s| s.set_attribute(key.clone(), value.clone())));
    }

    /// Clear one report-level attribute.
    pub fn clear_attribute(key: &str) {
        Self::with_recorder(|r| r.with_scope(|s| s.clear_attribute(key)));
    }

    /// Clear all report-level attributes.
    pub fn clear_all_attributes() {
        Self::with_recorder(|r| r.with_scope(|s| s.clear_all_attributes()));
    }

    /// Trigger an immediate `code_upload` report.
    pub fn upload() {
        Self::with_recorder(|r| r.report(manual_upload_meta(), None));
    }

    /// Trigger an immediate report with explicit metadata.
    pub fn upload_with(meta: ReportMeta) {
        Self::with_recorder(|r| r.report(meta, None));
    }

    /// Report a handled error as a non-fatal `error` issue, walking its
    /// `source()` chain and capturing a backtrace.
    pub fn capture_error<E: std::error::Error + ?Sized>(err: &E) {
        let name = short_type_name(std::any::type_name::<E>());
        let reason = err.to_string();
        let mut causes = Vec::new();
        let mut source = err.source();
        while let Some(s) = source {
            causes.push(Cause {
                name: "Error".to_string(),
                reason: s.to_string(),
            });
            source = s.source();
        }
        let frames = crate::frames::capture();
        let built = errors::build_handled_error(&name, &reason, &causes, frames, epoch_ms());
        Self::with_recorder(|r| r.report(built.meta, Some(built.crash_json)));
    }

    /// Report a message as a non-fatal `error` issue with a backtrace.
    pub fn capture_message(level: LogLevel, message: impl Into<String>) {
        let frames = crate::frames::capture();
        let mut built = errors::build_message(&message.into(), frames, epoch_ms());
        built.meta.severity = match level {
            LogLevel::Error => Severity::High,
            LogLevel::Warning => Severity::Medium,
            _ => Severity::VeryLow,
        };
        Self::with_recorder(|r| r.report(built.meta, Some(built.crash_json)));
    }

    /// Begin a deferred report, snapshotting the window now.
    pub fn create_report() -> Report {
        Report::new(epoch_ms())
    }

    /// Start an APM transaction. Open child spans on it and `finish()` to record.
    pub fn start_transaction(
        name: impl Into<String>,
        operation: impl Into<String>,
    ) -> crate::perf::Transaction {
        crate::perf::Transaction::start(name.into(), operation.into())
    }

    /// Block until pending work drains, or `timeout` elapses.
    pub fn flush(timeout: Duration) -> bool {
        Self::with_recorder(|r| r.flush(timeout)).unwrap_or(false)
    }

    fn capture(entry: CaptureEntry) {
        if PAUSED.load(Ordering::SeqCst) {
            return;
        }
        Self::with_recorder(|r| r.capture(entry));
    }

    fn with_recorder<R>(f: impl FnOnce(&Recorder) -> R) -> Option<R> {
        RECORDER.lock().unwrap().as_ref().map(f)
    }
}

/// Submit a populated deferred report (called by `Report::upload`).
pub(crate) fn submit_report(meta: ReportMeta, window_end: i64) {
    Bugsee::with_recorder(|r| r.report_at(meta, window_end, None));
}

/// Capture a completed APM transaction (called by `perf::Transaction::finish`).
pub(crate) fn submit_transaction(transaction: bugsee_core::model::perf::Transaction) {
    if PAUSED.load(Ordering::SeqCst) {
        return;
    }
    Bugsee::with_recorder(|r| r.capture(CaptureEntry::Performance(Box::new(transaction))));
}

/// Bridges the panic observer to the recorder: a caught panic becomes a
/// handled `error` report, an escaped panic a `crash` report.
#[cfg(feature = "panic")]
struct PanicSink;

#[cfg(feature = "panic")]
impl bugsee_panic::PanicReporter for PanicSink {
    fn report_panic(&self, report: bugsee_panic::PanicReport) {
        let reason = match &report.file {
            Some(file) => format!("{} ({}:{}:{})", report.reason, file, report.line, report.column),
            None => report.reason.clone(),
        };
        let built = errors::build_panic(&reason, report.frames, report.handled, epoch_ms());
        Bugsee::with_recorder(|r| r.report(built.meta, Some(built.crash_json)));
    }
}

/// Shorten a fully-qualified type path (`a::b::MyError` → `MyError`) while
/// keeping any generic parameters.
fn short_type_name(full: &str) -> String {
    let (base, generics) = match full.split_once('<') {
        Some((b, g)) => (b, Some(g)),
        None => (full, None),
    };
    let short = base.rsplit("::").next().unwrap_or(base);
    match generics {
        Some(g) => format!("{short}<{g}"),
        None => short.to_string(),
    }
}

/// `Result` ergonomics: report the error (if any) and pass the result through,
/// so a fallible expression can be instrumented inline with `.capture()?`.
pub trait ResultExt<T, E> {
    /// Report `Err` to Bugsee, then return `self` unchanged.
    fn capture(self) -> Result<T, E>;
}

impl<T, E: std::error::Error> ResultExt<T, E> for Result<T, E> {
    fn capture(self) -> Result<T, E> {
        if let Err(ref e) = self {
            Bugsee::capture_error(e);
        }
        self
    }
}

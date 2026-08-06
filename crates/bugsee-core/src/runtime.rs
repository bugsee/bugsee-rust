//
//  runtime.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The capture runtime: a capture worker that owns the part store and drains
//! entries, plus an uploader thread that delivers reports from the durable
//! on-disk queue with retry. Producers only enqueue; all disk and network work
//! happens off the caller's thread.
//!
//! Robustness invariants (post crash-review hardening):
//! - Locks use poison-recovery (`lock_recover`) so an internal panic never
//!   propagates a poisoned-mutex panic onto a host thread.
//! - The worker and uploader wrap each iteration in `catch_unwind`, so a panic
//!   in a host callback (`before_send`, `TelemetrySampler`, `Transport`) cannot
//!   silently kill capture/delivery for the process lifetime. NOTE: this
//!   containment relies on the unwinding panic runtime; under `panic = "abort"`
//!   `catch_unwind` is inert and a host-callback panic aborts the process. The
//!   crate is intended to be built `panic = "unwind"` (its own release profile
//!   sets this); native fatal-crash capture still works under `abort`, but
//!   per-callback containment does not (DESIGN.md §5/§15).
//! - The producer→worker channel is bounded by an in-flight counter (drop-newest
//!   under back-pressure) so a burst or a stalled worker cannot grow RSS without
//!   limit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{export, PartStore, WindowCaps};
use crate::model::entry::{Breadcrumb, CaptureEntry, TraceEntry};
use crate::model::enums::IssueType;
use crate::model::environment::Environment;
use crate::model::report::TimeWindow;
use crate::model::scope::Scope;
use crate::reporting::{self, AssembledReport, ReportMeta};
use crate::session::Session;
use crate::transport::{self, Transport, TransportError};
use crate::util::epoch_ms;
use crate::{queue, recovery};

/// Maximum upload attempts before a queued report is abandoned.
const UPLOAD_RETRY_CAP: u32 = 60;
/// Maximum next-launch recovery attempts for one pending session before it is
/// abandoned (guards against a corrupt-parts poison pill re-running every launch).
const MAX_RECOVERY_ATTEMPTS: u32 = 3;
/// Default in-flight capture-entry cap (drop-newest beyond this).
const DEFAULT_MAX_QUEUED: usize = 8192;
/// Default in-flight *report* cap. Reports are far heavier than capture entries
/// (each carries a `ReportMeta` + `crash_json`, and processing does snapshot +
/// ZIP + enqueue), so a report storm — e.g. `capture_error` on a hot path during
/// an outage — must not grow the channel without bound. Over this cap, new
/// NON-crash reports are dropped-newest; crash reports are never dropped.
const DEFAULT_MAX_REPORT_QUEUED: usize = 256;

/// A callback run on every report before delivery. Mutate the metadata in place;
/// return `false` to drop the report entirely.
pub type BeforeSend = Box<dyn Fn(&mut ReportMeta) -> bool + Send + Sync>;

/// Why a report was abandoned instead of delivered.
///
/// Every variant means the report is GONE — it has been removed from the
/// durable queue and will not be retried.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The server rejected it and retrying cannot help — a validation failure,
    /// an unknown app token, and so on. Carries the server's own message when
    /// it sent one, which is otherwise the only account of why.
    Rejected(String),
    /// The server reported an identical crash already recorded.
    Duplicate,
    /// Too many similar crashes; the signatures are now blacklisted locally so
    /// the same crash-on-launch loop stops re-uploading.
    TooManySimilar,
    /// Delivery kept failing transiently until the durable retry cap.
    RetriesExhausted,
}

/// Called when a report is abandoned. See [`DropReason`].
///
/// Exists because delivery is otherwise SILENT: the queue is drained on success
/// and on permanent failure alike, so [`Recorder::flush`] returning `true`
/// cannot distinguish "delivered" from "thrown away", and this crate does no
/// logging. Without a hook a host has no way to learn that its crash reports
/// are being rejected.
///
/// Runs on the uploader thread. A panic here is contained by the uploader's
/// `catch_unwind`, like any other host callback.
pub type OnReportDropped = Box<dyn Fn(DropReason) + Send + Sync>;

/// A callback run on every breadcrumb before it is captured. Return the
/// (possibly-mutated) breadcrumb to keep it, or `None` to drop it.
pub type BeforeBreadcrumb = Box<dyn Fn(Breadcrumb) -> Option<Breadcrumb> + Send + Sync>;

/// Lock a mutex, recovering the guard if it was poisoned by a panic. Library
/// code must never propagate a poisoned-lock panic onto a host thread.
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Produces system/process telemetry samples appended as `traces.system` on
/// each rotation tick. Implemented by the host layer (e.g. via `sysinfo`).
pub trait TelemetrySampler: Send {
    /// Return `(trace_name, value)` pairs for this tick (e.g. `cpu_usage`).
    fn sample(&mut self) -> Vec<(String, Value)>;
}

/// Configuration for a [`Recorder`].
pub struct RecorderConfig {
    pub data_dir: PathBuf,
    pub app_token: String,
    pub sdk_version: String,
    pub caps: WindowCaps,
    pub rotate_interval: Duration,
    /// Optional telemetry sampler run each rotation tick.
    pub sampler: Option<Box<dyn TelemetrySampler>>,
    /// Facts core cannot see from `std` alone (OS version, memory). Supplied by
    /// the host layer; absent fields are simply omitted from the environment.
    pub host_facts: crate::model::environment::HostFacts,
    /// Application identity (package id / version / build). Core cannot infer a
    /// host's version, so it must be supplied; unset fields are omitted.
    pub app_identity: crate::model::environment::AppIdentity,
    /// Base delay for upload retry backoff (doubles per attempt, capped at 300 s).
    pub upload_backoff_base: Duration,
    /// Optional callback to mutate or drop reports before delivery.
    pub before_send: Option<BeforeSend>,
    /// Notified whenever a report is abandoned rather than delivered.
    pub on_report_dropped: Option<OnReportDropped>,
    /// Optional callback to mutate or drop breadcrumbs before capture.
    pub before_breadcrumb: Option<BeforeBreadcrumb>,
    /// Fraction of non-fatal (`error`) reports to keep, in `[0, 1]`. Crashes are
    /// never sampled out.
    pub sample_rate: f64,
    /// Max in-flight capture entries before new ones are dropped (back-pressure).
    pub max_queued_entries: usize,
}

impl RecorderConfig {
    /// A config with default caps and a 1 s rotation interval.
    pub fn new(data_dir: impl Into<PathBuf>, app_token: impl Into<String>) -> Self {
        RecorderConfig {
            data_dir: data_dir.into(),
            app_token: app_token.into(),
            sdk_version: env!("CARGO_PKG_VERSION").to_string(),
            caps: WindowCaps::default(),
            rotate_interval: Duration::from_secs(1),
            sampler: None,
            host_facts: Default::default(),
            app_identity: Default::default(),
            upload_backoff_base: Duration::from_secs(30),
            before_send: None,
            on_report_dropped: None,
            before_breadcrumb: None,
            sample_rate: 1.0,
            max_queued_entries: DEFAULT_MAX_QUEUED,
        }
    }
}

/// A handle to a report snapshot taken at `create_report` time. The window is
/// hard-linked on disk immediately, so later eviction cannot erode it. Dropping
/// the handle without delivering it removes the on-disk links — so no path
/// (timeout, worker-gone, shutdown race) can leak the snapshot directory.
pub struct SnapshotHandle {
    dir: PathBuf,
    window: TimeWindow,
}

impl Drop for SnapshotHandle {
    fn drop(&mut self) {
        // Cleanup is unconditional: the assembler finishes reading the dir before
        // the handle drops, and every not-delivered path (timeout, worker-gone,
        // shutdown race) then reclaims the hard links here.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// State shared between the facade, the capture worker, and the uploader.
struct Shared {
    scope: Mutex<Scope>,
    env: Environment,
    environment_json: Vec<u8>,
    app_token: String,
    session: Mutex<Option<String>>,
    transport: Arc<dyn Transport>,
    before_send: Option<BeforeSend>,
    on_report_dropped: Option<OnReportDropped>,
    before_breadcrumb: Option<BeforeBreadcrumb>,
    sample_rate: f64,
    /// In-flight capture entries queued to the worker (back-pressure counter).
    queued: AtomicUsize,
    max_queued: usize,
    /// In-flight report messages queued to the worker (heavier than captures, so
    /// bounded separately; drop-newest non-crash reports over the cap).
    report_queued: AtomicUsize,
    max_report_queued: usize,
    /// Whether telemetry sampling is paused (mirrors the facade's pause state).
    paused: AtomicBool,
}

enum Msg {
    Capture(CaptureEntry),
    Report {
        meta: ReportMeta,
        window_end: i64,
        crash_json: Option<Vec<u8>>,
    },
    /// Snapshot the window now, returning a handle (deferred `create_report`).
    Snapshot {
        window_end: i64,
        ack: Sender<Option<SnapshotHandle>>,
    },
    /// Deliver a report from a pre-taken snapshot.
    UploadSnapshot {
        handle: SnapshotHandle,
        meta: ReportMeta,
    },
    Flush(Sender<()>),
    Stop(Sender<()>),
}

enum UploadMsg {
    Wake,
    /// Force-drain (up to `deadline`) and report whether the queue is empty.
    Drain {
        deadline: Duration,
        ack: Sender<bool>,
    },
    Stop(Sender<()>),
}

/// The running capture pipeline.
pub struct Recorder {
    tx: Sender<Msg>,
    upload_tx: Sender<UploadMsg>,
    shared: Arc<Shared>,
    caps: WindowCaps,
    data_dir: PathBuf,
    session: Session,
    worker: Option<JoinHandle<()>>,
    uploader: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Start the workers and begin capturing. Recovers any prior session that
    /// ended abnormally, queueing it for delivery.
    pub fn launch(config: RecorderConfig, transport: Arc<dyn Transport>) -> std::io::Result<Self> {
        let env = Environment::detect_with(
            &config.sdk_version,
            &config.host_facts,
            &config.app_identity,
        );
        let environment_json = serde_json::to_vec(&env).unwrap_or_default();
        let shared = Arc::new(Shared {
            scope: Mutex::new(Scope::default()),
            env,
            environment_json,
            app_token: config.app_token.clone(),
            session: Mutex::new(None),
            transport,
            before_send: config.before_send,
            on_report_dropped: config.on_report_dropped,
            before_breadcrumb: config.before_breadcrumb,
            sample_rate: config.sample_rate,
            queued: AtomicUsize::new(0),
            max_queued: config.max_queued_entries.max(1),
            report_queued: AtomicUsize::new(0),
            max_report_queued: DEFAULT_MAX_REPORT_QUEUED,
            paused: AtomicBool::new(false),
        });

        let session = Session::begin(&config.data_dir)?;
        let generation = session.generation();
        let data_dir = config.data_dir.clone();

        // Uploader thread: drains the durable queue with retry.
        let (upload_tx, upload_rx) = channel();
        let uploader_shared = Arc::clone(&shared);
        let uploader_data_dir = data_dir.clone();
        let backoff_base = config.upload_backoff_base;
        let uploader = std::thread::Builder::new()
            .name("bugsee-uploader".into())
            .spawn(move || {
                uploader_loop(upload_rx, uploader_shared, uploader_data_dir, backoff_base)
            })?;

        // Capture worker: owns the part store; enqueues reports.
        let (tx, rx) = channel();
        let store = PartStore::new(&config.data_dir, generation, config.caps)?;
        let worker_shared = Arc::clone(&shared);
        let rotate_interval = config.rotate_interval;
        let worker_data_dir = data_dir.clone();
        let caps = config.caps;
        let sampler = config.sampler;
        let worker_upload_tx = upload_tx.clone();

        let worker = std::thread::Builder::new()
            .name("bugsee-capture".into())
            .spawn(move || {
                // Queue any crashed prior session before capturing this one.
                // Contained: a panic here (e.g. a host before_send on a recovered
                // crash) must not abort the thread before worker_loop starts.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Reap orphaned queue sidecars from a prior killed enqueue/
                    // remove first (runs before this session enqueues anything).
                    queue::gc_orphans(&worker_data_dir);
                    run_recovery(&worker_shared, &worker_data_dir, generation);
                }));
                let _ = worker_upload_tx.send(UploadMsg::Wake);
                worker_loop(
                    rx,
                    store,
                    worker_shared,
                    rotate_interval,
                    worker_data_dir,
                    caps,
                    sampler,
                    worker_upload_tx,
                );
            })?;

        Ok(Recorder {
            tx,
            upload_tx,
            shared,
            caps,
            data_dir,
            session,
            worker: Some(worker),
            uploader: Some(uploader),
        })
    }

    /// Enqueue an entry. Never blocks; drops (newest) under back-pressure or if
    /// the worker is gone.
    pub fn capture(&self, entry: CaptureEntry) {
        capture_into(&self.shared, &self.tx, entry);
    }

    /// A cheap, cloneable handle for the operations the facade must run WITHOUT
    /// holding its global lock — anything that blocks (`flush`, `create_snapshot`)
    /// or runs host callbacks (`before_breadcrumb`). Holding a mutex across those
    /// would stall all capture or (for a re-entrant callback) deadlock.
    pub fn handle(&self) -> RecorderHandle {
        RecorderHandle {
            tx: self.tx.clone(),
            upload_tx: self.upload_tx.clone(),
            shared: Arc::clone(&self.shared),
        }
    }

    /// Apply the configured `before_breadcrumb` hook to `crumb`. Returns the
    /// (possibly-mutated) breadcrumb to keep, or `None` to drop it. With no hook
    /// configured the breadcrumb passes through unchanged. A panicking hook is
    /// contained and drops just that breadcrumb (never crashes the caller).
    pub fn before_breadcrumb(&self, crumb: Breadcrumb) -> Option<Breadcrumb> {
        self.handle().before_breadcrumb(crumb)
    }

    /// Route the facade's pause state into the recorder so the worker skips
    /// telemetry sampling while paused (in addition to the facade gating
    /// producer entries).
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Mutate the ambient scope (email / labels / attributes).
    pub fn with_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> R {
        let mut guard = lock_recover(&self.shared.scope);
        f(&mut guard)
    }

    /// Trigger a report with `meta`, snapshotting the window ending now.
    pub fn report(&self, meta: ReportMeta, crash_json: Option<Vec<u8>>) {
        self.report_at(meta, epoch_ms(), crash_json);
    }

    /// Like [`Recorder::report`] but with an explicit window-end timestamp.
    pub fn report_at(&self, meta: ReportMeta, window_end: i64, crash_json: Option<Vec<u8>>) {
        report_into(&self.shared, &self.tx, meta, window_end, crash_json);
    }

    /// Snapshot the window now (hard-links on disk), returning a handle to
    /// upload later. Blocks briefly on the worker; returns `None` on failure.
    pub fn create_snapshot(&self, window_end: i64) -> Option<SnapshotHandle> {
        self.handle().create_snapshot(window_end)
    }

    /// Deliver a report from a snapshot handle produced by [`create_snapshot`].
    pub fn upload_snapshot(&self, handle: SnapshotHandle, meta: ReportMeta) {
        self.handle().upload_snapshot(handle, meta)
    }

    /// Discard a snapshot handle without uploading. Dropping it reclaims the
    /// on-disk links via `SnapshotHandle::Drop`.
    pub fn discard_snapshot(handle: SnapshotHandle) {
        drop(handle);
    }

    /// Block until captured work is persisted and the queue is fully drained, or
    /// `timeout` elapses. Returns `true` only if the outbound queue is empty.
    pub fn flush(&self, timeout: Duration) -> bool {
        self.handle().flush(timeout)
    }

    /// Data directory root for this recorder.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Path where the native crash handler should write its crash-info marker.
    pub fn crash_info_path(&self) -> PathBuf {
        self.gen_dir().join(crate::recovery::CRASH_INFO_NAME)
    }

    /// Path where the panic observer should persist its snapshot.
    pub fn panic_info_path(&self) -> PathBuf {
        self.gen_dir().join(crate::panic_info::PANIC_INFO_NAME)
    }

    fn gen_dir(&self) -> PathBuf {
        self.data_dir
            .join("parts")
            .join(self.session.generation().to_string())
    }

    /// Configured window caps.
    pub fn caps(&self) -> WindowCaps {
        self.caps
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // Signal both threads; JOIN only if they acknowledge within the timeout.
        // A thread wedged in a host callback with no timeout is DETACHED (its
        // JoinHandle dropped un-joined) rather than hanging the stopping thread
        // forever. (Called only after the facade released its global lock.)
        let (tx, rx) = channel();
        let worker_acked =
            self.tx.send(Msg::Stop(tx)).is_ok() && rx.recv_timeout(Duration::from_secs(2)).is_ok();
        if let Some(handle) = self.worker.take() {
            if worker_acked {
                let _ = handle.join();
            }
        }
        let (utx, urx) = channel();
        let uploader_acked = self.upload_tx.send(UploadMsg::Stop(utx)).is_ok()
            && urx.recv_timeout(Duration::from_secs(2)).is_ok();
        if let Some(handle) = self.uploader.take() {
            if uploader_acked {
                let _ = handle.join();
            }
        }
        // Only mark a genuinely clean shutdown. If either thread merely DETACHED
        // (didn't ack in time — e.g. a slow flush), it may still be writing this
        // session's data, so removing the marker now would make next-launch
        // recovery miss the generation entirely if the process then dies mid-tail.
        // Leaving the marker costs at worst a false-positive abnormal-exit report
        // (or a clean re-skip by gc once the detached flush finishes) — far safer
        // than silently losing a crash.
        //
        // `thread::panicking()` is the precise "we are dying FROM a panic"
        // signal: an uncaught panic unwinds out of `main`, drops the launch
        // guard and lands here on the still-unwinding thread. Keeping the
        // liveness marker in that case is what lets next-launch recovery report
        // the fatal panic — without it we would shut down "cleanly", drop the
        // marker, and the panic that killed the process would go unreported
        // (the Rust default is `panic = "unwind"`, so this is the common case).
        // A panic that was merely *contained* never reaches here while
        // unwinding, and its snapshot is deleted by `report_caught`, so this
        // cannot turn a handled panic into a phantom crash.
        if worker_acked && uploader_acked && !std::thread::panicking() {
            self.session.end();
        }
    }
}

/// A cheap cloneable view of the recorder's channels + shared state. The facade
/// clones one out under a brief lock, releases the lock, and then does any
/// blocking / host-callback work through it — never holding the global mutex
/// across a blocking wait or user code.
#[derive(Clone)]
pub struct RecorderHandle {
    tx: Sender<Msg>,
    upload_tx: Sender<UploadMsg>,
    shared: Arc<Shared>,
}

impl RecorderHandle {
    /// Enqueue a capture entry (bounded, drop-newest under back-pressure).
    pub fn capture(&self, entry: CaptureEntry) {
        capture_into(&self.shared, &self.tx, entry);
    }

    /// Trigger a report with an explicit window-end timestamp.
    pub fn report_at(&self, meta: ReportMeta, window_end: i64, crash_json: Option<Vec<u8>>) {
        report_into(&self.shared, &self.tx, meta, window_end, crash_json);
    }

    /// Mutate the ambient scope.
    pub fn with_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> R {
        let mut guard = lock_recover(&self.shared.scope);
        f(&mut guard)
    }

    /// Set the paused flag (gates telemetry sampling).
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Run the `before_breadcrumb` hook (panic-contained), off any caller lock.
    pub fn before_breadcrumb(&self, crumb: Breadcrumb) -> Option<Breadcrumb> {
        match &self.shared.before_breadcrumb {
            Some(hook) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(crumb)))
                .unwrap_or(None),
            None => Some(crumb),
        }
    }

    /// Snapshot the window now (blocks briefly on the worker). Returns `None` on
    /// failure or timeout.
    pub fn create_snapshot(&self, window_end: i64) -> Option<SnapshotHandle> {
        let (ack, rx) = channel();
        if self.tx.send(Msg::Snapshot { window_end, ack }).is_err() {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
    }

    /// Deliver a report from a pre-taken snapshot.
    pub fn upload_snapshot(&self, handle: SnapshotHandle, meta: ReportMeta) {
        let _ = self.tx.send(Msg::UploadSnapshot { handle, meta });
    }

    /// Block until captured work is persisted and the queue drains, or `timeout`.
    /// Returns `true` only if the outbound queue is empty.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (tx, rx) = channel();
        if self.tx.send(Msg::Flush(tx)).is_err() {
            return false;
        }
        if rx.recv_timeout(timeout).is_err() {
            return false;
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        let (utx, urx) = channel();
        if self
            .upload_tx
            .send(UploadMsg::Drain {
                deadline: remaining,
                ack: utx,
            })
            .is_err()
        {
            return false;
        }
        urx.recv_timeout(remaining).unwrap_or(false)
    }
}

/// Bounded, drop-newest capture enqueue shared by `Recorder` and `RecorderHandle`.
fn capture_into(shared: &Arc<Shared>, tx: &Sender<Msg>, entry: CaptureEntry) {
    // Atomically reserve a slot; if that pushes us over the cap, release it and
    // drop (so the bound holds precisely even under concurrent producers).
    if shared.queued.fetch_add(1, Ordering::Relaxed) >= shared.max_queued {
        shared.queued.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    if tx.send(Msg::Capture(entry)).is_err() {
        shared.queued.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bounded report enqueue shared by `Recorder` and `RecorderHandle`. Non-crash
/// reports are dropped-newest once the in-flight report backlog exceeds the cap,
/// so a report storm can't grow the channel (RSS) without bound (F18); crash
/// reports are never dropped. The worker decrements `report_queued` as it drains
/// each `Msg::Report`.
fn report_into(
    shared: &Arc<Shared>,
    tx: &Sender<Msg>,
    meta: ReportMeta,
    window_end: i64,
    crash_json: Option<Vec<u8>>,
) {
    // Every enqueued report increments the counter (the worker decrements on
    // drain), but only NON-crash reports are subject to the drop-newest cap.
    let prev = shared.report_queued.fetch_add(1, Ordering::Relaxed);
    if meta.issue_type != IssueType::Crash && prev >= shared.max_report_queued {
        shared.report_queued.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    if tx
        .send(Msg::Report {
            meta,
            window_end,
            crash_json,
        })
        .is_err()
    {
        shared.report_queued.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Recover any prior generation that ended abnormally, queueing it for delivery.
/// The crashed session's state is discarded ONLY after the report is durably
/// enqueued — a failure leaves it on disk for a later attempt (no crash loss).
fn run_recovery(shared: &Shared, data_dir: &Path, current_generation: u64) {
    for pending in recovery::find_pending(data_dir, current_generation) {
        // Count the attempt *before* processing: a corrupt session that panics
        // the build/assemble path is a poison pill that would otherwise re-run —
        // and abort the whole loop — on every launch. After the cap, abandon it.
        let attempts = recovery::bump_attempts(data_dir, &pending);
        if attempts > MAX_RECOVERY_ATTEMPTS {
            recovery::discard(data_dir, &pending);
            continue;
        }
        // Isolate each pending so one corrupt session cannot block recovering the
        // rest (a panic here would otherwise unwind the entire loop).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            recover_one(shared, data_dir, &pending);
        }));
    }
    // Reap parts/attempts left by cleanly-exited or already-recovered generations.
    recovery::gc_orphans(data_dir, current_generation);
}

/// Build, filter, assemble and enqueue one pending session's crash report,
/// discarding its on-disk evidence only once it is safely queued.
fn recover_one(shared: &Shared, data_dir: &Path, pending: &recovery::PendingSession) {
    let span = export::report_span(&pending.parts_dir).ok().flatten();
    let now = epoch_ms();
    let (start, end) = span.unwrap_or((now, now));
    let mut built = recovery::build_report(pending, end);

    // Apply before_send to recovered crashes too (host redaction/drop).
    let keep = match &shared.before_send {
        Some(hook) => hook(&mut built.meta),
        None => true,
    };
    if !keep || queue::any_blacklisted(data_dir, &built.meta.signatures) {
        recovery::discard(data_dir, pending);
        return;
    }
    merge_scope(shared, &mut built.meta);

    let window = TimeWindow { start, end };
    let delivered = reporting::assemble_with_extras(
        &pending.parts_dir,
        window,
        &built.meta,
        &shared.env,
        Some(built.crash_json),
        &built.extra_files,
        &shared.app_token,
        end,
    )
    .and_then(|assembled| queue::enqueue(data_dir, &assembled))
    .is_ok();

    // Only destroy the crash evidence once it is safely queued.
    if delivered {
        recovery::discard(data_dir, pending);
    }
}

/// Merge ambient scope (email / labels / attributes) into a report's metadata.
fn merge_scope(shared: &Shared, meta: &mut ReportMeta) {
    let scope = lock_recover(&shared.scope);
    if meta.email.is_none() {
        meta.email = scope.email.clone();
    }
    for label in &scope.labels {
        if !meta.labels.contains(label) {
            meta.labels.push(label.clone());
        }
    }
    for (k, v) in &scope.attributes {
        meta.attrs.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    rx: Receiver<Msg>,
    mut store: PartStore,
    shared: Arc<Shared>,
    rotate_interval: Duration,
    data_dir: PathBuf,
    caps: WindowCaps,
    mut sampler: Option<Box<dyn TelemetrySampler>>,
    upload_tx: Sender<UploadMsg>,
) {
    // Rotation/eviction/telemetry must run on a wall-clock cadence, NOT only when
    // `recv_timeout` times out. Under sustained capture traffic every `recv`
    // returns `Ok` before the interval elapses, resetting the timer forever, so
    // the old "only on Timeout" placement starved the sliding window: parts never
    // rotated/evicted (bypassing `max_bytes`/`max_window`) and telemetry never
    // sampled (F17). Instead we recv with a timeout bounded by the next rotation
    // deadline and run the tick whenever that deadline passes, message or not.
    let mut next_rotate = Instant::now() + rotate_interval;
    loop {
        let timeout = next_rotate.saturating_duration_since(Instant::now());
        match rx.recv_timeout(timeout) {
            Ok(Msg::Capture(entry)) => {
                shared.queued.fetch_sub(1, Ordering::Relaxed);
                // A panic while serializing/appending must not kill the worker.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Ok(value) = entry.to_json() {
                        if let Ok(bytes) = serde_json::to_vec(&value) {
                            let _ = store.append(entry.wire_type(), entry.timestamp(), &bytes);
                        }
                    }
                }));
            }
            Ok(Msg::Report {
                meta,
                window_end,
                crash_json,
            }) => {
                shared.report_queued.fetch_sub(1, Ordering::Relaxed);
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    process_report(
                        &mut store, &shared, &data_dir, caps, meta, window_end, crash_json,
                    )
                    .is_ok()
                }))
                .unwrap_or(false);
                if ok {
                    let _ = upload_tx.send(UploadMsg::Wake);
                }
            }
            Ok(Msg::Snapshot { window_end, ack }) => {
                let handle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    take_snapshot(&mut store, &data_dir, caps, window_end)
                }))
                .ok()
                .flatten();
                let _ = ack.send(handle);
            }
            Ok(Msg::UploadSnapshot { handle, meta }) => {
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    deliver_snapshot(&shared, &data_dir, handle, meta)
                }))
                .unwrap_or(false);
                if ok {
                    let _ = upload_tx.send(UploadMsg::Wake);
                }
            }
            Ok(Msg::Flush(ack)) => {
                let _ = store.flush();
                let _ = ack.send(());
            }
            Ok(Msg::Stop(ack)) => {
                let _ = store.flush();
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => { /* fall through to the rotation tick */ }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Run the rotation/eviction/telemetry tick whenever the interval has
        // elapsed — via a message OR a timeout — so steady traffic can't starve it.
        if Instant::now() >= next_rotate {
            rotate_tick(&mut store, &shared, &mut sampler);
            next_rotate = Instant::now() + rotate_interval;
        }
    }
}

/// One rotation tick: sample telemetry (unless paused) then roll + evict the
/// window. Panic-contained so a host `TelemetrySampler` panic can't kill the
/// worker.
fn rotate_tick(
    store: &mut PartStore,
    shared: &Shared,
    sampler: &mut Option<Box<dyn TelemetrySampler>>,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Skip telemetry sampling while paused; rotation/eviction still runs so
        // the window keeps sliding.
        if !shared.paused.load(Ordering::Relaxed) {
            if let Some(sampler) = sampler.as_mut() {
                let ts = epoch_ms();
                for (name, value) in sampler.sample() {
                    let entry = TraceEntry {
                        timestamp: ts,
                        display_id: None,
                        name: Some(name),
                        value,
                        custom: Default::default(),
                    };
                    if let Ok(bytes) = serde_json::to_vec(&entry) {
                        let _ = store.append("traces.system", ts, &bytes);
                    }
                }
            }
        }
        let _ = store.rotate();
    }));
}

/// Snapshot the current window into a fresh report dir and return a handle.
fn take_snapshot(
    store: &mut PartStore,
    data_dir: &Path,
    caps: WindowCaps,
    window_end: i64,
) -> Option<SnapshotHandle> {
    let window_start = window_end.saturating_sub(caps.max_window_ms);
    let report_id = crate::util::random_hex(8);
    let dir = data_dir.join("reports").join(&report_id);
    std::fs::create_dir_all(&dir).ok()?;
    // Build the handle BEFORE snapshotting so a snapshot_into failure cleans up
    // the created dir via the handle's Drop (no orphan on the error path).
    let handle = SnapshotHandle {
        dir: dir.clone(),
        window: TimeWindow {
            start: window_start,
            end: window_end,
        },
    };
    store.snapshot_into(&dir, window_start, window_end).ok()?;
    Some(handle)
}

/// Assemble + enqueue a report from a pre-taken snapshot (deferred upload path).
fn deliver_snapshot(
    shared: &Shared,
    data_dir: &Path,
    handle: SnapshotHandle,
    mut meta: ReportMeta,
) -> bool {
    merge_scope(shared, &mut meta);
    if let Some(hook) = &shared.before_send {
        if !hook(&mut meta) {
            return false; // handle's Drop reclaims the snapshot dir
        }
    }
    // The snapshot dir is reclaimed by `handle`'s Drop on return (any path).
    reporting::assemble(
        &handle.dir,
        handle.window,
        &meta,
        &shared.env,
        None,
        &shared.app_token,
        handle.window.end,
    )
    .and_then(|assembled| queue::enqueue(data_dir, &assembled))
    .is_ok()
}

fn process_report(
    store: &mut PartStore,
    shared: &Shared,
    data_dir: &Path,
    caps: WindowCaps,
    mut meta: ReportMeta,
    window_end: i64,
    crash_json: Option<Vec<u8>>,
) -> std::io::Result<()> {
    merge_scope(shared, &mut meta);

    // before_send: let the host mutate or drop the report before any disk work.
    if let Some(hook) = &shared.before_send {
        if !hook(&mut meta) {
            return Ok(());
        }
    }

    // Sampling: drop a fraction of non-fatal reports (never crashes).
    if meta.issue_type == IssueType::Error
        && shared.sample_rate < 1.0
        && crate::util::random_unit_f64() >= shared.sample_rate
    {
        return Ok(());
    }

    // Suppress reports the server blacklisted (12004 TooManySimilar).
    if queue::any_blacklisted(data_dir, &meta.signatures) {
        return Ok(());
    }

    let window_start = window_end.saturating_sub(caps.max_window_ms);
    let report_id = crate::util::random_hex(8);
    let report_dir = data_dir.join("reports").join(&report_id);
    std::fs::create_dir_all(&report_dir)?;

    // Own the report dir with a `SnapshotHandle` so EVERY exit path reclaims its
    // hard links via Drop. The earlier manual `remove_dir_all` ran only after a
    // successful assemble, so a `snapshot_into`/`assemble` failure (most plausibly
    // disk-full) permanently orphaned a `reports/<id>` dir full of hard-linked
    // parts, and `reports/` was never GC'd (F7).
    let window = TimeWindow {
        start: window_start,
        end: window_end,
    };
    let handle = SnapshotHandle {
        dir: report_dir,
        window,
    };

    store.snapshot_into(&handle.dir, window_start, window_end)?;
    let assembled = reporting::assemble(
        &handle.dir,
        window,
        &meta,
        &shared.env,
        crash_json,
        &shared.app_token,
        window_end,
    )?;

    // Persist to the durable queue; the uploader delivers it. `handle`'s Drop
    // reclaims the snapshot dir on return (success or the `?` paths above).
    queue::enqueue(data_dir, &assembled)
}

fn uploader_loop(
    rx: Receiver<UploadMsg>,
    shared: Arc<Shared>,
    data_dir: PathBuf,
    backoff_base: Duration,
) {
    let poll = Duration::from_secs(5);
    // A fresh launch retries any queued report immediately (force), ignoring
    // backoff scheduled by the prior session. Guarded so a panicking Transport
    // impl can't kill the uploader.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drain(&shared, &data_dir, backoff_base, true)
    }));
    loop {
        match rx.recv_timeout(poll) {
            Ok(UploadMsg::Wake) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain(&shared, &data_dir, backoff_base, false)
                }));
            }
            Ok(UploadMsg::Drain { deadline, ack }) => {
                let empty = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_until_empty(&shared, &data_dir, backoff_base, deadline)
                }))
                .unwrap_or(false);
                let _ = ack.send(empty);
            }
            Ok(UploadMsg::Stop(ack)) => {
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain(&shared, &data_dir, backoff_base, false)
                }));
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Attempt delivery of every due queued report once. On the flush path `force`
/// is set so backing-off reports are retried immediately. Returns whether any
/// report was attempted (used to pace `drain_until_empty`).
fn drain(shared: &Shared, data_dir: &Path, backoff_base: Duration, force: bool) -> bool {
    let now = epoch_ms();
    let mut attempted = false;
    for report in queue::list_pending(data_dir) {
        let Ok((zip, request_json, retry, next_attempt)) = queue::load(&report) else {
            continue;
        };
        if !force && now < next_attempt {
            continue; // still backing off
        }
        attempted = true;
        // Scheduled/startup attempts are backoff-paced, so they count toward the
        // durable retry cap.
        deliver_queued(
            shared,
            data_dir,
            backoff_base,
            &report,
            zip,
            request_json,
            retry,
            now,
            true,
        );
    }
    attempted
}

/// Deliver one already-loaded queued report, updating its durable retry/backoff
/// state (or removing it) per the transport outcome.
///
/// `count_failure` controls whether a transient failure advances the durable
/// retry counter toward the [`UPLOAD_RETRY_CAP`] abandon-and-delete threshold.
/// It is `true` on the backoff-paced drain path (each attempt is a real, spaced
/// try) and `false` on a forced flush, which may re-attempt a report many times
/// before its deadline: counting there would race the counter to the cap in ~1 s
/// and permanently delete a still-deliverable crash bundle (F13). A forced flush
/// leaves the counter untouched — the scheduled path still abandons a genuinely
/// dead report over its normal backoff schedule.
#[allow(clippy::too_many_arguments)]
fn deliver_queued(
    shared: &Shared,
    data_dir: &Path,
    backoff_base: Duration,
    report: &queue::QueuedReport,
    zip: Vec<u8>,
    request_json: Vec<u8>,
    retry: u32,
    now: i64,
    count_failure: bool,
) {
    let assembled = AssembledReport {
        bundle_name: queue::bundle_name(report),
        zip,
        request_json,
    };
    // Resume at the PUT if a prior attempt created the issue but the PUT failed
    // (F14): re-POSTing create_issue mints a duplicate issue, or trips 12003 and
    // deletes the bundle unuploaded.
    let cached_endpoint = queue::endpoint(report);
    let mut endpoint_out: Option<String> = None;
    match transport::deliver(
        &*shared.transport,
        &shared.app_token,
        &shared.environment_json,
        &shared.session,
        &assembled,
        cached_endpoint.as_deref(),
        &mut endpoint_out,
    ) {
        Ok(()) => queue::remove(report),
        // A transient failure OR a still-expired session are both retryable.
        Err(TransportError::Transient(_)) | Err(TransportError::SessionExpired) => {
            // Persist the presigned endpoint if the failure left one to resume
            // from; otherwise drop any stale cached endpoint (F14).
            match endpoint_out {
                Some(ep) => queue::set_endpoint(report, &ep),
                None => queue::clear_endpoint(report),
            }
            if count_failure {
                let n = retry + 1;
                if n >= UPLOAD_RETRY_CAP {
                    queue::remove(report);
                    notify_dropped(shared, DropReason::RetriesExhausted);
                } else {
                    queue::set_meta(report, n, now + backoff_delay(n, backoff_base));
                }
            }
            // else (forced flush): leave retry/backoff untouched — the report
            // stays queued to be re-attempted within this flush and, if still
            // failing, on the next scheduled drain / launch.
        }
        Err(TransportError::TooManySimilar { signatures }) => {
            // Blacklist so future identical crashes aren't re-uploaded.
            let sigs = if signatures.is_empty() {
                queue::signatures_of(&assembled)
            } else {
                signatures
            };
            queue::blacklist_add(data_dir, &sigs);
            queue::remove(report);
            notify_dropped(shared, DropReason::TooManySimilar);
        }
        // Duplicate / permanent — abandon the report.
        Err(e) => {
            queue::remove(report);
            notify_dropped(shared, drop_reason_for(e));
        }
    }
}

/// Classify a terminal transport error for [`DropReason`].
fn drop_reason_for(err: TransportError) -> DropReason {
    match err {
        TransportError::DuplicateDropped => DropReason::Duplicate,
        // The server's own words: the report is being deleted, so this text is
        // the only record of why it never arrived.
        TransportError::Permanent(detail) => DropReason::Rejected(detail),
        other => DropReason::Rejected(format!("{other:?}")),
    }
}

/// Tell the host a report was abandoned, if it asked to know.
fn notify_dropped(shared: &Shared, reason: DropReason) {
    if let Some(hook) = &shared.on_report_dropped {
        hook(reason);
    }
}

/// Exponential backoff: `base * 2^(retry-1)`, capped at 300 s.
fn backoff_delay(retry: u32, base: Duration) -> i64 {
    let shift = retry.saturating_sub(1).min(20);
    let ms = (base.as_millis() as u64)
        .saturating_mul(1u64 << shift)
        .min(300_000);
    ms as i64
}

/// Drain until the queue is empty or `deadline` elapses, ignoring backoff so a
/// report in its backoff window is still attempted on an explicit flush. Forced
/// attempts do NOT count toward the durable retry cap (`count_failure = false`),
/// so a fast-failing offline flush re-attempts a report until the deadline
/// without ever racing its counter to the cap and deleting it (F13); a
/// flaky-but-recovering transport still delivers within the flush. Returns
/// whether the queue ended up empty.
fn drain_until_empty(
    shared: &Shared,
    data_dir: &Path,
    backoff_base: Duration,
    deadline: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let now = epoch_ms();
        for report in queue::list_pending(data_dir) {
            let Ok((zip, request_json, retry, _next)) = queue::load(&report) else {
                continue;
            };
            deliver_queued(
                shared,
                data_dir,
                backoff_base,
                &report,
                zip,
                request_json,
                retry,
                now,
                false,
            );
            if start.elapsed() >= deadline {
                break;
            }
        }
        if queue::list_pending(data_dir).is_empty() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::enums::BreadcrumbLevel;
    use crate::transport::MockTransport;

    fn tmp_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bugsee-bc-{}", crate::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn crumb(msg: &str) -> Breadcrumb {
        Breadcrumb {
            timestamp: 0,
            crumb_type: None,
            category: None,
            level: BreadcrumbLevel::Info,
            message: Some(msg.to_string()),
            data: None,
        }
    }

    #[test]
    fn before_breadcrumb_drops_and_mutates() {
        let dir = tmp_dir();
        let transport = Arc::new(MockTransport::default());
        let mut config = RecorderConfig::new(&dir, "T");
        config.before_breadcrumb = Some(Box::new(|mut b: Breadcrumb| match b.message.as_deref() {
            Some("drop") => None,
            _ => {
                b.category = Some("mutated".into());
                Some(b)
            }
        }));
        let recorder = Recorder::launch(config, transport).unwrap();

        // A hook returning None drops the breadcrumb.
        assert!(recorder.before_breadcrumb(crumb("drop")).is_none());
        // A kept breadcrumb carries the hook's mutation.
        let kept = recorder.before_breadcrumb(crumb("keep")).expect("kept");
        assert_eq!(kept.category.as_deref(), Some("mutated"));

        drop(recorder);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn before_breadcrumb_passthrough_without_hook() {
        let dir = tmp_dir();
        let transport = Arc::new(MockTransport::default());
        let recorder = Recorder::launch(RecorderConfig::new(&dir, "T"), transport).unwrap();
        // No hook configured: the breadcrumb passes through unchanged.
        assert!(recorder.before_breadcrumb(crumb("x")).is_some());
        drop(recorder);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

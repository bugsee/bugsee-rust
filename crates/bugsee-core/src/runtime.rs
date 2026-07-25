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
//!   silently kill capture/delivery for the process lifetime.
//! - The producer→worker channel is bounded by an in-flight counter (drop-newest
//!   under back-pressure) so a burst or a stalled worker cannot grow RSS without
//!   limit.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{export, PartStore, WindowCaps};
use crate::model::entry::{CaptureEntry, TraceEntry};
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
/// Default in-flight capture-entry cap (drop-newest beyond this).
const DEFAULT_MAX_QUEUED: usize = 8192;

/// A callback run on every report before delivery. Mutate the metadata in place;
/// return `false` to drop the report entirely.
pub type BeforeSend = Box<dyn Fn(&mut ReportMeta) -> bool + Send + Sync>;

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
    /// Base delay for upload retry backoff (doubles per attempt, capped at 300 s).
    pub upload_backoff_base: Duration,
    /// Optional callback to mutate or drop reports before delivery.
    pub before_send: Option<BeforeSend>,
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
            upload_backoff_base: Duration::from_secs(30),
            before_send: None,
            sample_rate: 1.0,
            max_queued_entries: DEFAULT_MAX_QUEUED,
        }
    }
}

/// A handle to a report snapshot taken at `create_report` time. The window is
/// hard-linked on disk immediately, so later eviction cannot erode it.
pub struct SnapshotHandle {
    dir: PathBuf,
    window: TimeWindow,
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
    sample_rate: f64,
    /// In-flight capture entries queued to the worker (back-pressure counter).
    queued: AtomicUsize,
    max_queued: usize,
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
    /// Force-drain and report whether the queue is empty afterward.
    Drain(Sender<bool>),
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
        let env = Environment::detect(&config.sdk_version);
        let environment_json = serde_json::to_vec(&env).unwrap_or_default();
        let shared = Arc::new(Shared {
            scope: Mutex::new(Scope::default()),
            env,
            environment_json,
            app_token: config.app_token.clone(),
            session: Mutex::new(None),
            transport,
            before_send: config.before_send,
            sample_rate: config.sample_rate,
            queued: AtomicUsize::new(0),
            max_queued: config.max_queued_entries.max(1),
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
            .spawn(move || uploader_loop(upload_rx, uploader_shared, uploader_data_dir, backoff_base))?;

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
                run_recovery(&worker_shared, &worker_data_dir, generation);
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
        // Bounded back-pressure: drop rather than grow the in-flight queue.
        if self.shared.queued.load(Ordering::Relaxed) >= self.shared.max_queued {
            return;
        }
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        if self.tx.send(Msg::Capture(entry)).is_err() {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
        }
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
        let _ = self.tx.send(Msg::Report {
            meta,
            window_end,
            crash_json,
        });
    }

    /// Snapshot the window now (hard-links on disk), returning a handle to
    /// upload later. Blocks briefly on the worker; returns `None` on failure.
    pub fn create_snapshot(&self, window_end: i64) -> Option<SnapshotHandle> {
        let (ack, rx) = channel();
        if self.tx.send(Msg::Snapshot { window_end, ack }).is_err() {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(5)).ok().flatten()
    }

    /// Deliver a report from a snapshot handle produced by [`create_snapshot`].
    pub fn upload_snapshot(&self, handle: SnapshotHandle, meta: ReportMeta) {
        let _ = self.tx.send(Msg::UploadSnapshot { handle, meta });
    }

    /// Discard a snapshot handle without uploading (removes its on-disk links).
    pub fn discard_snapshot(handle: SnapshotHandle) {
        let _ = std::fs::remove_dir_all(&handle.dir);
    }

    /// Block until captured work is persisted and the queue is fully drained, or
    /// `timeout` elapses. Returns `true` only if the outbound queue is empty.
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let (tx, rx) = channel();
        if self.tx.send(Msg::Flush(tx)).is_err() {
            return false;
        }
        if rx.recv_timeout(timeout).is_err() {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (utx, urx) = channel();
        if self.upload_tx.send(UploadMsg::Drain(utx)).is_err() {
            return false;
        }
        // `true` means the queue actually drained, not merely that draining ran.
        urx.recv_timeout(remaining.max(Duration::from_millis(1)))
            .unwrap_or(false)
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
        // Signal both threads to stop, then join. (Called only after the global
        // lock has been released — see the facade's `stop`.)
        let (tx, rx) = channel();
        if self.tx.send(Msg::Stop(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(2));
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
        let (utx, urx) = channel();
        if self.upload_tx.send(UploadMsg::Stop(utx)).is_ok() {
            let _ = urx.recv_timeout(Duration::from_secs(2));
        }
        if let Some(handle) = self.uploader.take() {
            let _ = handle.join();
        }
        // Clean shutdown: drop the liveness marker so next launch does not treat
        // this session as an abnormal exit.
        self.session.end();
    }
}

/// Recover any prior generation that ended abnormally, queueing it for delivery.
/// The crashed session's state is discarded ONLY after the report is durably
/// enqueued — a failure leaves it on disk for a later attempt (no crash loss).
fn run_recovery(shared: &Shared, data_dir: &Path, current_generation: u64) {
    for pending in recovery::find_pending(data_dir, current_generation) {
        let span = export::report_span(&pending.parts_dir).ok().flatten();
        let now = epoch_ms();
        let (start, end) = span.unwrap_or((now, now));
        let mut built = recovery::build_report(&pending, end);

        // Apply before_send to recovered crashes too (host redaction/drop).
        let keep = match &shared.before_send {
            Some(hook) => hook(&mut built.meta),
            None => true,
        };
        if !keep || queue::any_blacklisted(data_dir, &built.meta.signatures) {
            recovery::discard(data_dir, &pending);
            continue;
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
            recovery::discard(data_dir, &pending);
        }
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
    loop {
        match rx.recv_timeout(rotate_interval) {
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
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    process_report(&mut store, &shared, &data_dir, caps, meta, window_end, crash_json)
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
            Err(RecvTimeoutError::Timeout) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
                    let _ = store.rotate();
                }));
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
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
    store.snapshot_into(&dir, window_start, window_end).ok()?;
    Some(SnapshotHandle {
        dir,
        window: TimeWindow {
            start: window_start,
            end: window_end,
        },
    })
}

/// Assemble + enqueue a report from a pre-taken snapshot (deferred upload path).
fn deliver_snapshot(shared: &Shared, data_dir: &Path, handle: SnapshotHandle, mut meta: ReportMeta) -> bool {
    merge_scope(shared, &mut meta);
    if let Some(hook) = &shared.before_send {
        if !hook(&mut meta) {
            let _ = std::fs::remove_dir_all(&handle.dir);
            return false;
        }
    }
    let result = reporting::assemble(
        &handle.dir,
        handle.window,
        &meta,
        &shared.env,
        None,
        &shared.app_token,
        handle.window.end,
    )
    .and_then(|assembled| queue::enqueue(data_dir, &assembled));
    let _ = std::fs::remove_dir_all(&handle.dir);
    result.is_ok()
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

    store.snapshot_into(&report_dir, window_start, window_end)?;

    let window = TimeWindow {
        start: window_start,
        end: window_end,
    };
    let assembled = reporting::assemble(
        &report_dir,
        window,
        &meta,
        &shared.env,
        crash_json,
        &shared.app_token,
        window_end,
    )?;

    // Persist to the durable queue; the uploader delivers it.
    let enqueue_result = queue::enqueue(data_dir, &assembled);
    let _ = std::fs::remove_dir_all(&report_dir);
    enqueue_result
}

fn uploader_loop(rx: Receiver<UploadMsg>, shared: Arc<Shared>, data_dir: PathBuf, backoff_base: Duration) {
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
            Ok(UploadMsg::Drain(ack)) => {
                let empty = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_until_empty(&shared, &data_dir, backoff_base, Duration::from_secs(5))
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
        let assembled = AssembledReport {
            bundle_name: queue::bundle_name(&report),
            zip,
            request_json,
        };
        match transport::deliver(
            &*shared.transport,
            &shared.app_token,
            &shared.environment_json,
            &shared.session,
            &assembled,
        ) {
            Ok(()) => queue::remove(&report),
            // A transient failure OR a still-expired session are both retryable.
            Err(TransportError::Transient(_)) | Err(TransportError::SessionExpired) => {
                let n = retry + 1;
                if n >= UPLOAD_RETRY_CAP {
                    queue::remove(&report);
                } else {
                    queue::set_meta(&report, n, now + backoff_delay(n, backoff_base));
                }
            }
            Err(TransportError::TooManySimilar { signatures }) => {
                // Blacklist so future identical crashes aren't re-uploaded.
                let sigs = if signatures.is_empty() {
                    queue::signatures_of(&assembled)
                } else {
                    signatures
                };
                queue::blacklist_add(data_dir, &sigs);
                queue::remove(&report);
            }
            // Duplicate / permanent — abandon the report.
            Err(_) => queue::remove(&report),
        }
    }
    attempted
}

/// Exponential backoff: `base * 2^(retry-1)`, capped at 300 s.
fn backoff_delay(retry: u32, base: Duration) -> i64 {
    let shift = retry.saturating_sub(1).min(20);
    let ms = (base.as_millis() as u64)
        .saturating_mul(1u64 << shift)
        .min(300_000);
    ms as i64
}

/// Drain until the queue is empty or no report is deliverable before `deadline`.
/// Returns whether the queue ended up empty.
fn drain_until_empty(shared: &Shared, data_dir: &Path, backoff_base: Duration, deadline: Duration) -> bool {
    let start = Instant::now();
    loop {
        // Force so a report in backoff is still attempted on an explicit flush.
        drain(shared, data_dir, backoff_base, true);
        let pending = queue::list_pending(data_dir);
        if pending.is_empty() {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

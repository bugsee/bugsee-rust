//
//  runtime.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The capture runtime: a capture worker that owns the part store and drains
//! entries, plus an uploader thread that delivers reports from the durable
//! on-disk queue with retry. Producers only enqueue; all disk and network work
//! happens off the caller's thread. A report is assembled and persisted to the
//! queue, then delivered by the uploader — so delivery survives failures and
//! restarts.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{export, PartStore, WindowCaps};
use crate::model::entry::{CaptureEntry, TraceEntry};
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
        }
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
}

enum Msg {
    Capture(CaptureEntry),
    Report {
        meta: ReportMeta,
        window_end: i64,
        crash_json: Option<Vec<u8>>,
    },
    Flush(Sender<()>),
    Stop(Sender<()>),
}

enum UploadMsg {
    Wake,
    Drain(Sender<()>),
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

    /// Enqueue an entry. Never blocks; drops silently if the worker is gone.
    pub fn capture(&self, entry: CaptureEntry) {
        let _ = self.tx.send(Msg::Capture(entry));
    }

    /// Mutate the ambient scope (email / labels / attributes).
    pub fn with_scope<R>(&self, f: impl FnOnce(&mut Scope) -> R) -> R {
        let mut guard = self.shared.scope.lock().unwrap();
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

    /// Block until captured work is persisted and the queue has been drained
    /// once, or `timeout` elapses.
    pub fn flush(&self, timeout: Duration) -> bool {
        // First ensure the capture worker has processed everything (reports are
        // assembled + queued), then drain the uploader.
        let (tx, rx) = channel();
        if self.tx.send(Msg::Flush(tx)).is_err() {
            return false;
        }
        if rx.recv_timeout(timeout).is_err() {
            return false;
        }
        let (utx, urx) = channel();
        if self.upload_tx.send(UploadMsg::Drain(utx)).is_err() {
            return false;
        }
        urx.recv_timeout(timeout).is_ok()
    }

    /// Data directory root for this recorder.
    pub fn data_dir(&self) -> &std::path::Path {
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
fn run_recovery(shared: &Shared, data_dir: &std::path::Path, current_generation: u64) {
    for pending in recovery::find_pending(data_dir, current_generation) {
        let span = export::report_span(&pending.parts_dir).ok().flatten();
        let now = epoch_ms();
        let (start, end) = span.unwrap_or((now, now));
        let built = recovery::build_report(&pending, end);
        let window = TimeWindow { start, end };
        if let Ok(assembled) = reporting::assemble_with_extras(
            &pending.parts_dir,
            window,
            &built.meta,
            &shared.env,
            Some(built.crash_json),
            &built.extra_files,
            &shared.app_token,
            end,
        ) {
            let _ = queue::enqueue(data_dir, &assembled);
        }
        recovery::discard(data_dir, &pending);
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
                if let Ok(value) = entry.to_json() {
                    if let Ok(bytes) = serde_json::to_vec(&value) {
                        let _ = store.append(entry.wire_type(), entry.timestamp(), &bytes);
                    }
                }
            }
            Ok(Msg::Report {
                meta,
                window_end,
                crash_json,
            }) => {
                if process_report(&mut store, &shared, &data_dir, caps, meta, window_end, crash_json)
                    .is_ok()
                {
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
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn process_report(
    store: &mut PartStore,
    shared: &Shared,
    data_dir: &std::path::Path,
    caps: WindowCaps,
    mut meta: ReportMeta,
    window_end: i64,
    crash_json: Option<Vec<u8>>,
) -> std::io::Result<()> {
    // Merge ambient scope into the report metadata.
    {
        let scope = shared.scope.lock().unwrap();
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

fn uploader_loop(
    rx: Receiver<UploadMsg>,
    shared: Arc<Shared>,
    data_dir: PathBuf,
    backoff_base: Duration,
) {
    let poll = Duration::from_secs(5);
    // A fresh launch retries any queued report immediately (force), ignoring
    // backoff scheduled by the prior session.
    drain(&shared, &data_dir, backoff_base, true);
    loop {
        match rx.recv_timeout(poll) {
            Ok(UploadMsg::Wake) => {
                drain(&shared, &data_dir, backoff_base, false);
            }
            Ok(UploadMsg::Drain(ack)) => {
                drain_until_empty(&shared, &data_dir, backoff_base, Duration::from_secs(5));
                let _ = ack.send(());
            }
            Ok(UploadMsg::Stop(ack)) => {
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                drain(&shared, &data_dir, backoff_base, false);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Attempt delivery of every due queued report once. Returns whether any report
/// was attempted (used to pace `drain_until_empty`).
fn drain(shared: &Shared, data_dir: &std::path::Path, backoff_base: Duration, force: bool) -> bool {
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
            Err(TransportError::Transient(_)) => {
                let n = retry + 1;
                if n >= UPLOAD_RETRY_CAP {
                    queue::remove(&report);
                } else {
                    queue::set_meta(&report, n, now + backoff_delay(n, backoff_base));
                }
            }
            // Duplicate / permanent / blacklisted — abandon the report.
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

/// Drain until the queue is empty or no report is deliverable before `deadline`,
/// sleeping until the soonest scheduled retry between passes.
fn drain_until_empty(
    shared: &Shared,
    data_dir: &std::path::Path,
    backoff_base: Duration,
    deadline: Duration,
) {
    let start = Instant::now();
    loop {
        drain(shared, data_dir, backoff_base, false);
        let pending = queue::list_pending(data_dir);
        if pending.is_empty() {
            break;
        }
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            break;
        }
        let now = epoch_ms();
        let soonest = pending.iter().map(queue::next_attempt).min().unwrap_or(now);
        let wait_ms = (soonest - now).max(0) as u64;
        let remaining_ms = (deadline - elapsed).as_millis() as u64;
        if wait_ms > remaining_ms {
            break; // next retry is beyond the deadline
        }
        std::thread::sleep(Duration::from_millis(wait_ms.clamp(5, remaining_ms.max(5))));
    }
}

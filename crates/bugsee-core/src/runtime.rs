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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::capture::{export, PartStore, WindowCaps};
use crate::fork::{ForkLock, ForkLockable};
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

/// Where a recorder keeps its state.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// The data directory is usable: capture parts, the liveness marker and the
    /// retry queue are on disk, so a crash can be recovered on the next launch.
    Disk,
    /// No usable data directory (read-only, missing, full, ...). Handled errors
    /// and manual reports are still delivered, from memory, but there is no
    /// capture window and nothing survives the process: fatal crashes cannot be
    /// recovered and an unsent report is lost on exit.
    Memory,
}

/// Most reports held in memory awaiting delivery. Reports are rare and a bundle
/// is small, so this only bounds a pathological storm while offline.
const MAX_MEMORY_REPORTS: usize = 16;

/// A report waiting in memory for delivery, because it could not be queued on disk.
struct MemReport {
    /// Identifies the entry across the unlocked delivery attempt.
    id: u64,
    report: AssembledReport,
    retry: u32,
    next_attempt_ms: i64,
    /// Presigned upload endpoint cached after a failed PUT, so a retry resumes at
    /// the PUT instead of minting a duplicate issue (mirrors the disk queue).
    endpoint: Option<String>,
}

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
    /// The report could not be stored on disk and the in-memory backlog was
    /// full, so it was dropped rather than queued. Only seen with an unusable
    /// data directory.
    StorageUnavailable,
    /// Suppressed by the [`RateLimiter`](crate::ratelimit::RateLimiter) before
    /// it was assembled (a noisy error loop). Fires once per suppressed report
    /// from the capture worker thread, so keep the hook cheap. The running
    /// count is attached to the next admitted non-crash report as the
    /// `bugsee.reports_suppressed` attribute.
    RateLimited,
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
    /// PII scrubber applied to every report at assembly. On by default.
    pub scrub: crate::scrub::ScrubConfig,
    /// Report-volume limits applied before the durable queue. On by default.
    pub rate_limit: crate::ratelimit::RateLimitConfig,
    /// Max in-flight capture entries before new ones are dropped (back-pressure).
    pub max_queued_entries: usize,
    /// `Some(reason)` skips the disk entirely and runs in [`StorageMode::Memory`].
    /// For a host layer that already established no data directory is usable;
    /// the reason is logged and reported by [`Recorder::storage_reason`].
    pub memory_only: Option<String>,
    /// A previous default data directory to drain once at launch: recover its
    /// crashed sessions and deliver its queue. For a host layer that moved its
    /// default location and must not strand reports left at the old one.
    pub legacy_data_dir: Option<PathBuf>,
    /// More existing directories to recover from and drain once, like
    /// `legacy_data_dir`: trusted trees that could not take new files (a full or
    /// read-only default) even though a later default was writable.
    pub extra_data_dirs: Vec<PathBuf>,
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
            scrub: Default::default(),
            rate_limit: Default::default(),
            max_queued_entries: DEFAULT_MAX_QUEUED,
            memory_only: None,
            legacy_data_dir: None,
            extra_data_dirs: Vec::new(),
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
    scope: ForkLock<Scope>,
    env: Environment,
    environment_json: Vec<u8>,
    app_token: String,
    session: ForkLock<Option<String>>,
    transport: Arc<dyn Transport>,
    before_send: Option<BeforeSend>,
    on_report_dropped: Option<OnReportDropped>,
    before_breadcrumb: Option<BeforeBreadcrumb>,
    sample_rate: f64,
    scrubber: Option<crate::scrub::Scrubber>,
    limiter: crate::ratelimit::RateLimiter,
    /// In-flight capture entries queued to the worker (back-pressure counter).
    queued: AtomicUsize,
    max_queued: usize,
    /// In-flight report messages queued to the worker (heavier than captures, so
    /// bounded separately; drop-newest non-crash reports over the cap).
    report_queued: AtomicUsize,
    max_report_queued: usize,
    /// Whether telemetry sampling is paused (mirrors the facade's pause state).
    paused: AtomicBool,
    /// Reports that could not be queued on disk, delivered from memory. An entry
    /// stays here until its delivery is settled, so the length is the real backlog.
    mem_queue: ForkLock<Vec<MemReport>>,
    next_mem_id: AtomicU64,
    /// Crash signatures the server refused as too similar, remembered in memory
    /// (the on-disk `.blacklist` is unavailable or unreadable with no usable disk).
    mem_blacklist: ForkLock<BTreeSet<String>>,
    /// Queued bundles that were delivered but could not be deleted (read-only or
    /// full directory). Skipped from then on, or every drain would send them again.
    delivered_stuck: ForkLock<BTreeSet<PathBuf>>,
    /// Set the first time a disk write fails while running in disk mode: the
    /// recorder is no longer fully durable (see [`Recorder::is_degraded`]).
    degraded: AtomicBool,
}

/// The recorder's own locks join the `fork()` lock protocol, so a forked child
/// never inherits the scope or the cached access token locked by a thread (the
/// capture worker merges the scope into every report) that did not survive.
impl ForkLockable for Shared {
    fn fork_lock(&self) {
        self.scope.fork_lock();
        self.session.fork_lock();
        self.mem_queue.fork_lock();
        self.mem_blacklist.fork_lock();
        self.delivered_stuck.fork_lock();
        self.limiter.fork_lock();
    }
    fn fork_unlock(&self) {
        self.limiter.fork_unlock();
        self.delivered_stuck.fork_unlock();
        self.mem_blacklist.fork_unlock();
        self.mem_queue.fork_unlock();
        self.session.fork_unlock();
        self.scope.fork_unlock();
    }
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
    /// `None` in memory mode: there is no liveness marker to own.
    session: Option<Session>,
    storage: StorageMode,
    storage_reason: Option<String>,
    rotate_interval: Duration,
    backoff_base: Duration,
    worker: Option<JoinHandle<()>>,
    uploader: Option<JoinHandle<()>>,
}

/// The two pipeline threads and the channels that feed them.
struct Pipeline {
    tx: Sender<Msg>,
    upload_tx: Sender<UploadMsg>,
    worker: JoinHandle<()>,
    uploader: JoinHandle<()>,
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
        let scrubber = config
            .scrub
            .enabled
            .then(|| crate::scrub::Scrubber::new(config.scrub.clone()));
        // The session-registration body carries the environment too (paths,
        // host facts), so it is scrubbed like every report's request.json.
        let mut environment_json = serde_json::to_vec(&env).unwrap_or_default();
        if let Some(clean) = scrubber
            .as_ref()
            .and_then(|s| s.scrub_json_bytes(&environment_json))
        {
            environment_json = clean;
        }
        let shared = Arc::new(Shared {
            scope: ForkLock::new(Scope::default()),
            env,
            environment_json,
            app_token: config.app_token.clone(),
            session: ForkLock::new(None),
            transport,
            before_send: config.before_send,
            on_report_dropped: config.on_report_dropped,
            before_breadcrumb: config.before_breadcrumb,
            sample_rate: config.sample_rate,
            scrubber,
            limiter: crate::ratelimit::RateLimiter::new(config.rate_limit),
            queued: AtomicUsize::new(0),
            max_queued: config.max_queued_entries.max(1),
            report_queued: AtomicUsize::new(0),
            max_report_queued: DEFAULT_MAX_REPORT_QUEUED,
            paused: AtomicBool::new(false),
            mem_queue: ForkLock::new(Vec::new()),
            next_mem_id: AtomicU64::new(0),
            mem_blacklist: ForkLock::new(BTreeSet::new()),
            delivered_stuck: ForkLock::new(BTreeSet::new()),
            degraded: AtomicBool::new(false),
        });

        crate::fork::install_fork_hook();
        crate::fork::register_weak(&shared);

        let data_dir = config.data_dir.clone();
        // An unusable data directory degrades to memory mode instead of failing
        // the launch: the host must not lose error reporting (or its startup)
        // because a volume is read-only or full.
        let (disk, storage_reason) = if let Some(reason) = config.memory_only.clone() {
            (None, Some(reason))
        } else {
            match open_disk(&data_dir, config.caps) {
                Ok(disk) => (Some(disk), None),
                Err(e) => (None, Some(format!("{}: {e}", data_dir.display()))),
            }
        };
        if let Some(reason) = &storage_reason {
            static WARNED: AtomicBool = AtomicBool::new(false);
            crate::util::warn_once(
                &WARNED,
                &format!(
                    "data directory unusable ({reason}); running in memory only: handled \
                     errors and manual reports are still sent, but crashes cannot be \
                     recovered and nothing survives this process"
                ),
            );
        }
        let (session, store) = match disk {
            Some((session, store)) => (Some(session), Some(store)),
            None => (None, None),
        };
        // A directory that cannot take NEW files may still hold reports and crashes
        // a previous run persisted (that is what a full disk looks like), and
        // recovering them only reads and deletes. So an existing directory is
        // still drained and recovered from in memory mode.
        let drain_dir = (session.is_some() || data_dir.is_dir()).then_some(data_dir.as_path());
        let pipeline = spawn_pipeline(
            &shared,
            store.is_some().then_some(data_dir.as_path()),
            drain_dir,
            session.as_ref().map_or(u64::MAX, Session::generation),
            store,
            config.caps,
            config.rotate_interval,
            config.upload_backoff_base,
            config.sampler,
            true,
            distinct_extra_dirs(
                &data_dir,
                config
                    .legacy_data_dir
                    .into_iter()
                    .chain(config.extra_data_dirs)
                    .collect(),
            ),
        )?;

        Ok(Recorder {
            tx: pipeline.tx,
            upload_tx: pipeline.upload_tx,
            shared,
            caps: config.caps,
            data_dir,
            storage: if session.is_some() {
                StorageMode::Disk
            } else {
                StorageMode::Memory
            },
            storage_reason,
            session,
            rotate_interval: config.rotate_interval,
            backoff_base: config.upload_backoff_base,
            worker: Some(pipeline.worker),
            uploader: Some(pipeline.uploader),
        })
    }

    /// Bring a recorder back to life in a **fork child**.
    ///
    /// `fork()` copies this struct but not its threads: the capture worker and
    /// the uploader do not exist in the child, so every channel send would
    /// succeed into a queue nobody reads and the child would silently stop
    /// reporting. This abandons the dead pipeline (never joined — there is
    /// nothing to join) and starts a fresh one, keeping everything that makes the
    /// recorder *this app's* recorder: hooks, scope, transport, environment.
    ///
    /// The child gets its **own session generation and liveness marker**, so a
    /// crash in the child is recovered as the child's, and it never touches the
    /// parent's capture parts. It does recover sessions that died after the parent
    /// launched (a sibling worker that crashed), guarded by claims so siblings do
    /// not both do it; it does not force-drain the shared queue (the parent's
    /// uploader is already on it) or reap queue sidecars (the parent may be
    /// mid-enqueue).
    ///
    /// `sampler` replaces the telemetry sampler, which died with the worker.
    pub fn revive_after_fork(
        &mut self,
        sampler: Option<Box<dyn TelemetrySampler>>,
    ) -> std::io::Result<()> {
        // The parent's threads are not ours to join; drop their handles without
        // running anything.
        std::mem::forget(self.worker.take());
        std::mem::forget(self.uploader.take());

        // Entries queued to the dead worker will never be processed, so the
        // back-pressure counters they incremented would stay high forever and
        // eventually drop everything.
        self.shared.queued.store(0, Ordering::SeqCst);
        self.shared.report_queued.store(0, Ordering::SeqCst);

        // The parent's unsent in-memory reports are the PARENT's to send; the child
        // inheriting them would deliver every one a second time.
        self.shared.mem_queue.lock().clear();
        self.shared.mem_blacklist.lock().clear();
        self.shared.delivered_stuck.lock().clear();

        // A child that cannot open its own session degrades to memory rather than
        // failing: the parent's disk may be gone or read-only by now.
        let disk = match self.storage {
            StorageMode::Disk => match open_disk(&self.data_dir, self.caps) {
                Ok(disk) => Some(disk),
                Err(e) => {
                    self.storage = StorageMode::Memory;
                    self.storage_reason = Some(format!("{}: {e}", self.data_dir.display()));
                    None
                }
            },
            StorageMode::Memory => None,
        };
        let (session, store) = match disk {
            Some((session, store)) => (Some(session), Some(store)),
            None => (None, None),
        };
        let pipeline = spawn_pipeline(
            &self.shared,
            store.is_some().then_some(self.data_dir.as_path()),
            store.is_some().then_some(self.data_dir.as_path()),
            session.as_ref().map_or(u64::MAX, Session::generation),
            store,
            self.caps,
            self.rotate_interval,
            self.backoff_base,
            sampler,
            false,
            Vec::new(),
        )?;

        // The parent's session is still the parent's: leave its marker alone.
        std::mem::forget(std::mem::replace(&mut self.session, session));
        // The old channel senders must be LEAKED, never dropped. Dropping the last
        // sender disconnects the channel, which wakes the (nonexistent) receiver
        // thread through its parker — on macOS a libdispatch semaphore inherited
        // from the parent, which libdispatch refuses to signal in a forked process
        // (it traps, killing the child). Nothing here will ever read them again.
        std::mem::forget(std::mem::replace(&mut self.tx, pipeline.tx));
        std::mem::forget(std::mem::replace(&mut self.upload_tx, pipeline.upload_tx));
        self.worker = Some(pipeline.worker);
        self.uploader = Some(pipeline.uploader);
        Ok(())
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
        let mut guard = self.shared.scope.lock();
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

    /// Where this recorder keeps its state. In [`StorageMode::Memory`] crash
    /// capture is pointless (nothing could be recovered), so the host layer
    /// should not install the native handler.
    pub fn storage_mode(&self) -> StorageMode {
        self.storage
    }

    /// Reports suppressed by the rate limiter since launch.
    pub fn suppressed_reports(&self) -> u64 {
        self.shared.limiter.suppressed_total()
    }

    /// Whether a disk write has failed since launch (disk full, permission
    /// denied, directory removed). Reports still go out, rebuilt without capture
    /// context or held in memory, but crash recovery can no longer be relied on.
    pub fn is_degraded(&self) -> bool {
        self.shared.degraded.load(Ordering::Relaxed)
    }

    /// Why the recorder fell back to [`StorageMode::Memory`], if it did.
    pub fn storage_reason(&self) -> Option<&str> {
        self.storage_reason.as_deref()
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
        self.data_dir.join("parts").join(
            self.session
                .as_ref()
                .map_or(0, Session::generation)
                .to_string(),
        )
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
            if let Some(session) = &self.session {
                session.end();
            }
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
        let mut guard = self.shared.scope.lock();
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

/// `extra` without duplicates and without `data_dir` itself, compared by
/// canonical path. Recovering one directory twice in a launch would deliver its
/// sessions twice and burn their attempt cap.
fn distinct_extra_dirs(data_dir: &Path, extra: Vec<PathBuf>) -> Vec<PathBuf> {
    let key = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let mut seen = vec![key(data_dir)];
    let mut out = Vec::new();
    for dir in extra {
        let k = key(&dir);
        if !seen.contains(&k) {
            seen.push(k);
            out.push(dir);
        }
    }
    out
}

/// Claim a session and open its part store, or fail without leaving a liveness
/// marker behind (a marker with no store would be recovered as a phantom crash).
fn open_disk(data_dir: &Path, caps: WindowCaps) -> std::io::Result<(Session, PartStore)> {
    let session = Session::begin(data_dir)?;
    match PartStore::new(data_dir, session.generation(), caps) {
        Ok(store) => Ok((session, store)),
        Err(e) => {
            session.end();
            Err(e)
        }
    }
}

/// Start the uploader and the capture worker for `generation`.
///
/// `fresh_launch` is true for a process's first pipeline and false for one
/// revived in a fork child: only a fresh launch recovers crashed prior sessions
/// and force-drains the durable queue.
#[allow(clippy::too_many_arguments)]
fn spawn_pipeline(
    shared: &Arc<Shared>,
    data_dir: Option<&Path>,
    drain_dir: Option<&Path>,
    generation: u64,
    store: Option<PartStore>,
    caps: WindowCaps,
    rotate_interval: Duration,
    backoff_base: Duration,
    sampler: Option<Box<dyn TelemetrySampler>>,
    fresh_launch: bool,
    legacy_data_dirs: Vec<PathBuf>,
) -> std::io::Result<Pipeline> {
    // Uploader thread: drains the durable queue with retry.
    let (upload_tx, upload_rx) = channel();
    let uploader_shared = Arc::clone(shared);
    let uploader_data_dir = drain_dir.map(Path::to_path_buf);
    let uploader_legacy = legacy_data_dirs.clone();
    // Set once the worker has finished recovering the legacy directory, so the
    // uploader does not retire it while recovery is still about to fill its queue.
    let legacy_ready = Arc::new(AtomicBool::new(legacy_data_dirs.is_empty()));
    let uploader_ready = Arc::clone(&legacy_ready);
    let uploader = std::thread::Builder::new()
        .name("bugsee-uploader".into())
        .spawn(move || {
            uploader_loop(
                upload_rx,
                uploader_shared,
                uploader_data_dir,
                uploader_legacy,
                uploader_ready,
                backoff_base,
                fresh_launch,
            )
        })?;

    // Capture worker: owns the part store; enqueues reports.
    let (tx, rx) = channel();
    let worker_shared = Arc::clone(shared);
    let worker_data_dir = data_dir.map(Path::to_path_buf);
    let worker_drain_dir = drain_dir.map(Path::to_path_buf);
    let marker = data_dir.map(|d| Session::marker_path(d, generation));
    let worker_upload_tx = upload_tx.clone();

    let worker = std::thread::Builder::new()
        .name("bugsee-capture".into())
        .spawn(move || {
            // Queue any crashed prior session before capturing this one.
            // Contained: a panic here (e.g. a host before_send on a recovered
            // crash) must not abort the thread before worker_loop starts.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // Reap orphaned queue sidecars from a prior killed enqueue/remove
                // first (runs before this session enqueues anything). Only a fresh
                // launch: a fork child's parent may be mid-enqueue right now, and
                // its in-flight sidecars would look orphaned.
                if let (true, Some(dir)) = (fresh_launch, &worker_drain_dir) {
                    queue::gc_orphans(dir);
                }
                // With no writable disk the in-memory set is the only blacklist
                // `process_report` consults, so seed it from what is on disk.
                if let Some(dir) = &worker_drain_dir {
                    let known = queue::blacklisted_signatures(dir);
                    if !known.is_empty() {
                        worker_shared.mem_blacklist.lock().extend(known);
                    }
                }
                // A fork child recovers too. A prefork master recovers only at ITS
                // launch, and its own generation is the lowest, so it can never
                // recover a worker that died later; the next worker started from
                // it must, or that crash waits for the master to restart. Claims
                // keep siblings from recovering the same session twice.
                if let Some(dir) = &worker_drain_dir {
                    run_recovery(&worker_shared, dir, generation);
                }
                // Best effort, once: sessions a previous default location left
                // behind. No generation of ours lives there, so none is skipped.
                for dir in &legacy_data_dirs {
                    let known = queue::blacklisted_signatures(dir);
                    if !known.is_empty() {
                        worker_shared.mem_blacklist.lock().extend(known);
                    }
                    queue::gc_orphans(dir);
                    run_recovery(&worker_shared, dir, u64::MAX);
                }
            }));
            legacy_ready.store(true, Ordering::SeqCst);
            let _ = worker_upload_tx.send(UploadMsg::Wake);
            worker_loop(
                rx,
                store,
                worker_shared,
                rotate_interval,
                worker_data_dir,
                marker,
                caps,
                sampler,
                worker_upload_tx,
            );
        })?;

    Ok(Pipeline {
        tx,
        upload_tx,
        worker,
        uploader,
    })
}

/// Recover any prior generation that ended abnormally, queueing it for delivery.
/// The crashed session's state is discarded ONLY after the report is durably
/// enqueued — a failure leaves it on disk for a later attempt (no crash loss).
fn run_recovery(shared: &Shared, data_dir: &Path, current_generation: u64) {
    for pending in recovery::find_pending(data_dir, current_generation) {
        // Several processes share this directory (a prefork master and its
        // workers, above all): exactly one recovers a given session.
        let Some(_claim) = recovery::claim_pending(data_dir, &pending) else {
            continue;
        };
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
    // Scrubbed here, once: the recovered crash's capture channels, crash.json and
    // metadata all pass through `assemble_docs`, whichever way it is delivered.
    let assembled = reporting::assemble_scrubbed(
        &pending.parts_dir,
        window,
        &built.meta,
        &shared.env,
        Some(built.crash_json),
        &built.extra_files,
        &shared.app_token,
        end,
        shared.scrubber.as_ref(),
    );
    // A full or read-only directory cannot take the queue entry; deliver from
    // memory instead of losing the crash for this launch. The evidence stays on
    // disk (nothing to discard it with), so a later launch with a healthy disk
    // reports it again and the backend deduplicates.
    let delivered = match assembled {
        Ok(assembled) => {
            if queue::enqueue(data_dir, &assembled).is_ok() {
                true
            } else {
                hold_in_memory(shared, assembled);
                false
            }
        }
        Err(_) => false,
    };

    // Only destroy the crash evidence once it is safely queued.
    if delivered {
        recovery::discard(data_dir, pending);
    }
}

/// Merge ambient scope (email / labels / attributes) into a report's metadata.
fn merge_scope(shared: &Shared, meta: &mut ReportMeta) {
    let scope = shared.scope.lock();
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
    mut store: Option<PartStore>,
    shared: Arc<Shared>,
    rotate_interval: Duration,
    data_dir: Option<PathBuf>,
    marker: Option<PathBuf>,
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
                    // Memory mode has no capture window: the entry is dropped.
                    let Some(store) = store.as_mut() else { return };
                    if let Ok(value) = entry.to_json() {
                        if let Ok(bytes) = serde_json::to_vec(&value) {
                            if store
                                .append(entry.wire_type(), entry.timestamp(), &bytes)
                                .is_err()
                            {
                                shared.degraded.store(true, Ordering::Relaxed);
                            }
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
                        store.as_mut(),
                        &shared,
                        data_dir.as_deref(),
                        caps,
                        meta,
                        window_end,
                        crash_json,
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
                    take_snapshot(store.as_mut()?, data_dir.as_deref()?, caps, window_end)
                }))
                .ok()
                .flatten();
                let _ = ack.send(handle);
            }
            Ok(Msg::UploadSnapshot { handle, meta }) => {
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    deliver_snapshot(&shared, data_dir.as_deref(), handle, meta)
                }))
                .unwrap_or(false);
                if ok {
                    let _ = upload_tx.send(UploadMsg::Wake);
                }
            }
            Ok(Msg::Flush(ack)) => {
                if let Some(store) = store.as_mut() {
                    let _ = store.flush();
                }
                let _ = ack.send(());
            }
            Ok(Msg::Stop(ack)) => {
                if let Some(store) = store.as_mut() {
                    let _ = store.flush();
                }
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => { /* fall through to the rotation tick */ }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        // Run the rotation/eviction/telemetry tick whenever the interval has
        // elapsed — via a message OR a timeout — so steady traffic can't starve it.
        if Instant::now() >= next_rotate {
            rotate_tick(store.as_mut(), &shared, &mut sampler);
            if let Some(marker) = &marker {
                ensure_marker(&shared, marker);
            }
            next_rotate = Instant::now() + rotate_interval;
        }
    }
}

/// Make sure this session's liveness marker still exists. If the data directory
/// was removed and recreated under us (`create_dir_all` heals the tree but not
/// the marker), the next launch would not see this session as live, and its
/// crash would be dropped as an orphan. Rewrite it and mark the recorder
/// degraded: the capture window it had is gone with the removed files.
fn ensure_marker(shared: &Shared, marker: &Path) {
    if marker.exists() {
        return;
    }
    shared.degraded.store(true, Ordering::Relaxed);
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, std::process::id().to_string());
}

/// One rotation tick: sample telemetry (unless paused) then roll + evict the
/// window. Panic-contained so a host `TelemetrySampler` panic can't kill the
/// worker.
fn rotate_tick(
    mut store: Option<&mut PartStore>,
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
                    if let (Some(store), Ok(bytes)) = (store.as_mut(), serde_json::to_vec(&entry)) {
                        let _ = store.append("traces.system", ts, &bytes);
                    }
                }
            }
        }
        if let Some(store) = store.as_mut() {
            let _ = store.rotate();
        }
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
    data_dir: Option<&Path>,
    handle: SnapshotHandle,
    mut meta: ReportMeta,
) -> bool {
    merge_scope(shared, &mut meta);
    if let Some(hook) = &shared.before_send {
        if !hook(&mut meta) {
            return false; // handle's Drop reclaims the snapshot dir
        }
    }
    let Some(carried) = admit(shared, &mut meta) else {
        return false;
    };
    // The snapshot dir is reclaimed by `handle`'s Drop on return (any path).
    let assembled = reporting::assemble_scrubbed(
        &handle.dir,
        handle.window,
        &meta,
        &shared.env,
        None,
        &[],
        &shared.app_token,
        handle.window.end,
        shared.scrubber.as_ref(),
    )
    // The snapshot became unreadable (disk gone): still send what we know.
    .or_else(|_| {
        shared.degraded.store(true, Ordering::Relaxed);
        assemble_in_memory(shared, &meta, handle.window, None)
    });
    match assembled {
        Ok(assembled) => persist_or_hold(shared, data_dir, assembled),
        Err(_) => {
            shared.limiter.restore_suppressed(carried);
            false
        }
    }
}

/// Queue `assembled` on disk, or hold it in memory if the disk refuses it
/// (read-only, full, directory removed) or there is no disk. Always `true`: the
/// report is held either way.
fn persist_or_hold(shared: &Shared, data_dir: Option<&Path>, assembled: AssembledReport) -> bool {
    if let Some(dir) = data_dir {
        if queue::enqueue(dir, &assembled).is_ok() {
            return true;
        }
        shared.degraded.store(true, Ordering::Relaxed);
        // A failed enqueue can leave a partial sidecar set; the next launch's
        // orphan sweep reaps it. Do not retry the disk for this report.
    }
    hold_in_memory(shared, assembled);
    true
}

/// Assemble a bundle with no capture context and no file system access.
fn assemble_in_memory(
    shared: &Shared,
    meta: &ReportMeta,
    window: TimeWindow,
    crash_json: Option<Vec<u8>>,
) -> std::io::Result<AssembledReport> {
    reporting::assemble_docs(
        Vec::new(),
        window,
        meta,
        &shared.env,
        crash_json,
        &[],
        &shared.app_token,
        window.end,
        shared.scrubber.as_ref(),
    )
}

fn mem_blacklisted(shared: &Shared, sigs: &[String]) -> bool {
    if sigs.is_empty() {
        return false;
    }
    let set = shared.mem_blacklist.lock();
    sigs.iter().any(|s| set.contains(s))
}

/// Park a report in the bounded in-memory queue for the uploader. Over the cap
/// the NEWEST report is dropped (and the host told), keeping the backlog bounded.
fn hold_in_memory(shared: &Shared, report: AssembledReport) {
    let full = {
        let mut queue = shared.mem_queue.lock();
        if queue.len() >= MAX_MEMORY_REPORTS {
            true
        } else {
            queue.push(MemReport {
                id: shared.next_mem_id.fetch_add(1, Ordering::Relaxed),
                report,
                retry: 0,
                next_attempt_ms: 0,
                endpoint: None,
            });
            false
        }
    };
    if full {
        notify_dropped(shared, DropReason::StorageUnavailable);
    }
}

/// Apply the rate limiter. On admission, folds the count of reports suppressed
/// since the last admitted non-crash one into `meta` (so the backend can see
/// volume was cut) and returns that count, which the caller must hand back via
/// `limiter.restore_suppressed` if the report then fails to assemble. `None`
/// means the report was suppressed.
fn admit(shared: &Shared, meta: &mut ReportMeta) -> Option<u64> {
    use crate::ratelimit::Decision;
    let sig = meta.signatures.first().map_or("", String::as_str);
    match shared.limiter.check(meta.issue_type, sig) {
        Decision::Allow { suppressed_before } => {
            if suppressed_before > 0 {
                meta.attrs.insert(
                    "bugsee.reports_suppressed".into(),
                    Value::from(suppressed_before),
                );
            }
            Some(suppressed_before)
        }
        Decision::Deny => {
            notify_dropped(shared, DropReason::RateLimited);
            None
        }
    }
}

fn process_report(
    store: Option<&mut PartStore>,
    shared: &Shared,
    data_dir: Option<&Path>,
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

    // Suppress reports the server blacklisted (12004 TooManySimilar). Order
    // matters for a storm: the in-memory set is a lock + hash, so it goes first;
    // the rate limiter comes next (also lock + hash), so a flood of denied
    // reports never touches the disk; only the reports the limiter ADMITS pay
    // for the on-disk `.blacklist` read (which, unlike the in-memory set, also
    // sees blacklistings written by sibling processes sharing the directory).
    if mem_blacklisted(shared, &meta.signatures) {
        return Ok(());
    }
    let Some(carried) = admit(shared, &mut meta) else {
        return Ok(());
    };
    if data_dir.is_some_and(|dir| queue::any_blacklisted(dir, &meta.signatures)) {
        // Dropped after all: hand the suppressed-count summary back.
        shared.limiter.restore_suppressed(carried);
        return Ok(());
    }

    let window_start = window_end.saturating_sub(caps.max_window_ms);
    let window = TimeWindow {
        start: window_start,
        end: window_end,
    };

    // With a usable disk, snapshot + assemble from it. Any failure along the way
    // (most plausibly disk-full or a vanished directory) falls back to a bundle
    // without capture context, so the report still goes out.
    let has_disk = store.is_some() && data_dir.is_some();
    let from_disk = match (store, data_dir) {
        (Some(store), Some(data_dir)) => {
            assemble_from_disk(store, shared, data_dir, &meta, window, crash_json.clone())
        }
        _ => None,
    };
    let assembled = match from_disk {
        Some(assembled) => assembled,
        None => {
            if has_disk {
                shared.degraded.store(true, Ordering::Relaxed);
            }
            match assemble_in_memory(shared, &meta, window, crash_json) {
                Ok(assembled) => assembled,
                Err(e) => {
                    shared.limiter.restore_suppressed(carried);
                    return Err(e);
                }
            }
        }
    };

    // Persist to the durable queue; the uploader delivers it. Failing that, hold
    // it in memory.
    persist_or_hold(shared, data_dir, assembled);
    Ok(())
}

/// Snapshot the window into a fresh report dir and assemble it, or `None` if the
/// disk refuses any step.
fn assemble_from_disk(
    store: &mut PartStore,
    shared: &Shared,
    data_dir: &Path,
    meta: &ReportMeta,
    window: TimeWindow,
    crash_json: Option<Vec<u8>>,
) -> Option<AssembledReport> {
    let report_id = crate::util::random_hex(8);
    let report_dir = data_dir.join("reports").join(&report_id);
    // Own the report dir with a `SnapshotHandle` BEFORE creating it so EVERY exit
    // path reclaims its hard links via Drop. The earlier manual `remove_dir_all`
    // ran only after a successful assemble, so a `snapshot_into`/`assemble`
    // failure (most plausibly disk-full) permanently orphaned a `reports/<id>` dir
    // full of hard-linked parts, and `reports/` was never GC'd (F7).
    let handle = SnapshotHandle {
        dir: report_dir,
        window,
    };
    std::fs::create_dir_all(&handle.dir).ok()?;

    store
        .snapshot_into(&handle.dir, window.start, window.end)
        .ok()?;
    reporting::assemble_scrubbed(
        &handle.dir,
        window,
        meta,
        &shared.env,
        crash_json,
        &[],
        &shared.app_token,
        window.end,
        shared.scrubber.as_ref(),
    )
    .ok()
}

fn uploader_loop(
    rx: Receiver<UploadMsg>,
    shared: Arc<Shared>,
    data_dir: Option<PathBuf>,
    legacy_data_dirs: Vec<PathBuf>,
    legacy_ready: Arc<AtomicBool>,
    backoff_base: Duration,
    fresh_launch: bool,
) {
    let poll = Duration::from_secs(5);
    let mut legacy = legacy_data_dirs;
    // A fresh launch retries any queued report immediately (force), ignoring
    // backoff scheduled by the prior session. A fork child skips this: the
    // parent's uploader is already working the shared queue. Guarded so a
    // panicking Transport impl can't kill the uploader.
    if fresh_launch {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drain_all(&shared, data_dir.as_deref(), &legacy, backoff_base, true)
        }));
    }
    loop {
        match rx.recv_timeout(poll) {
            Ok(UploadMsg::Wake) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_all(&shared, data_dir.as_deref(), &legacy, backoff_base, false)
                }));
            }
            Ok(UploadMsg::Drain { deadline, ack }) => {
                let empty = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let dirs: Vec<&Path> = data_dir
                        .as_deref()
                        .into_iter()
                        .chain(legacy.iter().map(PathBuf::as_path))
                        .collect();
                    drain_until_empty(&shared, &dirs, backoff_base, deadline)
                }))
                .unwrap_or(false);
                let _ = ack.send(empty);
            }
            Ok(UploadMsg::Stop(ack)) => {
                // A report held in memory has no next launch to be retried by, so
                // give it one last bounded attempt (the stopper waits 2 s for the
                // ack). Disk reports survive in `queue/` and need nothing here.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_memory(
                        &shared,
                        backoff_base,
                        true,
                        false,
                        Some(Instant::now() + Duration::from_millis(1500)),
                    )
                }));
                let _ = ack.send(());
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_all(&shared, data_dir.as_deref(), &legacy, backoff_base, false)
                }));
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
        retire_legacy(&shared, &mut legacy, &legacy_ready);
    }
}

/// Queued reports in `dir` still awaiting delivery: everything on disk except
/// bundles already delivered whose deletion failed.
fn pending_reports(shared: &Shared, dir: &Path) -> Vec<queue::QueuedReport> {
    let mut all = queue::list_pending(dir);
    let stuck = shared.delivered_stuck.lock();
    if !stuck.is_empty() {
        all.retain(|r| !stuck.contains(&r.bundle));
    }
    all
}

/// Stop polling the legacy directory once recovery has run (it runs once per
/// launch, so nothing new can appear) and its queue is empty.
fn retire_legacy(shared: &Shared, legacy: &mut Vec<PathBuf>, ready: &AtomicBool) {
    if !ready.load(Ordering::SeqCst) {
        return;
    }
    legacy.retain(|dir| !pending_reports(shared, dir).is_empty());
}

/// [`drain`] the data directory, then the legacy one a launch was asked to adopt.
fn drain_all(
    shared: &Shared,
    data_dir: Option<&Path>,
    legacy: &[PathBuf],
    backoff_base: Duration,
    force: bool,
) -> bool {
    let mut attempted = drain(shared, data_dir, backoff_base, force);
    for dir in legacy {
        attempted |= drain_disk(shared, dir, backoff_base, force);
    }
    attempted
}

/// Attempt delivery of every due queued report once. On the flush path `force`
/// is set so backing-off reports are retried immediately. Returns whether any
/// report was attempted (used to pace `drain_until_empty`).
fn drain(shared: &Shared, data_dir: Option<&Path>, backoff_base: Duration, force: bool) -> bool {
    let attempted = drain_memory(shared, backoff_base, force, true, None);
    match data_dir {
        Some(dir) => drain_disk(shared, dir, backoff_base, force) | attempted,
        None => attempted,
    }
}

/// The on-disk half of [`drain`].
fn drain_disk(shared: &Shared, data_dir: &Path, backoff_base: Duration, force: bool) -> bool {
    let now = epoch_ms();
    let mut attempted = false;
    for report in pending_reports(shared, data_dir) {
        // Another process sharing the queue may be delivering this one.
        let Some(_claim) = queue::try_claim_lenient(&report) else {
            continue;
        };
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
        Ok(()) => {
            queue::remove(report);
            if report.bundle.exists() {
                // Delivered, but the directory will not let us delete it.
                shared.delivered_stuck.lock().insert(report.bundle.clone());
            }
        }
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
            shared.mem_blacklist.lock().extend(sigs);
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

/// Attempt delivery of the in-memory backlog. Mirrors [`deliver_queued`]: a
/// transient failure keeps the report (counting toward the retry cap only on the
/// backoff-paced path), anything terminal drops it and tells the host.
///
/// An entry STAYS in the queue while it is being delivered (the lock is held only
/// to pick it and to settle it, never across the network), so the queue length is
/// the true backlog and [`MAX_MEMORY_REPORTS`] holds. Stops at `deadline`, like
/// the disk loop in [`drain_until_empty`].
fn drain_memory(
    shared: &Shared,
    backoff_base: Duration,
    force: bool,
    count_failure: bool,
    deadline: Option<Instant>,
) -> bool {
    let mut attempted = false;
    // Each entry is tried once per call, whatever its outcome.
    let mut tried: Vec<u64> = Vec::new();
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let now = epoch_ms();
        let next = shared
            .mem_queue
            .lock()
            .iter()
            .find(|m| !tried.contains(&m.id) && (force || now >= m.next_attempt_ms))
            .map(|m| (m.id, m.report.clone(), m.endpoint.clone()));
        let Some((id, report, cached_endpoint)) = next else {
            break;
        };
        tried.push(id);
        attempted = true;

        let mut endpoint_out = None;
        let result = transport::deliver(
            &*shared.transport,
            &shared.app_token,
            &shared.environment_json,
            &shared.session,
            &report,
            cached_endpoint.as_deref(),
            &mut endpoint_out,
        );

        // Settle: `None` keeps the (updated) entry, `Some(reason)` ends it.
        let mut dropped = None;
        let mut queue = shared.mem_queue.lock();
        let Some(pos) = queue.iter().position(|m| m.id == id) else {
            continue; // cleared under us (fork child); nothing to settle
        };
        match result {
            Ok(()) => {
                queue.remove(pos);
            }
            Err(TransportError::Transient(_)) | Err(TransportError::SessionExpired) => {
                let item = &mut queue[pos];
                item.endpoint = endpoint_out;
                if count_failure {
                    item.retry += 1;
                    if item.retry >= UPLOAD_RETRY_CAP {
                        queue.remove(pos);
                        dropped = Some(DropReason::RetriesExhausted);
                    } else {
                        item.next_attempt_ms = now + backoff_delay(item.retry, backoff_base);
                    }
                }
            }
            Err(TransportError::TooManySimilar { signatures }) => {
                queue.remove(pos);
                dropped = Some(DropReason::TooManySimilar);
                // Stop the same fault from being sent again: no disk to blacklist on.
                let sigs = if signatures.is_empty() {
                    queue::signatures_of(&report)
                } else {
                    signatures
                };
                // Lock order: the queue lock is held; the blacklist is a leaf.
                shared.mem_blacklist.lock().extend(sigs);
            }
            Err(e) => {
                queue.remove(pos);
                dropped = Some(drop_reason_for(e));
            }
        }
        drop(queue);
        if let Some(reason) = dropped {
            notify_dropped(shared, reason);
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

/// Drain until the queue is empty or `deadline` elapses, ignoring backoff so a
/// report in its backoff window is still attempted on an explicit flush. Forced
/// attempts do NOT count toward the durable retry cap (`count_failure = false`),
/// so a fast-failing offline flush re-attempts a report until the deadline
/// without ever racing its counter to the cap and deleting it (F13); a
/// flaky-but-recovering transport still delivers within the flush. Returns
/// whether the queue ended up empty.
fn drain_until_empty(
    shared: &Shared,
    dirs: &[&Path],
    backoff_base: Duration,
    deadline: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        let now = epoch_ms();
        drain_memory(shared, backoff_base, true, false, Some(start + deadline));
        for &data_dir in dirs {
            for report in pending_reports(shared, data_dir) {
                let Some(_claim) = queue::try_claim_lenient(&report) else {
                    continue; // a peer is delivering it; the loop below waits it out
                };
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
        }
        let disk_empty = dirs.iter().all(|d| pending_reports(shared, d).is_empty());
        if disk_empty && shared.mem_queue.lock().is_empty() {
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

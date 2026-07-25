//
//  runtime.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The capture runtime: a single background worker that owns the part store,
//! drains captured entries, rotates on a timer, and assembles + delivers
//! reports on demand. Producers only enqueue; all disk and network work happens
//! off the caller's thread.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::capture::{export, PartStore, WindowCaps};
use crate::model::entry::CaptureEntry;
use crate::model::environment::Environment;
use crate::model::report::TimeWindow;
use crate::model::scope::Scope;
use crate::recovery;
use crate::reporting::{self, ReportMeta};
use crate::session::Session;
use crate::transport::{self, Transport};
use crate::util::epoch_ms;

/// Configuration for a [`Recorder`].
pub struct RecorderConfig {
    pub data_dir: PathBuf,
    pub app_token: String,
    pub sdk_version: String,
    pub caps: WindowCaps,
    pub rotate_interval: Duration,
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
        }
    }
}

/// State shared between the facade and the worker.
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
        ack: Sender<()>,
    },
    Flush(Sender<()>),
    Stop(Sender<()>),
}

/// The running capture pipeline. Cloneable handles capture into the same worker.
pub struct Recorder {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    caps: WindowCaps,
    data_dir: PathBuf,
    session: Session,
    worker: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Start the worker and begin capturing. Recovers and delivers any prior
    /// session that ended abnormally.
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

        let (tx, rx) = channel();
        let store = PartStore::new(&config.data_dir, generation, config.caps)?;
        let worker_shared = Arc::clone(&shared);
        let rotate_interval = config.rotate_interval;
        let data_dir = config.data_dir.clone();
        let worker_data_dir = data_dir.clone();
        let caps = config.caps;

        let worker = std::thread::Builder::new()
            .name("bugsee-capture".into())
            .spawn(move || {
                // Deliver any crashed prior session before capturing this one.
                run_recovery(&worker_shared, &worker_data_dir, generation);
                worker_loop(rx, store, worker_shared, rotate_interval, worker_data_dir, caps);
            })?;

        Ok(Recorder {
            tx,
            shared,
            caps,
            data_dir,
            session,
            worker: Some(worker),
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
    /// Fire-and-forget; use [`Recorder::flush`] to await delivery.
    pub fn report(&self, meta: ReportMeta, crash_json: Option<Vec<u8>>) {
        self.report_at(meta, epoch_ms(), crash_json);
    }

    /// Like [`Recorder::report`] but with an explicit window-end timestamp, used
    /// by the deferred `create_report` flow to pin the window at creation time.
    pub fn report_at(&self, meta: ReportMeta, window_end: i64, crash_json: Option<Vec<u8>>) {
        let (ack, _rx) = channel();
        let _ = self.tx.send(Msg::Report {
            meta,
            window_end,
            crash_json,
            ack,
        });
    }

    /// Block until all previously-enqueued work has been processed, or `timeout`.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (tx, rx) = channel();
        if self.tx.send(Msg::Flush(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// Data directory root for this recorder.
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Path where the native crash handler should write its crash-info marker
    /// for this generation (picked up by next-launch recovery).
    pub fn crash_info_path(&self) -> PathBuf {
        self.data_dir
            .join("parts")
            .join(self.session.generation().to_string())
            .join(crate::recovery::CRASH_INFO_NAME)
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
        // Clean shutdown: drop the liveness marker so next launch does not treat
        // this session as an abnormal exit.
        self.session.end();
    }
}

/// Recover and deliver any prior generation that ended abnormally.
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
            let _ = transport::deliver(
                &*shared.transport,
                &shared.app_token,
                &shared.environment_json,
                &shared.session,
                &assembled,
            );
        }
        recovery::discard(data_dir, &pending);
    }
}

fn worker_loop(
    rx: Receiver<Msg>,
    mut store: PartStore,
    shared: Arc<Shared>,
    rotate_interval: Duration,
    data_dir: PathBuf,
    caps: WindowCaps,
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
                ack,
            }) => {
                let _ = process_report(&mut store, &shared, &data_dir, caps, meta, window_end, crash_json);
                let _ = ack.send(());
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

    let window = crate::model::report::TimeWindow {
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

    let result = transport::deliver(
        &*shared.transport,
        &shared.app_token,
        &shared.environment_json,
        &shared.session,
        &assembled,
    );
    // Phase 1: clean up the snapshot regardless. Durable queue + retry lands in
    // Phase 5.
    let _ = std::fs::remove_dir_all(&report_dir);
    let _ = result;
    Ok(())
}

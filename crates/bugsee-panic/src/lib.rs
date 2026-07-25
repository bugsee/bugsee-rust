//
//  lib.rs
//  bugsee-panic
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The Rust panic observer and `catch_unwind` boundary guards.
//!
//! A single chained global hook captures the panic message, location, and a
//! pre-unwind backtrace into a thread-local slot (the hook does no reporting or
//! I/O — it just snapshots context before the stack unwinds). Boundary guards
//! then read that context after `catch_unwind` and hand a [`PanicReport`] to the
//! registered [`PanicReporter`], which decides recovery and delivery.
//!
//! See `DESIGN.md` §9.

use std::cell::RefCell;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Once, OnceLock};

use bugsee_core::model::crash::{Frame, FrameData};
use bugsee_core::panic_info::PanicInfo;
use bugsee_core::util::epoch_ms;

/// A captured panic, handed to the reporter.
pub struct PanicReport {
    pub reason: String,
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
    pub frames: Vec<Frame>,
    /// `true` when the panic was contained by a guard; `false` if it escaped.
    pub handled: bool,
}

/// A sink for captured panics (implemented by the host SDK layer).
pub trait PanicReporter: Send + Sync {
    /// Handle a captured panic. Must not itself panic.
    fn report_panic(&self, report: PanicReport);
}

struct CapturedContext {
    reason: String,
    file: Option<String>,
    line: u32,
    column: u32,
    frames: Vec<Frame>,
}

thread_local! {
    static LAST_PANIC: RefCell<Option<CapturedContext>> = const { RefCell::new(None) };
}

static REPORTER: OnceLock<Arc<dyn PanicReporter>> = OnceLock::new();
static INSTALL: Once = Once::new();
/// Where to persist the panic snapshot for next-launch abort correlation.
static SNAPSHOT_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Set the on-disk path where the observer persists a panic snapshot (used to
/// correlate an aborting panic with its `SIGABRT` on the next launch).
pub fn set_snapshot_path(path: PathBuf) {
    if let Ok(mut guard) = SNAPSHOT_PATH.lock() {
        *guard = Some(path);
    }
}

/// Clear the persisted-snapshot path.
pub fn clear_snapshot_path() {
    if let Ok(mut guard) = SNAPSHOT_PATH.lock() {
        *guard = None;
    }
}

/// Install the chained global panic observer and register `reporter`.
/// Idempotent — the hook is installed at most once; the first reporter wins.
pub fn install(reporter: Arc<dyn PanicReporter>) {
    let _ = REPORTER.set(reporter);
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            // Minimal pre-unwind capture into thread-local storage.
            let (file, line, column) = match info.location() {
                Some(loc) => (Some(loc.file().to_string()), loc.line(), loc.column()),
                None => (None, 0, 0),
            };
            let reason = payload_message(info.payload());
            let frames = capture_frames();

            // Persist a snapshot so an aborting panic can be correlated with its
            // SIGABRT on the next launch. try_lock avoids any deadlock if the
            // panic happened while the path was being set.
            if let Ok(guard) = SNAPSHOT_PATH.try_lock() {
                if let Some(path) = guard.as_ref() {
                    let snapshot = PanicInfo {
                        reason: reason.clone(),
                        file: file.clone(),
                        line,
                        column,
                        timestamp: epoch_ms(),
                        frames: frames.clone(),
                    };
                    let _ = snapshot.write_to(path);
                }
            }

            LAST_PANIC.with(|slot| {
                *slot.borrow_mut() = Some(CapturedContext {
                    reason,
                    file,
                    line,
                    column,
                    frames,
                });
            });
            // Preserve prior hook behavior (default abort message, other SDKs).
            previous(info);
        }));
    });
}

/// Run `f` inside a panic boundary. On a Rust panic the pre-unwind context is
/// reported as `handled` and the payload is returned as `Err`.
pub fn guard<R>(f: impl FnOnce() -> R) -> std::thread::Result<R> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(payload) => {
            report_caught(&payload);
            Err(payload)
        }
    }
}

/// Spawn an SDK-owned thread whose root contains any panic (reported as
/// `handled`, since the process survives).
pub fn spawn_guarded<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = guard(f);
        })
}

fn report_caught(payload: &(dyn std::any::Any + Send)) {
    let ctx = LAST_PANIC.with(|slot| slot.borrow_mut().take());

    // The observer persists a snapshot for EVERY panic (it can't know at hook
    // time whether the panic will be caught). Since this panic was contained,
    // delete that snapshot so a later crash does not misread it as fatal. Use a
    // BLOCKING lock (unlike the hook, which must `try_lock` to avoid a reentrant
    // self-deadlock): this runs AFTER `catch_unwind` returns — the panic has
    // fully unwound and released any locks — so a spuriously-lost `try_lock` race
    // must not silently skip the cleanup and strand a stale snapshot.
    if let Ok(guard) = SNAPSHOT_PATH.lock() {
        if let Some(path) = guard.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }

    let Some(reporter) = REPORTER.get() else {
        return;
    };
    let report = match ctx {
        Some(c) => PanicReport {
            reason: c.reason,
            file: c.file,
            line: c.line,
            column: c.column,
            frames: c.frames,
            handled: true,
        },
        // No hook context (hook not installed) — fall back to the payload.
        None => PanicReport {
            reason: payload_message(payload),
            file: None,
            line: 0,
            column: 0,
            frames: Vec::new(),
            handled: true,
        },
    };
    reporter.report_panic(report);
}

/// Extract a human-readable message from a panic payload.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "Box<dyn Any>".to_string()
    }
}

/// Capture and resolve the current backtrace as `crash.json` frames, hiding
/// SDK-internal frames.
fn capture_frames() -> Vec<Frame> {
    let mut frames = Vec::new();
    let bt = backtrace::Backtrace::new();
    for frame in bt.frames() {
        for symbol in frame.symbols() {
            let name = symbol
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let file = symbol.filename().map(|p| p.to_string_lossy().into_owned());
            let line = symbol.lineno().map(|l| l as i64).unwrap_or(-1);
            let hidden = name.starts_with("bugsee")
                || name.starts_with("backtrace::")
                || name.starts_with("core::panic")
                || name.starts_with("std::panic");
            let trace = match (&file, line) {
                (Some(f), l) if l >= 0 => format!("{name} ({f}:{l})"),
                (Some(f), _) => format!("{name} ({f})"),
                (None, _) => name.clone(),
            };
            frames.push(Frame {
                trace,
                hidden,
                data: FrameData {
                    source: file,
                    member_class: None,
                    member: Some(name),
                    line,
                },
            });
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Collector {
        reports: Mutex<Vec<String>>,
    }
    impl PanicReporter for Collector {
        fn report_panic(&self, report: PanicReport) {
            self.reports.lock().unwrap().push(report.reason);
        }
    }

    #[test]
    fn guard_contains_panic_and_reports_reason() {
        let collector = Arc::new(Collector::default());
        install(collector.clone());

        let result = guard(|| {
            panic!("boom in guard");
        });
        assert!(result.is_err(), "panic contained, not propagated");

        let reasons = collector.reports.lock().unwrap();
        assert!(
            reasons.iter().any(|r| r.contains("boom in guard")),
            "reported reasons: {reasons:?}"
        );
    }

    #[test]
    fn guard_passes_through_ok() {
        assert_eq!(guard(|| 2 + 2).unwrap(), 4);
    }
}

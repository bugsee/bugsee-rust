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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};

use bugsee_core::model::crash::{Frame, FrameData};
use bugsee_core::panic_info::PanicInfo;
use bugsee_core::platform_io::storage_path;
use bugsee_core::util::epoch_ms;
use bugsee_platform::Storage;

/// A captured panic, handed to the reporter.
pub struct PanicReport {
    pub reason: String,
    pub file: Option<String>,
    pub line: u32,
    pub column: u32,
    pub frames: Vec<Frame>,
    /// `true` when the panic was contained by a guard; `false` if it escaped.
    pub handled: bool,
    /// Name of the thread that panicked, when it is **not** the main thread.
    ///
    /// `None` means the panic happened on `main` (or on an unnamed thread we
    /// could not distinguish). `Some(name)` marks a panic that killed only a
    /// worker thread — the process survived it, so it is reported as a non-fatal
    /// error rather than a crash, and the thread name rides the report as
    /// context.
    pub thread: Option<String>,
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
static SNAPSHOT: Mutex<Option<(Arc<dyn Storage>, String)>> = Mutex::new(None);

/// Set the storage target where the observer persists a panic snapshot (used to
/// correlate an aborting panic with its `SIGABRT` on the next launch).
/// `rel` is relative to the storage root (e.g. `parts/<gen>/panic.info`).
pub fn set_snapshot(storage: Arc<dyn Storage>, rel: impl Into<String>) {
    if let Ok(mut guard) = SNAPSHOT.lock() {
        *guard = Some((storage, rel.into()));
    }
}

/// Clear the persisted-snapshot target.
pub fn clear_snapshot() {
    if let Ok(mut guard) = SNAPSHOT.lock() {
        *guard = None;
    }
}

/// Opt-in: report every panic from inside the hook instead of relying on
/// next-launch recovery. See [`set_report_from_hook`].
static REPORT_FROM_HOOK: AtomicBool = AtomicBool::new(false);

/// Report panics **immediately, from inside the panic hook**, rather than the
/// default mark-and-recover behaviour.
///
/// Default (`false`): the hook only captures. An uncaught panic that kills the
/// process is reported on the **next launch** by recovery, and a panic caught at
/// a [`guard`] boundary is reported there as `handled`. The fault path stays
/// minimal, which is the crash-time isolation rule the SDK is built around.
///
/// Enabled (`true`): the hook additionally builds the report and hands it to the
/// reporter straight away (the host then flushes synchronously). This is the
/// only way to catch a panic that terminates a **non-main thread** — such a
/// panic never unwinds out of `main`, so there is no process death for recovery
/// to observe.
///
/// The trade-off is real: the reporting path then runs allocation, disk and
/// network work on the panicking thread. Prefer the default unless you need
/// immediate delivery or thread-panic coverage.
pub fn set_report_from_hook(enabled: bool) {
    REPORT_FROM_HOOK.store(enabled, Ordering::SeqCst);
}

/// Whether hook-time reporting is enabled.
pub fn reports_from_hook() -> bool {
    REPORT_FROM_HOOK.load(Ordering::SeqCst)
}

thread_local! {
    /// How many [`guard`] frames are active on this thread.
    ///
    /// The hook cannot know whether a panic will be caught — but it *can* know
    /// whether one of our `catch_unwind` boundaries is on the stack ready to
    /// catch it. Reporting inline while a guard is active would double-report:
    /// once from the hook and again from `report_caught`. So inline reporting is
    /// gated on this being zero, i.e. nothing of ours will contain this panic.
    static GUARD_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Whether a [`guard`] boundary is active on the current thread (and will
/// therefore catch and report this panic itself).
fn guard_active() -> bool {
    GUARD_DEPTH.with(|d| d.get() > 0)
}

/// The current thread's name when it is a **worker** thread, else `None`.
///
/// Used to decide whether a panic is process-fatal. This is a heuristic, and
/// deliberately biased toward `None` (treat as main / process-fatal), because
/// the failure modes are asymmetric:
/// - a false `Some` would down-grade a genuinely fatal panic to a non-fatal
///   error **and** skip the recovery path, losing the crash;
/// - a false `None` merely falls back to mark-and-recover, which for a surviving
///   process reports nothing — the status quo we are improving on.
///
/// Rust names the initial thread `"main"`. An unnamed thread (`None` from
/// `Thread::name()`) is therefore *not* main, but we cannot label it, so it is
/// reported as `"<unnamed>"`.
///
/// Caveat: under `panic = "abort"` **every** panic is process-fatal regardless of
/// thread, and there is no runtime way to detect that build setting. Under abort
/// a worker-thread panic reported here is followed by the process dying, which
/// the SIGABRT correlation path also records.
fn worker_thread_name() -> Option<String> {
    let current = std::thread::current();
    match current.name() {
        Some("main") => None,
        Some(name) => Some(name.to_string()),
        None => Some("<unnamed>".to_string()),
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
            if let Ok(guard) = SNAPSHOT.try_lock() {
                if let Some((storage, rel)) = guard.as_ref() {
                    let snapshot = PanicInfo {
                        reason: reason.clone(),
                        file: file.clone(),
                        line,
                        column,
                        timestamp: epoch_ms(),
                        frames: frames.clone(),
                    };
                    let _ = snapshot.write_to(storage.as_ref(), rel);
                }
            }

            // Decide whether to report right here, or leave it to a `guard`
            // boundary / next-launch recovery.
            //
            // A panic on a NON-MAIN thread is always reported inline. It kills
            // only that thread — the process survives — so there is no
            // next-launch recovery to fall back on (nothing died), and equally
            // no crash-time isolation concern: the process is healthy and this
            // is just an ordinary report on an ordinary thread. Without this
            // such panics are invisible.
            //
            // A main-thread panic keeps the default mark-and-recover path, where
            // the fault path stays minimal, unless the host opted in.
            // …but never while one of our guards is on the stack: it will catch
            // this panic and report it as `handled`, so reporting here too would
            // emit the same panic twice.
            let worker_thread = worker_thread_name();
            let will_be_caught = guard_active();
            if !will_be_caught
                && (worker_thread.is_some() || REPORT_FROM_HOOK.load(Ordering::SeqCst))
            {
                if let Some(reporter) = REPORTER.get() {
                    // `handled` drives crash-vs-error downstream. A worker-thread
                    // panic did NOT terminate the process, so reporting it as a
                    // crash would corrupt crash-free-session rates; it is a
                    // non-fatal error, distinguished from a caught panic by the
                    // `thread` field. A main-thread panic reported from here
                    // (opt-in) genuinely is fatal.
                    let is_worker = worker_thread.is_some();
                    reporter.report_panic(PanicReport {
                        reason: reason.clone(),
                        file: file.clone(),
                        line,
                        column,
                        frames: frames.clone(),
                        handled: is_worker,
                        thread: worker_thread,
                    });
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
    // Tell the hook a catch boundary is active on this thread, so it does not
    // ALSO report the panic inline (which would double-report it — once as
    // uncaught from the hook, once as handled from here). Restored on every
    // path, including the unwinding one.
    GUARD_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    let result = panic::catch_unwind(AssertUnwindSafe(f));
    GUARD_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));

    match result {
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
    if let Ok(guard) = SNAPSHOT.lock() {
        if let Some((storage, rel)) = guard.as_ref() {
            let _ = storage.remove_file(&storage_path(rel));
        }
    }

    let Some(reporter) = REPORTER.get() else {
        return;
    };
    // `thread: None` — this panic was CAUGHT, so it is a handled error
    // regardless of which thread it happened on; the `thread` field exists to
    // mark an *uncaught* worker-thread panic.
    let report = match ctx {
        Some(c) => PanicReport {
            reason: c.reason,
            file: c.file,
            line: c.line,
            column: c.column,
            frames: c.frames,
            handled: true,
            thread: None,
        },
        // No hook context (hook not installed) — fall back to the payload.
        None => PanicReport {
            reason: payload_message(payload),
            file: None,
            line: 0,
            column: 0,
            frames: Vec::new(),
            handled: true,
            thread: None,
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
            // Shared with the handled-error capture path (bugsee/src/frames.rs)
            // so the two cannot drift: an unhidden panic-runtime frame becomes
            // the reported crash site and collapses distinct panics into one
            // group.
            let hidden = bugsee_core::model::crash::is_internal_frame(&name);
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
        reports: Mutex<Vec<(String, bool, Option<String>)>>,
    }
    impl PanicReporter for Collector {
        fn report_panic(&self, report: PanicReport) {
            self.reports
                .lock()
                .unwrap()
                .push((report.reason, report.handled, report.thread));
        }
    }

    /// `REPORTER` is a `OnceLock` (first install wins) and the hook is global, so
    /// every test in this binary must share ONE collector. Tests run in parallel
    /// against it and filter by their own unique panic message.
    fn collector() -> Arc<Collector> {
        static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();
        let c = COLLECTOR.get_or_init(|| Arc::new(Collector::default()));
        install(c.clone());
        c.clone()
    }

    /// Every report whose reason contains `needle`.
    fn matching(c: &Collector, needle: &str) -> Vec<(String, bool, Option<String>)> {
        c.reports
            .lock()
            .unwrap()
            .iter()
            .filter(|(reason, _, _)| reason.contains(needle))
            .cloned()
            .collect()
    }

    #[test]
    fn guard_contains_panic_and_reports_reason() {
        let c = collector();
        let result = guard(|| {
            panic!("boom in guard");
        });
        assert!(result.is_err(), "panic contained, not propagated");
        assert!(
            !matching(&c, "boom in guard").is_empty(),
            "the caught panic was reported"
        );
    }

    #[test]
    fn guard_passes_through_ok() {
        assert_eq!(guard(|| 2 + 2).unwrap(), 4);
    }

    /// A caught panic must be reported EXACTLY ONCE — by the guard, as handled.
    ///
    /// Regression: the hook reports uncaught worker-thread panics inline, and
    /// cargo runs every test on a *named worker thread*, so without the
    /// guard-depth gate this panic was reported twice (once inline as uncaught,
    /// once by the guard as handled).
    #[test]
    fn caught_panic_on_a_worker_thread_is_reported_once() {
        let c = collector();
        let _ = guard(|| panic!("caught once only"));

        let found = matching(&c, "caught once only");
        assert_eq!(found.len(), 1, "reported exactly once: {found:?}");
        assert!(found[0].1, "a caught panic is handled");
        assert!(
            found[0].2.is_none(),
            "a caught panic carries no uncaught-thread marker"
        );
    }

    /// An UNCAUGHT panic on a worker thread is reported inline by the hook —
    /// nothing catches it and the process survives, so no other path would ever
    /// surface it.
    #[test]
    fn uncaught_worker_thread_panic_is_reported_inline() {
        let c = collector();

        // No `guard` here: let the panic kill the thread, as user code would.
        let handle = std::thread::Builder::new()
            .name("worker-under-test".into())
            .spawn(|| panic!("uncaught on a worker"))
            .unwrap();
        assert!(handle.join().is_err(), "the worker thread died");

        let found = matching(&c, "uncaught on a worker");
        assert_eq!(found.len(), 1, "reported exactly once: {found:?}");
        assert!(
            found[0].1,
            "the process survived, so it is a non-fatal error, not a crash"
        );
        assert_eq!(
            found[0].2.as_deref(),
            Some("worker-under-test"),
            "annotated with the thread that died"
        );
    }

    #[test]
    fn guard_depth_is_restored_after_an_unwinding_panic() {
        assert!(!guard_active());
        let _ = guard(|| panic!("unwind through the guard"));
        assert!(!guard_active(), "depth restored on the panicking path");
    }
}

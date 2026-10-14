//
//  shutdown.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! The shutdown contract, per exit path (see `Bugsee::stop`): what is delivered
//! by the dying process, what is recovered on the next launch, and that nothing
//! is lost. Delivery is at-least-once; these cases avoid killing a process
//! mid-upload, so they can assert exactly-once.
//!
//! Each case runs a real child process (this same test binary, re-entered
//! through `child_entry`), ends it the way the case says, then relaunches the SDK
//! in this process over the same data directory.

#![cfg(unix)]

use std::io::{Cursor, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee::core::transport::{Transport, TransportError};
use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions, LogLevel};
use serde_json::Value;
use zip::ZipArchive;

const KIND_ENV: &str = "BUGSEE_SHUTDOWN_KIND";
const DIR_ENV: &str = "BUGSEE_SHUTDOWN_DIR";
const FAIL_ENV: &str = "BUGSEE_SHUTDOWN_FAIL_UPLOADS";
const SLOW_ENV: &str = "BUGSEE_SHUTDOWN_SLOW_UPLOADS";
const TERM_MS_ENV: &str = "BUGSEE_SHUTDOWN_TERM_MS";
const PENDING: &str = "pending-report-marker";
const ABNORMAL: &str = "terminated abnormally";

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Writes every uploaded bundle to a file so the parent can see what a child
/// delivered; optionally fails every upload (an unreachable backend) or hangs
/// in it (a backend that accepts the connection and never answers).
struct FileTransport {
    dir: PathBuf,
    fail: bool,
    slow: bool,
    n: AtomicUsize,
}

impl Transport for FileTransport {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("token".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        if self.slow {
            std::thread::sleep(Duration::from_secs(20));
        }
        if self.fail || self.slow {
            return Err(TransportError::Transient("backend unreachable".into()));
        }
        Ok("https://uploads.example/x".into())
    }
    fn upload_bundle(&self, _: &str, zip: &[u8]) -> Result<(), TransportError> {
        let n = self.n.fetch_add(1, Ordering::SeqCst);
        std::fs::write(
            self.dir
                .join("out")
                .join(format!("{}-{n}.zip", std::process::id())),
            zip,
        )
        .map_err(|e| TransportError::Transient(e.to_string()))
    }
}

fn crash_json(zip: &[u8]) -> Value {
    let mut archive = ZipArchive::new(Cursor::new(zip)).expect("bundle is a zip");
    let mut entry = archive.by_name("crash.json").expect("crash.json present");
    let mut raw = Vec::new();
    entry.read_to_end(&mut raw).unwrap();
    serde_json::from_slice(&raw).unwrap()
}

fn delivered_by_child(dir: &Path) -> Vec<Value> {
    std::fs::read_dir(dir.join("out"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| crash_json(&std::fs::read(e.path()).unwrap()))
        .collect()
}

// ---------------------------------------------------------------------------
// The child.
// ---------------------------------------------------------------------------

static HOST_HANDLER_RAN: AtomicBool = AtomicBool::new(false);
/// What was in force when the "late" host handler took the signal over.
static LATE_PREVIOUS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn host_handler(_: libc::c_int) {
    HOST_HANDLER_RAN.store(true, Ordering::SeqCst);
}

extern "C" fn host_siginfo_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    _: *mut libc::c_void,
) {
    // SAFETY: the kernel passes a valid siginfo to an SA_SIGINFO handler.
    if !info.is_null() && unsafe { (*info).si_signo } == sig {
        HOST_HANDLER_RAN.store(true, Ordering::SeqCst);
    }
}

/// A host that took the signal over after the SDK (tokio, `signal-hook`) and
/// chains to the previous handler, which is the SDK's.
extern "C" fn late_chaining_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    HOST_HANDLER_RAN.store(true, Ordering::SeqCst);
    let prev = LATE_PREVIOUS.load(Ordering::SeqCst);
    if prev > 1 {
        // SAFETY: the SDK armed its handler with SA_SIGINFO.
        let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
            unsafe { std::mem::transmute(prev) };
        f(sig, info, ctx);
    }
}

fn set_action(sig: libc::c_int, handler: usize, flags: libc::c_int) -> usize {
    // SAFETY: a zeroed sigaction filled in with a plain handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler;
        sa.sa_flags = flags;
        libc::sigemptyset(&mut sa.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        assert_eq!(libc::sigaction(sig, &sa, &mut old), 0);
        old.sa_sigaction
    }
}

fn wait_for_file(path: &Path, limit: Duration) -> bool {
    let start = Instant::now();
    while !path.exists() {
        if start.elapsed() > limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    true
}

/// Not a test by itself: the body of every child. A plain run does nothing.
#[test]
fn child_entry() {
    let Ok(kind) = std::env::var(KIND_ENV) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR_ENV).unwrap());
    let ready = || std::fs::write(dir.join("ready"), b"1").unwrap();
    let term_ms: u64 = std::env::var(TERM_MS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);

    match kind.as_str() {
        "sigterm_host_handler" => {
            set_action(libc::SIGTERM, host_handler as *const () as usize, 0);
        }
        "sigterm_oneshot_handler" => {
            // A host's one-shot handler: it must run for THIS signal and not be
            // turned into a kill by the SDK's own default-disposition logic.
            set_action(
                libc::SIGTERM,
                host_handler as *const () as usize,
                libc::SA_RESETHAND,
            );
        }
        "sigterm_siginfo_handler" => {
            set_action(
                libc::SIGTERM,
                host_siginfo_handler as *const () as usize,
                libc::SA_SIGINFO,
            );
        }
        "sigterm_ignored" => {
            set_action(libc::SIGTERM, libc::SIG_IGN, 0);
        }
        _ => {}
    }
    let mut options = LaunchOptions::new("APP_TOKEN")
        .data_dir(dir.join("data"))
        .native_crash_capture(false)
        .with_transport(Arc::new(FileTransport {
            dir: dir.clone(),
            fail: std::env::var(FAIL_ENV).is_ok(),
            slow: std::env::var(SLOW_ENV).is_ok(),
            n: AtomicUsize::new(0),
        }));
    if kind == "exit_flush" {
        options = options.flush_at_exit(Duration::from_secs(2));
    }
    let guard = Bugsee::launch_with(options).expect("launch");
    let hooked = kind.starts_with("sigterm") || kind.starts_with("fork");
    if hooked && kind != "sigterm_no_hook" {
        let t = Duration::from_millis(term_ms);
        Bugsee::install_termination_flush(t).expect("install");
        Bugsee::install_termination_flush(t).expect("install twice (idempotent)");
    }
    if kind == "sigterm_late_handler" {
        let old = set_action(
            libc::SIGTERM,
            late_chaining_handler as *const () as usize,
            libc::SA_SIGINFO,
        );
        LATE_PREVIOUS.store(old, Ordering::SeqCst);
    }
    Bugsee::capture_message(LogLevel::Error, PENDING);

    // A host that decides for itself: wait for its handler, then leave through
    // the normal path.
    let exit_via_host = |guard: bugsee::LaunchGuard| -> ! {
        let start = Instant::now();
        while !HOST_HANDLER_RAN.load(Ordering::SeqCst) {
            assert!(start.elapsed() < Duration::from_secs(60), "no signal");
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(dir.join("host-handler-ran"), b"1").unwrap();
        // Let the SDK's own handling (a flush) finish first, as a real host
        // would while it winds down.
        std::thread::sleep(Duration::from_millis(300));
        drop(guard);
        std::process::exit(0);
    };

    match kind.as_str() {
        // The signal must be what persists the report: no flush beforehand.
        "sigterm" => {
            ready();
            std::thread::sleep(Duration::from_secs(60));
            std::process::exit(3); // the signal should have ended us
        }
        "sigterm_host_handler"
        | "sigterm_siginfo_handler"
        | "sigterm_oneshot_handler"
        | "sigterm_late_handler" => {
            ready();
            exit_via_host(guard)
        }
        // Ignored at install: the SDK must leave it ignored.
        "sigterm_ignored" => {
            ready();
            std::thread::sleep(Duration::from_secs(3));
            drop(guard);
            std::process::exit(0);
        }
        // A stop while an upload hangs in the transport: it must neither wait
        // out the upload nor leave the session looking like a crash.
        "slow_transport" => {
            ready();
            std::thread::sleep(Duration::from_millis(500)); // uploader is in the call
            let start = Instant::now();
            drop(guard);
            std::fs::write(dir.join("stop-ms"), start.elapsed().as_millis().to_string()).unwrap();
            std::process::exit(0);
        }
        // A fork child with and without re-arming the hook.
        "fork_plain" | "fork_reinstall" => {
            Bugsee::flush(Duration::from_secs(1));
            // SAFETY: the grandchild only touches the SDK and `_exit`s/gets killed.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                if kind == "fork_reinstall" {
                    Bugsee::install_termination_flush(Duration::from_secs(1)).expect("re-arm");
                }
                std::fs::write(dir.join("grandchild-ready"), b"1").unwrap();
                std::thread::sleep(Duration::from_secs(60));
                // SAFETY: never return into the harness copy.
                unsafe { libc::_exit(7) };
            }
            assert!(wait_for_file(
                &dir.join("grandchild-ready"),
                Duration::from_secs(10)
            ));
            // SAFETY: signalling the process forked above.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let start = Instant::now();
            let mut status = 0;
            let result = loop {
                // SAFETY: waiting for our own child.
                let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if r == pid {
                    break if libc::WIFSIGNALED(status) {
                        format!("signaled:{}", libc::WTERMSIG(status))
                    } else {
                        format!("exited:{}", libc::WEXITSTATUS(status))
                    };
                }
                if start.elapsed() > Duration::from_secs(15) {
                    // SAFETY: killing our own stuck child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    unsafe { libc::waitpid(pid, &mut status, 0) };
                    break "hung".to_string();
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            std::fs::write(dir.join("grandchild-status"), result).unwrap();
            ready();
            drop(guard);
            std::process::exit(0);
        }
        // Reports are durably queued before the process ends the given way.
        "sigterm_no_hook" | "sigterm_stuck" | "kill" | "exit_plain" | "exit_flush" | "return" => {
            Bugsee::flush(Duration::from_secs(1));
            ready();
            match kind.as_str() {
                "exit_plain" | "exit_flush" => std::process::exit(0),
                "return" => {
                    drop(guard);
                    std::process::exit(0);
                }
                _ => std::thread::sleep(Duration::from_secs(60)),
            }
        }
        other => panic!("unknown kind {other}"),
    }
}

// ---------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------

struct Outcome {
    status: std::process::ExitStatus,
    stderr: String,
    dir: PathBuf,
    child_delivered: Vec<Value>,
    /// From the first signal sent to the child's end.
    after_signal: Duration,
}

impl Drop for Outcome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn run_child(kind: &str, env: &[(&str, &str)], signals: &[libc::c_int]) -> Outcome {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-shutdown-{kind}-{}",
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(dir.join("out")).unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["child_entry", "--exact", "--nocapture", "--test-threads=1"])
        .env(KIND_ENV, kind)
        .env(DIR_ENV, &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn");

    let start = Instant::now();
    while !dir.join("ready").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child died before it was ready"
        );
        assert!(start.elapsed() < Duration::from_secs(30), "child not ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    let signalled = Instant::now();
    for (i, sig) in signals.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        // SAFETY: signalling a child this test spawned.
        unsafe { libc::kill(child.id() as libc::pid_t, *sig) };
    }
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if signalled.elapsed() > Duration::from_secs(40) {
            let _ = child.kill();
            panic!("child did not end");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let after_signal = signalled.elapsed();
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let child_delivered = delivered_by_child(&dir);
    Outcome {
        status,
        stderr,
        dir,
        child_delivered,
        after_signal,
    }
}

/// Launch over the child's data directory and collect what recovery delivers.
/// `flush` queues behind recovery on the same worker, so no sleeping is needed.
fn relaunch(dir: &Path) -> Vec<Value> {
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir.join("data"))
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");
    assert!(Bugsee::flush(Duration::from_secs(10)), "queue drains");
    let out = mock
        .uploaded_bundles
        .lock()
        .unwrap()
        .iter()
        .map(|b| crash_json(b))
        .collect();
    drop(guard);
    out
}

fn count(reports: &[Value], needle: &str) -> usize {
    reports
        .iter()
        .filter(|r| r.to_string().contains(needle))
        .count()
}

const FAIL: &[(&str, &str)] = &[(FAIL_ENV, "1")];

/// Run one case; returns the outcome and `(pending, abnormal)` counts over
/// everything the child delivered plus the first relaunch. A second relaunch
/// must deliver nothing more.
fn run_case(kind: &str, env: &[(&str, &str)], signals: &[libc::c_int]) -> (Outcome, usize, usize) {
    let _serial = serial();
    let out = run_child(kind, env, signals);
    let recovered = relaunch(&out.dir);
    let again = relaunch(&out.dir);
    assert!(
        again.is_empty(),
        "[{kind}] reported again on the following launch: {again:?}"
    );
    let mut all = out.child_delivered.clone();
    all.extend(recovered);
    let pending = count(&all, PENDING);
    let abnormal = count(&all, ABNORMAL);
    (out, pending, abnormal)
}

#[test]
fn returning_from_main_is_a_clean_stop() {
    let (out, pending, abnormal) = run_case("return", FAIL, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1, "the queued report is delivered exactly once");
    assert_eq!(abnormal, 0, "a deliberate stop is not a crash");
}

#[test]
fn exit_without_stop_is_an_abnormal_exit_unless_opted_in() {
    // Default: nothing in the process can tell `exit` from a death, so it is
    // reported as abnormal. The queued report is still not lost.
    let (out, pending, abnormal) = run_case("exit_plain", FAIL, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);

    // `flush_at_exit`: the same exit is deliberate.
    let (out, pending, abnormal) = run_case("exit_flush", FAIL, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn sigkill_loses_neither_the_queue_nor_the_abnormal_exit() {
    let (out, pending, abnormal) = run_case("kill", FAIL, &[libc::SIGKILL]);
    assert_eq!(out.status.signal(), Some(libc::SIGKILL));
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);
}

#[test]
fn sigterm_without_the_hook_is_untouched() {
    let (out, pending, abnormal) = run_case("sigterm_no_hook", FAIL, &[libc::SIGTERM]);
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 1);
}

#[test]
fn sigterm_and_sigint_with_the_hook_flush_then_die_of_the_signal() {
    // The backend is unreachable, so the report can only survive in the queue,
    // and only if the signal made the SDK persist it before the process ended.
    for sig in [libc::SIGTERM, libc::SIGINT] {
        let (out, pending, abnormal) = run_case("sigterm", FAIL, &[sig]);
        assert_eq!(
            out.status.signal(),
            Some(sig),
            "the process must still die of the signal, not exit: {:?} {}",
            out.status,
            out.stderr
        );
        assert_eq!(pending, 1, "delivered or queued, exactly once");
        assert_eq!(abnormal, 0, "a handled termination is a deliberate stop");
    }
}

#[test]
fn sigterm_with_the_hook_delivers_when_the_backend_is_up() {
    let (out, pending, abnormal) = run_case("sigterm", &[], &[libc::SIGTERM]);
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert_eq!(
        count(&out.child_delivered, PENDING),
        1,
        "delivered pre-exit"
    );
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn a_hosts_own_handler_still_runs_and_decides() {
    // Plain, SA_SIGINFO (must get a real siginfo) and one-shot SA_RESETHAND.
    for kind in [
        "sigterm_host_handler",
        "sigterm_siginfo_handler",
        "sigterm_oneshot_handler",
    ] {
        let (out, pending, abnormal) = run_case(kind, FAIL, &[libc::SIGTERM]);
        assert!(
            out.dir.join("host-handler-ran").exists(),
            "[{kind}] the previous handler was swallowed: {}",
            out.stderr
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "[{kind}] the host chose to exit normally; the SDK must not kill it: {:?} {}",
            out.status,
            out.stderr
        );
        assert_eq!(pending, 1, "[{kind}]");
        assert_eq!(abnormal, 0, "[{kind}]");
    }
}

#[test]
fn a_handler_installed_after_ours_is_never_killed_by_us() {
    // tokio / signal-hook style: the host takes the signal over after the SDK
    // and chains to us as its previous handler. We see "default" behind us, but
    // must notice we are no longer in force and not terminate the process.
    let (out, pending, abnormal) = run_case("sigterm_late_handler", FAIL, &[libc::SIGTERM]);
    assert!(out.dir.join("host-handler-ran").exists(), "{}", out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the host owns the signal; the SDK killed it: {:?} {}",
        out.status,
        out.stderr
    );
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn a_signal_ignored_at_install_stays_ignored() {
    let (out, pending, abnormal) = run_case("sigterm_ignored", FAIL, &[libc::SIGTERM]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "an ignored SIGTERM must not terminate: {:?} {}",
        out.status,
        out.stderr
    );
    assert_eq!(pending, 1);
    assert_eq!(abnormal, 0);
}

#[test]
fn a_second_signal_does_not_wait_out_the_flush() {
    // A 20 s flush against an unreachable backend; the operator presses Ctrl+C
    // twice.
    let env = [(FAIL_ENV, "1"), (TERM_MS_ENV, "20000")];
    let (out, pending, _abnormal) =
        run_case("sigterm_stuck", &env, &[libc::SIGTERM, libc::SIGTERM]);
    assert_eq!(out.status.signal(), Some(libc::SIGTERM));
    assert!(
        out.after_signal < Duration::from_secs(10),
        "took {:?}",
        out.after_signal
    );
    assert_eq!(pending, 1, "the queue was durable before the signal");
}

#[test]
fn a_stop_during_a_hanging_upload_is_still_a_clean_stop() {
    let env = [(SLOW_ENV, "1")];
    let (out, pending, abnormal) = run_case("slow_transport", &env, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", out.stderr);
    let stop_ms: u64 = std::fs::read_to_string(out.dir.join("stop-ms"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        stop_ms < 8000,
        "stop waited on the hanging upload: {stop_ms} ms"
    );
    assert_eq!(abnormal, 0, "an upload in flight is not a crashed session");
    assert_eq!(pending, 1, "the abandoned report is re-sent next launch");
}

#[test]
fn a_fork_child_neither_hangs_nor_loses_the_signal() {
    // Without re-arming, the inherited handler has no watcher thread: it must
    // terminate the child like the default would. With re-arming, the child's
    // own watcher flushes and then terminates it.
    for kind in ["fork_plain", "fork_reinstall"] {
        let (out, pending, abnormal) = run_case(kind, FAIL, &[]);
        assert_eq!(out.status.code(), Some(0), "[{kind}] {}", out.stderr);
        let status = std::fs::read_to_string(out.dir.join("grandchild-status")).unwrap();
        assert_eq!(
            status,
            format!("signaled:{}", libc::SIGTERM),
            "[{kind}] the forked child must die of SIGTERM"
        );
        assert_eq!(pending, 1, "[{kind}]");
        assert_eq!(abnormal, 0, "[{kind}]");
    }
}

#[test]
fn shutdown_is_idempotent() {
    let _serial = serial();
    let dir = std::env::temp_dir().join(format!(
        "bugsee-shutdown-idem-{}",
        bugsee::core::util::random_hex(6)
    ));
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(&dir)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("launch");
    Bugsee::capture_message(LogLevel::Error, PENDING);
    assert!(Bugsee::shutdown(Duration::from_secs(10)), "drained");
    assert!(!Bugsee::is_launched());
    // Everything after the first call is a no-op, including the guard's Drop.
    assert!(!Bugsee::shutdown(Duration::from_secs(1)));
    Bugsee::stop();
    drop(guard);
    assert_eq!(mock.uploaded_bundles.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

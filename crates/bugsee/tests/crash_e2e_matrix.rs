//
//  crash_e2e_matrix.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Every way a Rust process realistically dies, driven through the WHOLE client
//! path: launch the SDK, die for real in a child process, relaunch, recover, and
//! assert on the `crash.json` that would be delivered.
//!
//! The lower layers are covered separately — `bugsee-native/tests/crash_matrix.rs`
//! pins what the handler writes, `bugsee-core/tests/recovery_e2e.rs` pins what
//! recovery does with hand-written markers. Neither proves the pair, per case, and
//! the cases differ in exactly the ways that matter: a stack overflow leaves the
//! handler almost no stack, an aborting panic must correlate into ONE event, an
//! unwinding panic never produces a signal at all.
//!
//! `harness = false` (see `Cargo.toml`): the child must die from the real main
//! thread, which libtest's spawned test threads are not. This binary therefore
//! prints its own results and exits non-zero on any failure.

#![cfg(all(feature = "native", any(unix, windows)))]

use std::io::{Cursor, Read};
use std::sync::Arc;
use std::time::Duration;

use bugsee::core::MockTransport;
use bugsee::{Bugsee, LaunchOptions};
use serde_json::Value;
use zip::ZipArchive;

const KIND_ENV: &str = "BUGSEE_CRASH_E2E_KIND";
const DIR_ENV: &str = "BUGSEE_CRASH_E2E_DIR";
const PANIC_TEXT: &str = "bugsee-e2e-deliberate-panic";

// ---------------------------------------------------------------------------
// The child: launch, then die in the requested way.
// ---------------------------------------------------------------------------

#[inline(never)]
fn fault_null() {
    // SAFETY: deliberately faulting.
    unsafe { std::ptr::write_volatile(std::hint::black_box(std::ptr::null_mut::<u8>()), 1) }
}

#[inline(never)]
#[allow(unconditional_recursion)]
fn recurse(n: u64) -> u64 {
    let pad = [n as u8; 256];
    std::hint::black_box(&pad);
    recurse(n + 1) + pad[0] as u64
}

#[inline(never)]
fn fault_illegal_instruction() {
    // SAFETY: deliberately faulting.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        std::arch::asm!("ud2");
        #[cfg(target_arch = "aarch64")]
        std::arch::asm!("udf #0");
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
fn fault_divide_by_zero() {
    // SAFETY: deliberately faulting.
    unsafe {
        std::arch::asm!(
            "xor edx, edx",
            "xor ecx, ecx",
            "mov eax, 1",
            "div ecx",
            out("eax") _, out("ecx") _, out("edx") _,
        );
    }
}

fn run_child(kind: &str, dir: &str) -> ! {
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir)
            .native_crash_capture(true)
            // The child never delivers; the parent recovers and delivers.
            .with_transport(Arc::new(MockTransport::default())),
    )
    .expect("launch");

    // Let the recorder establish its generation and liveness marker first, so
    // the parent sees a session that ended abnormally.
    std::thread::sleep(Duration::from_millis(300));

    match kind {
        "segv" => fault_null(),
        "segv_thread" => {
            let _ = std::thread::spawn(fault_null).join();
        }
        "stack_overflow" => {
            recurse(0);
        }
        "stack_overflow_thread" => {
            let _ = std::thread::spawn(|| {
                recurse(0);
            })
            .join();
        }
        "illegal_instruction" => fault_illegal_instruction(),
        #[cfg(target_arch = "x86_64")]
        "divide_by_zero" => fault_divide_by_zero(),
        "abort" => std::process::abort(),
        // The Rust default: an uncaught panic UNWINDS out of main, dropping the
        // guard on the way. No signal is ever raised.
        "unwinding_panic" => {
            let _keep = &guard;
            panic!("{PANIC_TEXT}");
        }
        // What `panic = "abort"` does: the panic hook runs (the SDK snapshots the
        // panic), then the runtime aborts. Emulated with a chained hook because
        // the profile is per-build, not per-test.
        "aborting_panic" => {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                previous(info);
                std::process::abort();
            }));
            panic!("{PANIC_TEXT}");
        }
        // Dies without running any destructor: no clean shutdown, no signal.
        "exit_without_shutdown" => std::process::exit(3),
        other => panic!("unknown crash kind {other}"),
    }
    // Reached only if the platform failed to fault; exit cleanly so the parent
    // reports "did not die" rather than timing out.
    drop(guard);
    std::process::exit(0);
}

// ---------------------------------------------------------------------------
// The parent: crash it, relaunch it, read what would be delivered.
// ---------------------------------------------------------------------------

/// What a recovered report must look like for a given way of dying.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Thin native variant carrying this signal.
    Native(&'static str),
    /// Managed panic event carrying [`PANIC_TEXT`].
    Panic,
    /// "Application terminated abnormally" — a session that just stopped.
    AbnormalExit,
    /// Platform-dependent: any of these is an acceptable, honest report.
    /// (Constructed only where a platform has more than one honest outcome.)
    #[cfg_attr(unix, allow(dead_code))]
    OneOf(&'static [Expect]),
}

struct Case {
    kind: &'static str,
    expect: Expect,
}

fn cases() -> Vec<Case> {
    let mut v = vec![
        Case {
            kind: "segv",
            expect: Expect::Native("SIGSEGV"),
        },
        Case {
            kind: "segv_thread",
            expect: Expect::Native("SIGSEGV"),
        },
        // A stack overflow is a memory fault on every platform (Windows reports
        // STATUS_STACK_OVERFLOW as SIGSEGV on purpose).
        Case {
            kind: "stack_overflow",
            expect: Expect::Native("SIGSEGV"),
        },
        Case {
            kind: "stack_overflow_thread",
            expect: Expect::Native("SIGSEGV"),
        },
        Case {
            kind: "illegal_instruction",
            expect: Expect::Native("SIGILL"),
        },
        Case {
            kind: "unwinding_panic",
            expect: Expect::Panic,
        },
        Case {
            kind: "exit_without_shutdown",
            expect: Expect::AbnormalExit,
        },
    ];
    #[cfg(target_arch = "x86_64")]
    v.push(Case {
        kind: "divide_by_zero",
        expect: Expect::Native("SIGFPE"),
    });

    // `abort()`: POSIX delivers SIGABRT, which the handler sees. Windows' Rust
    // abort is a fast-fail that no in-process handler can intercept, so there it
    // is an abnormal exit — an honest report, just a less specific one.
    #[cfg(unix)]
    v.push(Case {
        kind: "abort",
        expect: Expect::Native("SIGABRT"),
    });
    #[cfg(windows)]
    v.push(Case {
        kind: "abort",
        expect: Expect::OneOf(&[Expect::Native("SIGABRT"), Expect::AbnormalExit]),
    });

    // The panic is what the user needs, however the process then dies: SIGABRT
    // correlation on POSIX, the surviving snapshot on Windows.
    v.push(Case {
        kind: "aborting_panic",
        expect: Expect::Panic,
    });
    v
}

fn crash_json_from(bundle: &[u8]) -> Value {
    let mut zip = ZipArchive::new(Cursor::new(bundle)).expect("bundle is a zip");
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).expect("zip entry");
        if entry.name() == "crash.json" {
            let mut raw = Vec::new();
            entry.read_to_end(&mut raw).expect("read crash.json");
            return serde_json::from_slice(&raw).expect("crash.json parses");
        }
    }
    panic!("no crash.json in the delivered bundle");
}

/// Launch the SDK over `dir` with a mock transport and return what it delivered.
fn relaunch_and_collect(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mock = Arc::new(MockTransport::default());
    let guard = Bugsee::launch_with(
        LaunchOptions::new("APP_TOKEN")
            .data_dir(dir)
            .native_crash_capture(false)
            .with_transport(mock.clone()),
    )
    .expect("relaunch");
    // Recovery runs on the worker thread; flush waits for the queue to drain.
    std::thread::sleep(Duration::from_millis(1500));
    Bugsee::flush(Duration::from_secs(10));
    let bundles = mock.uploaded_bundles.lock().unwrap().clone();
    drop(guard);
    bundles
}

fn check(expect: Expect, crash: &Value) -> Result<(), String> {
    match expect {
        Expect::OneOf(options) => {
            let mut why = Vec::new();
            for o in options {
                match check(*o, crash) {
                    Ok(()) => return Ok(()),
                    Err(e) => why.push(format!("{o:?}: {e}")),
                }
            }
            Err(format!("none of the acceptable shapes matched: {why:?}"))
        }
        Expect::Native(signal) => {
            if crash["exception_type"] != "native" {
                return Err(format!("not a native crash: {crash}"));
            }
            if crash["signal"]["name"] != signal {
                return Err(format!("signal is {}, want {signal}", crash["signal"]));
            }
            // The module map is what makes a native crash symbolicatable at all.
            let modules = crash["modules"].as_array().ok_or("no modules[]")?;
            if !modules
                .iter()
                .any(|m| m["code_id"].as_str().is_some_and(|s| !s.is_empty()))
            {
                return Err("no module carries a code_id: nothing could be symbolicated".into());
            }
            let frames = crash["frames"].as_array().ok_or("no frames[]")?;
            if frames.is_empty() {
                return Err("no frames captured".into());
            }
            if !frames
                .iter()
                .any(|f| f["module"].as_str().is_some_and(|s| !s.is_empty()))
            {
                return Err(format!("no frame resolved to a module: {frames:?}"));
            }
            Ok(())
        }
        Expect::Panic => {
            if crash["exception_type"] != "exception" {
                return Err(format!("not a managed event: {crash}"));
            }
            if crash["exception"]["name"] != "panic" {
                return Err(format!("not a panic: {}", crash["exception"]));
            }
            let reason = crash["exception"]["reason"].as_str().unwrap_or_default();
            if !reason.contains(PANIC_TEXT) {
                return Err(format!("panic message lost: {reason:?}"));
            }
            Ok(())
        }
        Expect::AbnormalExit => {
            if crash["exception"]["name"] != "AppExit" {
                return Err(format!("not an abnormal exit: {}", crash["exception"]));
            }
            Ok(())
        }
    }
}

/// Every `crash.info` under `dir`, flattened onto one line (diagnostics only).
fn crash_info_files(dir: &std::path::Path) -> String {
    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.file_name().is_some_and(|n| n == "crash.info") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                out.push(text.replace('\n', " | "));
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, &mut found);
    if found.is_empty() {
        "<none>".into()
    } else {
        found.join(" ;; ")
    }
}

fn run_case(case: &Case) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!(
        "bugsee-crash-e2e-{}-{}",
        case.kind,
        bugsee::core::util::random_hex(6)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let result = (|| {
        // --- die for real, in a child process ------------------------------
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env(KIND_ENV, case.kind)
            .env(DIR_ENV, &dir)
            .output()
            .map_err(|e| format!("spawn: {e}"))?;
        if output.status.success() {
            return Err(format!(
                "child should have died; stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // What the dying child left on disk, kept for the failure message:
        // recovery consumes it, and "no frames" alone does not say whether the
        // handler never ran, ran without unwinding, or was killed mid-write.
        let left_behind = crash_info_files(&dir);

        // --- relaunch: exactly one usable report ---------------------------
        let bundles = relaunch_and_collect(&dir);
        if bundles.len() != 1 {
            return Err(format!(
                "expected exactly one report, got {} (child stderr: {})",
                bundles.len(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let crash = crash_json_from(&bundles[0]);
        check(case.expect, &crash).map_err(|e| {
            format!(
                "{e}\n    crash.info left by the child: {left_behind}\n    child status: {:?}, stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })?;

        // Common to every report, however the process died.
        if crash["source_sdk"] != "rust" {
            return Err(format!("report is not self-describing: {crash}"));
        }
        if !crash["signatures"]
            .as_array()
            .is_some_and(|s| !s.is_empty())
        {
            return Err("no client signature: a crash loop could not be suppressed".into());
        }

        // --- and it is reported ONCE, not on every launch ------------------
        let again = relaunch_and_collect(&dir);
        if !again.is_empty() {
            return Err(format!(
                "the crash was reported again on the next launch ({} bundles)",
                again.len()
            ));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn main() {
    if let Ok(kind) = std::env::var(KIND_ENV) {
        let dir = std::env::var(DIR_ENV).expect("data dir");
        run_child(&kind, &dir);
    }

    // `cargo test` passes libtest flags (`--nocapture`, a name filter); this
    // binary has no harness, so treat a positional argument as a substring filter.
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let mut failed = Vec::new();
    for case in cases() {
        if filter.as_deref().is_some_and(|f| !case.kind.contains(f)) {
            continue;
        }
        match run_case(&case) {
            Ok(()) => println!("test crash_e2e::{} ... ok", case.kind),
            Err(e) => {
                println!("test crash_e2e::{} ... FAILED\n    {e}", case.kind);
                failed.push(case.kind);
            }
        }
    }
    if !failed.is_empty() {
        println!("\ncrash e2e failures: {failed:?}");
        std::process::exit(1);
    }
}

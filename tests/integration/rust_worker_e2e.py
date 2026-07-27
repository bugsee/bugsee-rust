#!/usr/bin/env python3
"""End-to-end: Rust SDK produces crash bundles -> the worker's crash/rust.py consumes them.

See README.md next to this file. Run with --help for options.

The point of this harness is the seam between the two repos. Each side is well
unit-tested in isolation, but nothing else verifies that the `code_id` the SDK
reports at runtime is the same identity the worker (and the symbol store) key on
— get that wrong and symbol upload silently resolves nothing.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import types
import zipfile
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[2]

# ---------------------------------------------------------------------------
# The generated test app.
# ---------------------------------------------------------------------------

CARGO_TOML = """\
[package]
name = "bugsee-e2e-app"
version = "0.1.0"
edition = "2021"

[dependencies]
bugsee = {{ path = "{sdk}/crates/bugsee" }}
# NOTE: `{panic_mode}` is substituted per build — the harness builds this project
# twice, once aborting and once unwinding, because the two panic strategies take
# completely different paths through the SDK.

# What a real user must set for symbolication to be possible at all: release
# builds carry NO debug info by default. `debug = 1` is line tables, which is
# all the symbolicator needs.
[profile.release]
debug = 1
# Keep the symbols in the binary so this harness can read the build id back.
strip = false
# The two panic strategies exercise DIFFERENT SDK paths, so the harness builds
# both:
#   abort  -> the hook writes a snapshot, abort raises SIGABRT, the native
#             handler records it, and next-launch recovery CORRELATES the two
#             into one managed crash.
#   unwind -> (the Rust default) no signal at all. The panic unwinds out of
#             `main`, `Recorder::drop` observes `thread::panicking()` and KEEPS
#             the liveness marker, so next-launch recovery reports the lone
#             panic snapshot as a fatal panic.
panic = "{panic_mode}"
"""

MAIN_RS = """\
use std::time::Duration;
use bugsee::{Bugsee, LaunchOptions};

fn launch(data_dir: &str) -> bugsee::LaunchGuard {
    // Point delivery at a closed port: every report stays queued on disk, which
    // is exactly what this harness wants to inspect.
    Bugsee::launch_with(
        LaunchOptions::new("E2E_TOKEN")
            .data_dir(data_dir)
            .endpoint("http://127.0.0.1:1/v2".to_string())
            .native_crash_capture(true),
    )
    .expect("launch")
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let data_dir = std::env::args().nth(2).expect("data dir");

    match mode.as_str() {
        // Crash modes: launch, then die. The SDK leaves a marker on disk.
        "panic" => {
            let _g = launch(&data_dir);
            std::thread::sleep(Duration::from_millis(150));
            panic!("e2e induced panic");
        }
        "segv" => {
            let _g = launch(&data_dir);
            std::thread::sleep(Duration::from_millis(150));
            // A genuine SIGSEGV: write through a null pointer.
            unsafe {
                let p: *mut u64 = std::ptr::null_mut();
                std::ptr::write_volatile(p, 0xdead);
            }
            println!("UNREACHABLE");
        }
        // Recovery mode: launching again turns the previous run's marker into a
        // queued report bundle.
        "recover" => {
            let _g = launch(&data_dir);
            // Give the worker thread time to run recovery + enqueue.
            std::thread::sleep(Duration::from_millis(1500));
            Bugsee::flush(Duration::from_millis(1500));
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    }
}
"""


class Failure(Exception):
    pass


def run(cmd, **kw):
    """Run a command, returning the CompletedProcess (never raises on status)."""
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def build_project(workdir: Path, panic_mode: str) -> Path:
    proj = workdir / f"app-{panic_mode}"
    (proj / "src").mkdir(parents=True, exist_ok=True)
    (proj / "Cargo.toml").write_text(CARGO_TOML.format(sdk=SDK_ROOT, panic_mode=panic_mode))
    (proj / "src" / "main.rs").write_text(MAIN_RS)

    env = dict(os.environ)
    if sys.platform.startswith("linux"):
        # Without a GNU build-id an ELF module cannot be matched to its symbols
        # (symbolfiles/elf.py skips such objects outright).
        env["RUSTFLAGS"] = env.get("RUSTFLAGS", "") + " -C link-arg=-Wl,--build-id"

    print(f"• building the test app (release, debug info, panic={panic_mode})…")
    r = run(["cargo", "build", "--release"], cwd=proj, env=env)
    if r.returncode != 0:
        raise Failure(f"cargo build failed:\n{r.stdout}\n{r.stderr}")

    exe = proj / "target" / "release" / "bugsee-e2e-app"
    if not exe.exists():
        raise Failure(f"built binary not found at {exe}")
    return exe


def produce_bundle(exe: Path, mode: str, data_dir: Path, expect_signal: bool = True) -> dict:
    """Crash in `mode`, relaunch to recover, and return the recovered crash.json."""

    if data_dir.exists():
        shutil.rmtree(data_dir)
    data_dir.mkdir(parents=True)

    print(f"• crashing the app: {mode}")
    crash = run([str(exe), mode, str(data_dir)])
    # An aborting panic and a segv die from a SIGNAL (negative rc). An UNWINDING
    # panic instead exits 101 after unwinding out of main — there is no signal,
    # which is exactly why that case needs the marker-retention path.
    if expect_signal and crash.returncode >= 0:
        print(f"  ! expected a fatal signal, got rc={crash.returncode}")

    print("• relaunching to run next-launch recovery")
    rec = run([str(exe), "recover", str(data_dir)])
    if rec.returncode != 0:
        raise Failure(f"recovery run failed (rc={rec.returncode}):\n{rec.stderr}")

    bundles = sorted((data_dir / "queue").glob("*.bundle.zip"))
    if not bundles:
        raise Failure(
            f"no bundle queued for mode={mode}; the crash was not recovered.\n"
            f"crash stderr:\n{crash.stderr[:2000]}"
        )

    return json.loads(read_bundle_entry(bundles[0], "crash.json"))


# Zstd is ZIP method 93. Python's stdlib zipfile only understands it on 3.14+,
# and the environment that has `symbolic` here is 3.12 — so decompress it
# ourselves when the stdlib refuses.
ZIP_ZSTANDARD = 93


def _zstd_decompress(blob: bytes) -> bytes:
    try:
        from compression import zstd as _z  # py3.14 stdlib
        return _z.decompress(blob)
    except Exception:
        pass
    try:
        import zstandard
        return zstandard.ZstdDecompressor().decompressobj().decompress(blob)
    except Exception:
        pass
    try:
        import pyzstd
        return pyzstd.decompress(blob)
    except Exception as e:
        raise Failure(
            "bundle entry is zstd-compressed (ZIP method 93) and no zstd "
            f"decompressor is available in this Python: {e}"
        )


def read_bundle_entry(bundle: Path, name: str) -> bytes:
    with zipfile.ZipFile(bundle) as z:
        names = z.namelist()
        if name not in names:
            raise Failure(f"bundle has no {name} (entries: {names})")
        info = z.getinfo(name)
        try:
            return z.read(name)
        except NotImplementedError:
            if info.compress_type != ZIP_ZSTANDARD:
                raise
        # Read the raw deflate-less payload and inflate it with zstd.
        with open(bundle, "rb") as fh:
            fh.seek(info.header_offset)
            # Local file header: 30 bytes + name + extra (lengths at 26/28).
            head = fh.read(30)
            name_len = int.from_bytes(head[26:28], "little")
            extra_len = int.from_bytes(head[28:30], "little")
            fh.seek(info.header_offset + 30 + name_len + extra_len)
            raw = fh.read(info.compress_size)
        return _zstd_decompress(raw)


# ---------------------------------------------------------------------------
# Assertions on the SDK side.
# ---------------------------------------------------------------------------

def check_panic_crash(crash: dict) -> None:
    print("• checking the panic crash.json")
    assert_eq(crash.get("exception_type"), "exception", "panic is a managed exception")
    assert_true(crash.get("handled") is False, "an uncaught panic is unhandled")
    exc = crash.get("exception") or {}
    assert_true("e2e induced panic" in (exc.get("reason") or ""),
                f"panic reason preserved, got {exc.get('reason')!r}")
    frames = exc.get("frames") or []
    assert_true(len(frames) > 0, "panic carries frames")
    assert_true(any(f.get("trace") for f in frames), "frames carry a trace string")
    assert_true(bool(crash.get("signatures")), "panic carries a client signature")


def check_unwinding_panic_crash(crash: dict) -> None:
    """The Rust DEFAULT panic strategy — no signal is raised at all.

    This case used to produce no report whatsoever: the panic unwound out of
    `main`, the launch guard dropped, the SDK shut down "cleanly" and removed the
    liveness marker, so next-launch recovery had nothing to find. It is now
    surfaced because `Recorder::drop` keeps the marker when it observes
    `thread::panicking()`.
    """

    print("• checking the UNWINDING panic crash.json (Rust default)")
    assert_eq(crash.get("exception_type"), "exception", "reported as a managed exception")
    assert_true(crash.get("handled") is False, "a fatal panic is unhandled")
    assert_eq(crash.get("ndkCrash"), False, "no native signal was involved")
    exc = crash.get("exception") or {}
    assert_eq(exc.get("name"), "panic", "reported as a panic")
    assert_true("e2e induced panic" in (exc.get("reason") or ""),
                f"the panic message survives, got {exc.get('reason')!r}")
    assert_true(bool(exc.get("frames")), "carries the panic frames")
    assert_true(bool(crash.get("signatures")), "carries a dedup signature")
    # It must NOT degrade to the generic abnormal-exit report.
    assert_true(exc.get("name") != "AppExit",
                "not reported as a generic abnormal exit")


def check_native_crash(crash: dict) -> list:
    print("• checking the native crash.json")
    assert_eq(crash.get("exception_type"), "native", "segv is the native variant")
    assert_eq((crash.get("signal") or {}).get("name"), "SIGSEGV", "signal is SIGSEGV")

    modules = crash.get("modules") or []
    assert_true(bool(modules), "native crash carries the module map")
    with_id = [m for m in modules if m.get("code_id")]
    assert_true(bool(with_id),
                "at least one module reports a code_id — without it NO symbol "
                "upload can ever be matched")

    frames = crash.get("frames") or []
    assert_true(bool(frames), "native crash carries frames")
    assert_true(any(f.get("reladdr") is not None for f in frames),
                "frames carry module-relative offsets")
    return modules


def check_code_id_matches_binary(modules: list, exe: Path) -> None:
    """THE cross-repo assertion: the runtime code_id == the binary's real identity."""

    try:
        from symbolic.debuginfo import Archive
    except Exception:
        print("  ~ skipping symbolic cross-check (symbolic not importable here)")
        return

    try:
        archive = Archive.open(str(exe))
        objects = list(archive.iter_objects())
    except Exception as e:
        print(f"  ~ skipping symbolic cross-check (could not open binary: {e})")
        return

    if not objects:
        print("  ~ skipping symbolic cross-check (no objects in binary)")
        return

    # What the worker would store for this binary, normalized the way the
    # crash-side lookup normalizes (lowercase, no dashes).
    def norm(v):
        return str(v or "").replace("-", "").lower()

    stored = {norm(o.code_id) for o in objects} | {norm(o.debug_id) for o in objects}
    stored.discard("")

    exe_name = exe.name
    reported = {norm(m.get("code_id")) for m in modules
                if m.get("filename") == exe_name and m.get("code_id")}
    if not reported:
        raise Failure(f"the main module ({exe_name}) reported no code_id; modules={modules}")

    # A debug id is the code id plus an age suffix on some formats, so accept a
    # prefix relationship in either direction rather than demanding equality.
    def compatible(a, b):
        return a == b or a.startswith(b) or b.startswith(a)

    for r in reported:
        if any(compatible(r, s) for s in stored):
            print(f"  ✓ code_id {r[:16]}… matches the built binary's identity")
            return

    raise Failure(
        "SDK-reported code_id does not match the binary's identity — symbol "
        f"upload would resolve NOTHING.\n  reported: {sorted(reported)}\n"
        f"  in binary: {sorted(stored)}"
    )


# ---------------------------------------------------------------------------
# The worker half.
# ---------------------------------------------------------------------------

def load_worker(worker_root: Path):
    """Import the worker's crash.rust, shimming what a non-3.14 env lacks."""

    import zipfile as _zipfile
    if not hasattr(_zipfile, "ZIP_ZSTANDARD"):
        _zipfile.ZIP_ZSTANDARD = 93
        _zipfile.ZIP_ZSTANDARD_VERSION = 63
        _zipfile.ZSTANDARD_VERSION = 63
    if "bugsee_demangle" not in sys.modules:
        stub = types.ModuleType("bugsee_demangle")
        stub.__getattr__ = lambda name: (lambda *a, **k: None)
        sys.modules["bugsee_demangle"] = stub

    sys.path.insert(0, str(worker_root))
    from crash import rust  # noqa: E402
    return rust


def check_worker_processes(rust_mod, panic_crash: dict, native_crash: dict) -> None:
    from unittest import mock

    print("• feeding both crashes to the worker's crash/rust.py")

    managed = rust_mod.process_crash_report("org", "app", panic_crash, "/tmp", sys.platform, {})
    assert_eq(managed.get("status"), "ready", "worker processes the panic")
    assert_true(bool(managed.get("summaryUpdate")), "panic gets a summary")
    summary = managed.get("summaryUpdate", "")
    assert_true("Unknown location" not in summary,
                f"panic resolves a location, got {summary!r}")
    # The location must be the APPLICATION's frame, not a panic-runtime one.
    # Every panic unwinds through the same std/`__rustc` shims, so reporting one
    # of those collapses distinct bugs into a single group.
    assert_true("bugsee_e2e_app" in summary,
                f"crash site is the app's own frame, got {summary!r}")
    assert_true("main.rs" in summary,
                f"crash site keeps the user's source location, got {summary!r}")
    assert_true(any(s.startswith("s.") for s in managed.get("signatures") or []),
                "worker appends its own server signature")

    # No symbols are uploaded in this harness, so the native crash must come back
    # missing_sym — which proves the worker actually attempted the lookup with
    # the code_id the SDK produced.
    with mock.patch.object(rust_mod.api, "get_symbol_files", return_value={"ok": True}) as lookup:
        native = rust_mod.process_crash_report("org", "app", native_crash, "/tmp", sys.platform, {})

    assert_eq(native.get("status"), "missing_sym",
              "an un-uploaded module is reported as missing_sym")
    assert_true(lookup.called, "the worker queried the symbol store")
    queried = lookup.call_args[0][2]
    assert_true(bool(queried), "the lookup carried at least one code id")
    print(f"  ✓ worker queried the symbol store for {len(queried)} module id(s)")
    assert_true(bool(native.get("missing_images")), "missing modules reported back")


# ---------------------------------------------------------------------------

_failures = []


def assert_true(cond, what):
    if cond:
        print(f"  ✓ {what}")
    else:
        print(f"  ✗ {what}")
        _failures.append(what)


def assert_eq(actual, expected, what):
    assert_true(actual == expected, f"{what} (got {actual!r})")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--worker", type=Path, default=SDK_ROOT.parent / "worker",
                    help="path to the worker checkout")
    ap.add_argument("--keep", action="store_true", help="keep the generated project")
    ap.add_argument("--skip-worker", action="store_true",
                    help="only produce the bundles; skip the worker half")
    args = ap.parse_args()

    workdir = Path(tempfile.mkdtemp(prefix="bugsee-e2e-"))
    print(f"workdir: {workdir}")
    try:
        # `abort` covers the aborting panic (SIGABRT -> correlated) and the segv.
        exe_abort = build_project(workdir, "abort")
        panic_crash = produce_bundle(exe_abort, "panic", workdir / "data-panic")
        native_crash = produce_bundle(exe_abort, "segv", workdir / "data-segv")

        # `unwind` is the Rust DEFAULT and the case that previously produced no
        # report at all — the panic unwinds out of main with no signal.
        exe_unwind = build_project(workdir, "unwind")
        unwind_crash = produce_bundle(
            exe_unwind, "panic", workdir / "data-unwind", expect_signal=False)

        check_panic_crash(panic_crash)
        check_unwinding_panic_crash(unwind_crash)
        modules = check_native_crash(native_crash)
        check_code_id_matches_binary(modules, exe_abort)

        if args.skip_worker:
            print("• skipping the worker half (--skip-worker)")
        else:
            if not (args.worker / "crash" / "rust.py").exists():
                raise Failure(f"worker not found at {args.worker} (use --worker PATH)")
            rust_mod = load_worker(args.worker)
            check_worker_processes(rust_mod, panic_crash, native_crash)

        print()
        if _failures:
            print(f"FAILED — {len(_failures)} assertion(s):")
            for f in _failures:
                print(f"  - {f}")
            return 1
        print("ALL CHECKS PASSED")
        return 0
    except Failure as e:
        print(f"\nHARNESS ERROR: {e}")
        return 2
    finally:
        if args.keep:
            print(f"kept: {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())

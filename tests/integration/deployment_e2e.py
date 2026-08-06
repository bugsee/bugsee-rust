#!/usr/bin/env python3
"""End-to-end against a REAL Bugsee deployment (default: apidev.bugsee.com).

This is the half `rust_worker_e2e.py` cannot cover. That harness feeds crash
documents to worker code running in this process, with the endpoint pinned at
`http://127.0.0.1:1/v2` — a deliberately dead address — so it proves the
documents parse and symbolicate but never exercises transport, auth, the
symbol-upload API, the symbol-store lookup, or grouping.

This script drives the whole path a real user does:

    build (with debug info + a build id)
      -> upload symbols with bugsee-cli
      -> crash the binary against the deployment
      -> relaunch so next-launch recovery delivers the bundle
      -> repeat, so grouping has two events to merge

It deliberately does NOT assert on the result. Whether the frames resolved and
whether the two runs grouped are facts that live in the deployment's database,
so the script ends by printing exactly what to check and the `code_id` to match
against. Read it back with the Bugsee MCP tools (`list_issues` / `get_issue`)
or the dashboard.

CONFIGURATION — all via environment, so no credential is ever passed on a
command line (where it would land in shell history and `ps`):

    BUGSEE_APP_TOKEN   required. The app token for a **Rust-type** application.
    BUGSEE_ENDPOINT    optional. The BASE url. Default https://apidev.bugsee.com
    BUGSEE_CLI         optional. Path to the bugsee-cli binary. Auto-discovered
                       from PATH or a sibling bugsee-cli checkout otherwise.

Both names are deliberate: bugsee-cli declares `BUGSEE_APP_TOKEN` and
`BUGSEE_ENDPOINT` as global `env =` args, so it picks them up on its own and
the token never appears in argv. Note the two consumers disagree about the
shape of the endpoint — the CLI takes a base and appends `/v2/apps/<token>/…`
itself, while the SDK wants the `/v2` url. This script therefore treats the
variable as the BASE and appends `/v2` for the SDK only.

A note on the app: the application must be of type `rust` (appserver
`473fa430`). Pointing this at an ios/android app will ingest through the wrong
branch and the result will be misleading rather than obviously broken.
"""

import argparse
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SDK_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_ENDPOINT = "https://apidev.bugsee.com"


class Failure(Exception):
    pass


# ---------------------------------------------------------------------------
# The generated app.
# ---------------------------------------------------------------------------

CARGO_TOML = """\
[package]
name = "bugsee-deploy-e2e"
version = "{version}"
edition = "2021"

[dependencies]
bugsee = {{ path = "{sdk}/crates/bugsee" }}

# Exactly what a real user must configure. Release builds carry NO debug info
# by default, and without a build id there is no identity to key symbols on —
# either omission makes the upload succeed and resolve nothing.
[profile.release]
debug = 1
strip = false
{split_debuginfo}
# Abort on panic so the native handler records a SIGABRT that next-launch
# recovery correlates back to the panic snapshot. The unwinding path is
# covered by rust_worker_e2e.py; this script is about the network, not the
# panic strategies.
panic = "abort"
"""

MAIN_RS = """\
use std::time::Duration;
use bugsee::{Bugsee, LaunchOptions};

fn launch(data_dir: &str) -> bugsee::LaunchGuard {
    Bugsee::launch_with(
        LaunchOptions::new(env!("BUGSEE_APP_TOKEN"))
            // Delivery is otherwise silent: the queue drains whether a report
            // was DELIVERED or THROWN AWAY, so `flush() == true` alone cannot
            // tell the difference. Without this the harness once reported a
            // clean run against a server that rejected every single report.
            .on_report_dropped(|reason| println!("REPORT_DROPPED {reason:?}"))
            .app_version(bugsee::app_version!())
            .app_build("1")
            .data_dir(data_dir)
            .endpoint(env!("BUGSEE_ENDPOINT").to_string())
            .native_crash_capture(true),
    )
    .expect("launch")
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let data_dir = std::env::args().nth(2).expect("data dir");

    match mode.as_str() {
        // Die with a genuine SIGSEGV, leaving a marker on disk.
        "crash" => {
            let _g = launch(&data_dir);
            std::thread::sleep(Duration::from_millis(200));
            unsafe {
                let p: *mut u64 = std::ptr::null_mut();
                std::ptr::write_volatile(p, 0xdead);
            }
            println!("UNREACHABLE");
        }
        // Relaunch: recovery turns the marker into a bundle and DELIVERS it.
        //
        // `flush` returns true ONLY when the outbound queue drained, so it
        // distinguishes "reached the server" from "gave up and left it on
        // disk". Reporting success without checking would make this harness
        // pass against an endpoint that is not even listening — which is the
        // one thing it exists to rule out.
        "deliver" => {
            let _g = launch(&data_dir);
            std::thread::sleep(Duration::from_millis(2000));
            let drained = Bugsee::flush(Duration::from_secs(30));
            std::thread::sleep(Duration::from_millis(500));
            println!("{}", if drained { "DELIVERED" } else { "QUEUE_NOT_DRAINED" });
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    }
}
"""


# ---------------------------------------------------------------------------
# Helpers.
# ---------------------------------------------------------------------------

def run(cmd, **kw):
    kw.setdefault("check", True)
    kw.setdefault("capture_output", True)
    kw.setdefault("text", True)
    return subprocess.run(cmd, **kw)


def discover_cli() -> Path:
    """Locate bugsee-cli: explicit env, then PATH, then a sibling checkout."""

    explicit = os.environ.get("BUGSEE_CLI")
    if explicit:
        path = Path(explicit)
        if not path.exists():
            raise Failure(f"BUGSEE_CLI={explicit} does not exist")
        return path

    on_path = shutil.which("bugsee-cli") or shutil.which("bugsee")
    if on_path:
        return Path(on_path)

    sibling = SDK_ROOT.parent / "bugsee-cli"
    for profile in ("release", "debug"):
        candidate = sibling / "target" / profile / "bugsee-cli"
        if candidate.exists():
            return candidate

    raise Failure(
        "bugsee-cli not found. Set BUGSEE_CLI, put it on PATH, or build it:\n"
        f"    cargo build --release --manifest-path {sibling}/Cargo.toml"
    )


def preflight() -> dict:
    token = os.environ.get("BUGSEE_APP_TOKEN")
    if not token:
        raise Failure(
            "BUGSEE_APP_TOKEN is not set.\n\n"
            "It must be the token of a Rust-type application. As of this "
            "writing no Rust app exists on staging — the appserver gained the "
            "type in 473fa430, but one still has to be created.\n\n"
            "    export BUGSEE_APP_TOKEN=...   # never pass it as an argument"
        )

    if not shutil.which("cargo"):
        raise Failure("cargo not found on PATH")

    base = os.environ.get("BUGSEE_ENDPOINT", DEFAULT_ENDPOINT).rstrip("/")
    if base.endswith("/v2"):
        raise Failure(
            f"BUGSEE_ENDPOINT should be the BASE url, not the /v2 one: {base}\n"
            "bugsee-cli appends /v2/apps/<token>/… itself, so a /v2 base "
            "would produce /v2/v2/… . Drop the suffix; this script adds it "
            "for the SDK."
        )

    return {
        "token": token,
        "base": base,
        "sdk_endpoint": f"{base}/v2",
        "cli": discover_cli(),
    }


def write_project(workdir: Path, version: str) -> Path:
    """Materialize the Cargo project. Returns its root."""

    root = workdir / "app"
    (root / "src").mkdir(parents=True, exist_ok=True)

    # macOS needs a packed .dSYM for the symbolicator; ELF carries its build id
    # inline, so there is nothing to pack there.
    split = ""
    if platform.system() == "Darwin":
        split = 'split-debuginfo = "packed"'

    (root / "Cargo.toml").write_text(
        CARGO_TOML.format(sdk=SDK_ROOT, version=version, split_debuginfo=split)
    )
    (root / "src" / "main.rs").write_text(MAIN_RS)
    return root


def build(root: Path, cfg: dict) -> Path:
    """Build release. Token/endpoint are baked in via env!(), not argv."""

    env = dict(os.environ)
    env["BUGSEE_APP_TOKEN"] = cfg["token"]
    env["BUGSEE_ENDPOINT"] = cfg["sdk_endpoint"]

    if platform.system() == "Linux":
        # GNU build-id is what the symbol store keys ELF records on. Some
        # toolchains default it off; being explicit costs nothing.
        flags = env.get("RUSTFLAGS", "")
        env["RUSTFLAGS"] = f"{flags} -C link-arg=-Wl,--build-id".strip()

    run(["cargo", "build", "--release"], cwd=root, env=env)

    exe = root / "target" / "release" / "bugsee-deploy-e2e"
    if not exe.exists():
        raise Failure(f"build produced no binary at {exe}")
    return exe


def binary_code_ids(exe: Path) -> set:
    """The identities the symbol store would key this binary on."""

    try:
        from symbolic.debuginfo import Archive
    except Exception:
        print("  ~ symbolic not importable; cannot show the expected code_id")
        return set()

    try:
        objects = list(Archive.open(str(exe)).iter_objects())
    except Exception as e:
        print(f"  ~ could not read the binary's identity: {e}")
        return set()

    def norm(v):
        return str(v or "").replace("-", "").lower()

    ids = {norm(o.code_id) for o in objects} | {norm(o.debug_id) for o in objects}
    ids.discard("")
    return ids


def upload_symbols(cli: Path, exe: Path, cfg: dict, version: str) -> None:
    """`debug-files upload --type rust`, pointed at the profile directory.

    Token and endpoint are NOT passed as arguments: bugsee-cli declares both
    as global `env =` args, so it reads them from the environment and the
    token stays out of argv (and therefore out of `ps`).

    `--version` / `--build` are required and are recorded on the symbol
    document; they must match what the SDK reports or the symbols attach to a
    build the crash did not come from.
    """

    env = dict(os.environ)
    env["BUGSEE_APP_TOKEN"] = cfg["token"]
    env["BUGSEE_ENDPOINT"] = cfg["base"]   # base, not /v2 — the CLI appends it

    cmd = [
        str(cli), "debug-files", "upload",
        "--type", "rust",
        "--version", version,
        "--build", "1",
        str(exe.parent),
    ]
    print(f"  $ {' '.join(cmd)}")

    result = subprocess.run(cmd, capture_output=True, text=True, env=env)
    for line in (result.stdout or "").splitlines():
        print(f"    {line}")
    if result.returncode != 0:
        for line in (result.stderr or "").splitlines():
            print(f"    ! {line}")
        raise Failure(f"symbol upload failed (exit {result.returncode})")


def crash_and_deliver(exe: Path, workdir: Path, run_index: int) -> None:
    """One crash + one delivering relaunch, in a fresh data dir."""

    data_dir = workdir / f"data{run_index}"
    data_dir.mkdir(parents=True, exist_ok=True)

    crashed = subprocess.run(
        [str(exe), "crash", str(data_dir)], capture_output=True, text=True
    )
    if crashed.returncode >= 0:
        raise Failure(
            f"run {run_index}: expected death by signal, got exit "
            f"{crashed.returncode} (stdout: {crashed.stdout!r})"
        )
    print(f"  ✓ run {run_index}: died on signal {-crashed.returncode}")

    delivered = subprocess.run(
        [str(exe), "deliver", str(data_dir)],
        capture_output=True, text=True, timeout=120,
    )
    out = delivered.stdout or ""

    # A dropped report drains the queue exactly like a delivered one, so this
    # has to be checked BEFORE trusting the DELIVERED marker.
    dropped = [ln for ln in out.splitlines() if ln.startswith("REPORT_DROPPED")]
    if dropped:
        raise Failure(
            f"run {run_index}: the server ACCEPTED nothing — the report was "
            "abandoned, not delivered:\n  "
            + "\n  ".join(dropped)
        )

    if "QUEUE_NOT_DRAINED" in out:
        raise Failure(
            f"run {run_index}: the bundle was recovered but NOT delivered — "
            "the outbound queue still holds it after a 30s flush. The report "
            "is on disk, not on the server. Check the endpoint, the token, "
            "and network reachability.\n"
            f"  stderr: {delivered.stderr!r}"
        )

    if "DELIVERED" not in out:
        raise Failure(
            f"run {run_index}: delivery relaunch did not complete\n"
            f"  stdout: {out!r}\n  stderr: {delivered.stderr!r}"
        )

    print(f"  ✓ run {run_index}: recovered and delivered (queue drained)")


# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--runs", type=int, default=2,
                    help="crash/deliver cycles; >1 exercises grouping (default 2)")
    ap.add_argument("--skip-upload", action="store_true",
                    help="skip symbol upload, to see what an unsymbolicated "
                         "issue looks like")
    ap.add_argument("--keep", action="store_true",
                    help="keep the generated project for inspection")
    args = ap.parse_args()

    try:
        cfg = preflight()
    except Failure as e:
        print(f"\n✗ {e}\n", file=sys.stderr)
        return 2

    version = f"0.1.{int(time.time())}"
    workdir = Path(tempfile.mkdtemp(prefix="bugsee-deploy-e2e-"))

    print(f"\nendpoint : {cfg['base']}  (SDK uses {cfg['sdk_endpoint']})")
    print(f"cli      : {cfg['cli']}")
    # The generated app passes this through `LaunchOptions::app_version`, so it
    # rides BOTH the symbol document and the crash report and the two agree.
    print(f"version  : {version}   (app + symbol document)")
    print(f"workdir  : {workdir}\n")

    try:
        print("• building the app")
        root = write_project(workdir, version)
        exe = build(root, cfg)
        code_ids = binary_code_ids(exe)
        for cid in sorted(code_ids):
            print(f"  ✓ built; code_id {cid}")

        if args.skip_upload:
            print("\n• skipping symbol upload (--skip-upload)")
        else:
            print("\n• uploading symbols")
            upload_symbols(cfg["cli"], exe, cfg, version)

        print(f"\n• crashing {args.runs}×")
        for i in range(1, args.runs + 1):
            crash_and_deliver(exe, workdir, i)

    except Failure as e:
        print(f"\n✗ {e}\n", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as e:
        print(f"\n✗ command failed: {e}\n{e.stderr}\n", file=sys.stderr)
        return 1
    finally:
        if args.keep:
            print(f"\n(kept {workdir})")
        else:
            shutil.rmtree(workdir, ignore_errors=True)

    identity = (
        "\n".join("     " + c for c in sorted(code_ids)) if code_ids
        else "     (symbolic unavailable — read it from the issue instead)"
    )

    if args.runs < 1:
        print(f"""
─────────────────────────────────────────────────────────────────────
Built only (--runs 0). Nothing was delivered, so there is nothing to
verify on the deployment yet.

Module identity of the build:
{identity}
─────────────────────────────────────────────────────────────────────
""")
        return 0

    grouping = (
        f"  4. All {args.runs} events landed on ONE issue, not {args.runs}.\n"
        if args.runs > 1 else
        "  4. (grouping needs --runs 2 or more to say anything)\n"
    )

    print(f"""
─────────────────────────────────────────────────────────────────────
Delivered. What to verify — none of it is assertable from here, since
it all lives in the deployment's database:

  1. A new crash issue exists, carrying app version {version} —
     the same value the symbols were uploaded under.
  2. It routed as a Rust crash (source_sdk == "rust"), not through a
     platform-guessing branch.
  3. Frames RESOLVED to bugsee_deploy_e2e::main and friends rather
     than bare addresses. This is the step the symbol-UUID
     canonicalization gates.
{grouping}
Expected module identity:
{identity}

Read it back with the MCP tools:
     list_issues(application_id_or_key="<APP>", type="crash")
     get_issue(...)                # check the resolved frames
─────────────────────────────────────────────────────────────────────
""")
    return 0


if __name__ == "__main__":
    sys.exit(main())

//
//  multiprocess.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//
//! Several processes sharing one data directory (a prefork server's workers, a CLI
//! run beside a daemon): every pending report is delivered exactly once, and a
//! holder that dies mid-work never blocks the others (issue #14).
//!
//! The "processes" are real: this test binary re-executes itself in a child role
//! selected by environment variables.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bugsee_core::transport::{Transport, TransportError};
use bugsee_core::{queue, recovery, AssembledReport, Recorder, RecorderConfig};

const ROLE: &str = "BUGSEE_MP_ROLE";
const DIR: &str = "BUGSEE_MP_DIR";
const LOG: &str = "BUGSEE_MP_LOG";

/// A transport that records each upload by appending a line to a shared log file
/// (one `write` under `O_APPEND`, atomic across processes).
struct LogTransport {
    log: PathBuf,
    lock: Mutex<()>,
}
impl Transport for LogTransport {
    fn register_session(&self, _: &str, _: &[u8]) -> Result<String, TransportError> {
        Ok("t".into())
    }
    fn create_issue(&self, _: &str, _: Option<&str>, _: &[u8]) -> Result<String, TransportError> {
        Ok("endpoint".into())
    }
    fn upload_bundle(&self, _: &str, zip: &[u8]) -> Result<(), TransportError> {
        let _g = self.lock.lock().unwrap();
        // Give a racing peer a real window to double-deliver.
        std::thread::sleep(Duration::from_millis(20));
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        // Queued test items carry a readable name; recovered crashes are binary
        // ZIPs, logged as a tag (each recovered session yields one).
        let id = match std::str::from_utf8(zip) {
            Ok(text) if text.starts_with("queued-") => text.to_string(),
            _ => "recovered-crash".to_string(),
        };
        // One `write` call: `writeln!` would split text and newline.
        f.write_all(format!("{id}\n").as_bytes()).unwrap();
        Ok(())
    }
}

/// Child entry point. A no-op in a normal test run.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR).unwrap());
    match role.as_str() {
        "worker" => {
            // Spin until the parent releases every worker at once.
            let go = dir.join("GO");
            while !go.exists() {
                std::thread::sleep(Duration::from_millis(1));
            }
            let transport = Arc::new(LogTransport {
                log: PathBuf::from(std::env::var(LOG).unwrap()),
                lock: Mutex::new(()),
            });
            let rec = Recorder::launch(RecorderConfig::new(&dir, "tok"), transport).unwrap();
            rec.flush(Duration::from_secs(3));
        }
        "holder" => {
            // Hold every queue item and every recoverable session, then wait to
            // be killed.
            let mut held = Vec::new();
            for q in queue::list_pending(&dir) {
                held.push(queue::try_claim(&q).expect("claim queue item"));
            }
            for p in recovery::find_pending(&dir, u64::MAX) {
                held.push(
                    recovery::claim_pending(&dir, &p)
                        .expect("claim session")
                        .expect("a real claim"),
                );
            }
            // Signal readiness through the filesystem: libtest owns stdout.
            std::fs::write(dir.join("READY.tmp"), held.len().to_string()).unwrap();
            std::fs::rename(dir.join("READY.tmp"), dir.join("READY")).unwrap();
            std::thread::sleep(Duration::from_secs(120));
        }
        "session" => {
            // A live session of its own, as a peer's launch would create.
            let session = bugsee_core::session::Session::begin(&dir).unwrap();
            std::fs::write(dir.join("READY.tmp"), session.generation().to_string()).unwrap();
            std::fs::rename(dir.join("READY.tmp"), dir.join("READY")).unwrap();
            std::thread::sleep(Duration::from_secs(120));
        }
        "lockholder" => {
            // Hold `sessions/1.lock`, as a peer's probe or launch would for a moment.
            let claim = bugsee_core::claim::try_claim(&dir.join("sessions").join("1.lock"));
            std::fs::write(dir.join("READY.tmp"), claim.is_some().to_string()).unwrap();
            std::fs::rename(dir.join("READY.tmp"), dir.join("READY")).unwrap();
            std::thread::sleep(Duration::from_secs(120));
        }
        other => panic!("unknown role {other}"),
    }
}

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let p =
            std::env::temp_dir().join(format!("bugsee-mp-{}", bugsee_core::util::random_hex(8)));
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn(role: &str, dir: &Path, log: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--nocapture"])
        .env(ROLE, role)
        .env(DIR, dir)
        .env(LOG, log)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap()
}

/// `queued` reports in the queue and `crashed` dead sessions awaiting recovery.
fn seed(dir: &Path, queued: usize, crashed: u64) {
    for i in 0..queued {
        queue::enqueue(
            dir,
            &AssembledReport {
                bundle_name: format!("{}.bundle.zip", bugsee_core::util::random_hex(10)),
                zip: format!("queued-{i}").into_bytes(),
                request_json: br#"{"signatures":[]}"#.to_vec(),
            },
        )
        .unwrap();
    }
    let sessions = dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for g in 1..=crashed {
        // "0" is the portable "owner gone" sentinel.
        std::fs::write(sessions.join(format!("{g}.alive")), "0").unwrap();
        // The lock file a crashed session leaves behind: present, held by no one.
        std::fs::write(sessions.join(format!("{g}.lock")), "").unwrap();
        std::fs::create_dir_all(dir.join("parts").join(g.to_string())).unwrap();
    }
    std::fs::write(dir.join("gen"), crashed.to_string()).unwrap();
}

fn delivered(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|s| s.lines().map(String::from).collect())
        .unwrap_or_default()
}

/// Every file under `dir`, relative, for failure messages.
fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p.strip_prefix(dir).unwrap().display().to_string());
            }
        }
    }
    out.sort();
    out
}

fn run_workers(dir: &Path, log: &Path, n: usize) {
    let kids: Vec<Child> = (0..n).map(|_| spawn("worker", dir, log)).collect();
    std::thread::sleep(Duration::from_millis(300)); // all spinning on GO
    std::fs::write(dir.join("GO"), "").unwrap();
    for mut k in kids {
        let _ = k.wait();
    }
}

#[test]
fn racing_launchers_deliver_each_pending_report_exactly_once() {
    // Repeated: the double-delivery windows are narrow, so one round can pass by luck.
    for round in 0..8 {
        let tmp = TempDir::new();
        let dir = tmp.0.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let log = tmp.0.join("delivered.log");
        seed(&dir, 6, 4);

        run_workers(&dir, &log, 5);

        let got = delivered(&log);
        // 6 queued reports + 4 recovered crashes, no more, no fewer.
        assert_eq!(
            got.len(),
            10,
            "round {round}, delivery log: {got:?}, left on disk: {:?}",
            tree(&dir)
        );
        for i in 0..6 {
            let n = got.iter().filter(|l| **l == format!("queued-{i}")).count();
            assert_eq!(n, 1, "round {round}: queued-{i} delivered {n} times");
        }
        assert_eq!(got.iter().filter(|l| *l == "recovered-crash").count(), 4);
        assert!(queue::list_pending(&dir).is_empty());
    }
}

#[test]
fn a_holder_killed_mid_work_does_not_block_the_others() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let log = tmp.0.join("delivered.log");
    seed(&dir, 4, 3);

    let mut holder = spawn("holder", &dir, &log);
    let ready = dir.join("READY");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "holder never reported ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "7");

    // While the holder lives, nothing it holds is delivered by anyone else.
    run_workers(&dir, &log, 2);
    assert!(delivered(&log).is_empty(), "live holder's work was taken");
    std::fs::remove_file(dir.join("GO")).unwrap();

    // SIGKILL / TerminateProcess: no destructor runs, its claim files survive.
    holder.kill().unwrap();
    let _ = holder.wait();

    let deadline = Instant::now() + Duration::from_secs(20);
    run_workers(&dir, &log, 3);
    while delivered(&log).len() < 7 && Instant::now() < deadline {
        run_workers(&dir, &log, 1);
    }
    assert_eq!(delivered(&log).len(), 7, "log: {:?}", delivered(&log));
}

/// Containers with separate pid namespaces share a volume, and each app is pid 1,
/// so a peer's recorded pid can name a live process HERE that is not the peer.
/// The marker's pid must therefore play no part in deciding the peer is dead.
#[test]
fn a_live_peers_session_is_not_mistaken_for_a_crash_whatever_pid_its_marker_names() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let log = tmp.0.join("delivered.log");

    let mut peer = spawn("session", &dir, &log);
    let ready = dir.join("READY");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "peer never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let generation: u64 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();

    // Make the marker name a pid that is alive in THIS namespace and is not the
    // peer: the parent's own, then init's. Neither may make the peer look dead,
    // nor make it look alive once it is gone.
    let marker = dir.join("sessions").join(format!("{generation}.alive"));
    // A reaped child's pid: gone from this namespace, as a peer's own pid is.
    let mut reaped = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "no_such_test"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let _ = reaped.wait();
    let foreign_pids = [
        std::process::id().to_string(),
        "1".to_string(),
        "4294967290".to_string(),
        reaped.id().to_string(),
    ];
    for foreign in foreign_pids {
        std::fs::write(&marker, &foreign).unwrap();
        run_workers(&dir, &log, 3);
        let _ = std::fs::remove_file(dir.join("GO"));
        assert!(delivered(&log).is_empty(), "live peer reported as a crash");
        assert!(marker.exists(), "live peer's marker was reclaimed");
    }

    // Once the peer really dies, its session is recovered exactly once, even
    // though the marker still names a live pid.
    peer.kill().unwrap();
    let _ = peer.wait();
    run_workers(&dir, &log, 3);
    assert_eq!(delivered(&log), vec!["recovered-crash".to_string()]);
}

/// A launch that finds `<gen>.lock` momentarily held by someone else must not leave
/// an unheld lock file behind: every peer would read it as "that session died" and
/// recover (and delete) the live session.
#[test]
fn a_launch_that_loses_the_lock_race_is_not_recovered_as_a_crash() {
    let tmp = TempDir::new();
    let dir = tmp.0.join("data");
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    let log = tmp.0.join("delivered.log");

    let mut holder = spawn("lockholder", &dir, &log);
    let ready = dir.join("READY");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "holder never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&ready).unwrap(), "true");
    std::fs::remove_file(&ready).unwrap();

    // Generation 1 is ours, but its lock file is held by the other process.
    let session = bugsee_core::session::Session::begin(&dir).unwrap();
    assert_eq!(session.generation(), 1);
    holder.kill().unwrap();
    let _ = holder.wait();

    // Peers now look at a live session (this process) with no usable lock file.
    run_workers(&dir, &log, 3);
    assert!(
        delivered(&log).is_empty(),
        "live session reported as a crash"
    );
    assert!(dir.join("sessions").join("1.alive").exists());
    drop(session);
}

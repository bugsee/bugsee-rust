//
//  options.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Typed launch configuration. Mirrors the mobile SDK option surface, adapted
//! to a builder rather than a string-keyed registry.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bugsee_core::capture::WindowCaps;
use bugsee_core::model::entry::Breadcrumb;
use bugsee_core::reporting::ReportMeta;
use bugsee_core::runtime::{BeforeBreadcrumb, BeforeSend};
use bugsee_core::transport::Transport;

/// Configuration passed to [`crate::Bugsee::launch_with`].
pub struct LaunchOptions {
    pub(crate) app_token: String,
    pub(crate) on_report_dropped: Option<bugsee_core::runtime::OnReportDropped>,
    pub(crate) app_version: Option<String>,
    pub(crate) app_build: Option<String>,
    pub(crate) app_package_id: Option<String>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) max_window: Duration,
    pub(crate) max_bytes: u64,
    pub(crate) max_events: u64,
    pub(crate) rotate_interval: Duration,
    pub(crate) endpoint: Option<String>,
    pub(crate) transport: Option<Arc<dyn Transport>>,
    pub(crate) native_crash_capture: bool,
    pub(crate) report_panics_from_hook: bool,
    pub(crate) system_telemetry: bool,
    pub(crate) before_send: Option<BeforeSend>,
    pub(crate) before_breadcrumb: Option<BeforeBreadcrumb>,
    pub(crate) sample_rate: f64,
    pub(crate) launch_recovery_budget: Duration,
}

impl LaunchOptions {
    /// Options for `app_token` with sensible defaults.
    pub fn new(app_token: impl Into<String>) -> Self {
        LaunchOptions {
            app_token: app_token.into(),
            on_report_dropped: None,
            app_version: None,
            app_build: None,
            app_package_id: None,
            data_dir: None,
            max_window: Duration::from_secs(60),
            max_bytes: 8 << 20,
            max_events: 100_000,
            rotate_interval: Duration::from_secs(1),
            endpoint: None,
            transport: None,
            native_crash_capture: true,
            report_panics_from_hook: false,
            system_telemetry: true,
            before_send: None,
            before_breadcrumb: None,
            sample_rate: 1.0,
            launch_recovery_budget: bugsee_core::runtime::DEFAULT_LAUNCH_RECOVERY_BUDGET,
        }
    }

    /// Fraction of non-fatal (`error`) reports to keep, in `[0.0, 1.0]`. Crashes
    /// are never sampled out. Default `1.0` (keep everything).
    pub fn sample_rate(mut self, rate: f64) -> Self {
        self.sample_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Register a callback run on every report before delivery. Mutate the
    /// metadata (summary/severity/labels/attributes/…) in place; return `false`
    /// to drop the report.
    pub fn before_send(
        mut self,
        f: impl Fn(&mut ReportMeta) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.before_send = Some(Box::new(f));
        self
    }

    /// Enable/disable installing the native fatal-crash handler (default `true`).
    /// Disable when the host already owns native crash handling.
    pub fn native_crash_capture(mut self, enabled: bool) -> Self {
        self.native_crash_capture = enabled;
        self
    }

    /// Report panics **immediately from the panic hook** instead of the default
    /// mark-and-recover behaviour (default `false`).
    ///
    /// By default the hook only captures: a panic caught at an SDK boundary is
    /// reported there as `handled`, and an uncaught panic that terminates the
    /// process is reported on the **next launch** by recovery. That keeps the
    /// fault path minimal — no allocation, disk or network work on the panicking
    /// thread.
    ///
    /// Enable this to deliver the report during the panic instead. Two reasons
    /// to want it:
    /// - **immediacy** — the report does not wait for a next launch, which
    ///   matters for short-lived processes that may never restart;
    /// - **thread panics** — a panic that kills a non-main thread never unwinds
    ///   out of `main`, so there is no process death for recovery to observe and
    ///   the default mode will not report it.
    ///
    /// The cost is that reporting runs on the panicking thread. Pair it with a
    /// `flush` in your own hook/shutdown path if you need delivery guaranteed
    /// before the process dies.
    pub fn report_panics_from_hook(mut self, enabled: bool) -> Self {
        self.report_panics_from_hook = enabled;
        self
    }

    /// Enable/disable periodic system/process telemetry sampling (default `true`).
    pub fn system_telemetry(mut self, enabled: bool) -> Self {
        self.system_telemetry = enabled;
        self
    }

    /// Set the on-disk data directory, taking precedence over everything else.
    ///
    /// The directory holds the capture window, the liveness marker and the retry
    /// queue, and crash recovery works only if it OUTLIVES the process: on a
    /// container, point it at a volume. See [`DataDirSource`] for the full
    /// resolution order and what happens when it is not writable.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(dir.into());
        self
    }

    /// Maximum wall-time span of captured context retained.
    pub fn max_window(mut self, window: Duration) -> Self {
        self.max_window = window;
        self
    }

    /// Maximum on-disk bytes retained across the capture window.
    pub fn max_bytes(mut self, bytes: u64) -> Self {
        self.max_bytes = bytes;
        self
    }

    /// Maximum total entry count retained across the capture window.
    pub fn max_events(mut self, events: u64) -> Self {
        self.max_events = events;
        self
    }

    /// Register a callback run on every breadcrumb before it is captured. Return
    /// the (possibly-mutated) breadcrumb to keep it, or `None` to drop it.
    pub fn before_breadcrumb(
        mut self,
        f: impl Fn(Breadcrumb) -> Option<Breadcrumb> + Send + Sync + 'static,
    ) -> Self {
        self.before_breadcrumb = Some(Box::new(f));
        self
    }

    /// Override the API base URL (defaults to `https://api.bugsee.com/v2`).
    /// Called when a report is abandoned instead of delivered.
    ///
    /// Delivery is otherwise SILENT. The durable queue is drained on success
    /// and on permanent failure alike, so [`crate::Bugsee::flush`] returning
    /// `true` means "the queue is empty", NOT "the server got it" — and this
    /// SDK does no logging. Without this hook a host cannot tell that every one
    /// of its crash reports is being rejected.
    ///
    /// Runs on the uploader thread; keep it short and non-blocking.
    ///
    /// ```no_run
    /// use bugsee::{Bugsee, LaunchOptions};
    /// let _guard = Bugsee::launch_with(
    ///     LaunchOptions::new("APP_TOKEN")
    ///         .on_report_dropped(|reason| eprintln!("bugsee dropped a report: {reason:?}")),
    /// );
    /// ```
    pub fn on_report_dropped(
        mut self,
        hook: impl Fn(bugsee_core::runtime::DropReason) + Send + Sync + 'static,
    ) -> Self {
        self.on_report_dropped = Some(Box::new(hook));
        self
    }

    /// The application's version, e.g. `"1.4.2"`.
    ///
    /// Without it an issue carries no version at all: it cannot be filtered by
    /// release, and regressions cannot be tracked across builds. The SDK cannot
    /// infer it — `env!("CARGO_PKG_VERSION")` inside this crate is the SDK's own
    /// version — so pass [`crate::app_version!`], which expands in YOUR crate.
    pub fn app_version(mut self, version: impl Into<String>) -> Self {
        self.app_version = Some(version.into());
        self
    }

    /// The application's build number, if it has one distinct from the version.
    pub fn app_build(mut self, build: impl Into<String>) -> Self {
        self.app_build = Some(build.into());
        self
    }

    /// Overrides the application identifier, which otherwise defaults to the
    /// executable's file name. Use a bundle-id-style string
    /// (`"com.example.app"`) to match how the mobile SDKs report it.
    pub fn app_package_id(mut self, package_id: impl Into<String>) -> Self {
        self.app_package_id = Some(package_id.into());
        self
    }

    /// Time `launch` may spend queueing the previous run's crash reports before
    /// it returns. Default 500 ms.
    ///
    /// A host that crashes again right after launch (a crash loop under a
    /// supervisor) still gets the previous crash reported, because it is made
    /// durable on this path; only the upload is asynchronous. The newest crashed
    /// session is always queued, and at most 8 are queued here, newest first;
    /// the budget stops further ones from starting once it has elapsed, and
    /// whatever is left is recovered on the background worker. It is checked
    /// between sessions, so one very large session can exceed it (the cost of a
    /// session grows with its capture window, `max_window`/`max_bytes`). `Duration::ZERO` skips the launch-path
    /// pass and recovers everything in the background.
    pub fn launch_recovery_budget(mut self, budget: Duration) -> Self {
        self.launch_recovery_budget = budget;
        self
    }

    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Inject a custom transport (e.g. for tests), bypassing the default HTTP one.
    pub fn with_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    pub(crate) fn caps(&self) -> WindowCaps {
        WindowCaps {
            max_window_ms: self.max_window.as_millis() as i64,
            max_bytes: self.max_bytes,
            max_events: self.max_events,
        }
    }

    /// Pick the data directory; see [`DataDirSource`] for the order and what an
    /// unusable choice means.
    pub(crate) fn resolve_data_dir(&self) -> DataDirResolution {
        resolve_data_dir(
            self.data_dir.as_deref(),
            &self.app_token,
            &|name| std::env::var_os(name),
            &std::env::temp_dir,
        )
    }
}

/// Environment variable naming the data directory when
/// [`LaunchOptions::data_dir`] is not set.
pub const DATA_DIR_ENV: &str = "BUGSEE_DATA_DIR";

/// Which step of the resolution order produced the data directory.
///
/// 1. [`Explicit`](DataDirSource::Explicit): [`LaunchOptions::data_dir`].
/// 2. [`Environment`](DataDirSource::Environment): `BUGSEE_DATA_DIR`.
/// 3. [`PlatformDefault`](DataDirSource::PlatformDefault): `bugsee/<token>` under
///    `$XDG_DATA_HOME` (else `~/.local/share`) on Linux, `~/Library/Application
///    Support` on macOS, `%LOCALAPPDATA%` on Windows. Skipped when the base is
///    unset, as in a container with no home directory.
/// 4. [`TempFallback`](DataDirSource::TempFallback): `bugsee-<uid>/<token>` under
///    the system temp directory (`bugsee/<token>` on Windows), created private
///    (`0700`) and refused if it is a symlink or owned by another user. Often
///    wiped on restart, so crash recovery may not survive it;
///    [`StorageStatus::may_be_ephemeral`](crate::StorageStatus::may_be_ephemeral)
///    reports it.
///
/// **A directory you asked for is final.** If 1 or 2 is set and not writable the
/// SDK does NOT quietly use another location on disk (that would scatter
/// reports somewhere the operator never chose); it runs in memory and reports
/// why in [`StorageStatus::explicit_rejected`](crate::StorageStatus::explicit_rejected).
/// Only the defaults (3, then 4) fall through to each other.
///
/// With no usable directory the SDK runs
/// [in memory](bugsee_core::runtime::StorageMode::Memory): handled errors and
/// manual reports are still sent, but there is no capture window and no crash
/// recovery. It logs one line to stderr when it degrades.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDirSource {
    Explicit,
    Environment,
    PlatformDefault,
    TempFallback,
    /// No directory is in use (memory mode).
    None,
}

/// The outcome of [`resolve_data_dir`].
pub(crate) struct DataDirResolution {
    /// The chosen directory, or `None` if no candidate was writable.
    pub dir: Option<PathBuf>,
    pub source: DataDirSource,
    /// Default candidates tried and rejected before the winner, with the reason.
    pub skipped: Vec<String>,
    /// Why an explicit / environment directory was rejected (it is final).
    pub explicit_rejected: Option<String>,
    /// A rejected candidate that nevertheless EXISTS as a directory (full or
    /// read-only). It may hold reports and crashes a previous run persisted, so
    /// it is still drained and recovered from; no new files are written to it.
    pub recovery_dir: Option<PathBuf>,
    /// The location earlier SDK versions used by default
    /// (`<temp>/bugsee/<token>`), if it exists, is trustworthy and is not `dir`.
    /// Its leftovers are recovered once at launch.
    pub legacy_dir: Option<PathBuf>,
}

/// Pure resolution core: `env` and `temp` are injected so tests need not touch
/// the process environment.
pub(crate) fn resolve_data_dir(
    explicit: Option<&Path>,
    app_token: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
    temp: &dyn Fn() -> PathBuf,
) -> DataDirResolution {
    let token = short_token(app_token);

    let chosen = explicit
        .map(|d| (DataDirSource::Explicit, d.to_path_buf()))
        .or_else(|| {
            env(DATA_DIR_ENV)
                .filter(|v| !v.is_empty())
                .map(|v| (DataDirSource::Environment, PathBuf::from(v)))
        });
    if let Some((source, dir)) = chosen {
        return match prepare_dir(&dir, false) {
            Ok(()) => DataDirResolution {
                dir: Some(dir),
                source,
                skipped: Vec::new(),
                explicit_rejected: None,
                recovery_dir: None,
                legacy_dir: None,
            },
            Err(e) => DataDirResolution {
                explicit_rejected: Some(format!("{}: {e}", dir.display())),
                recovery_dir: dir.is_dir().then_some(dir),
                dir: None,
                source: DataDirSource::None,
                skipped: Vec::new(),
                legacy_dir: None,
            },
        };
    }

    // Only now: an explicit directory never needs the temp location.
    let temp = temp();
    let mut candidates: Vec<(DataDirSource, PathBuf, bool)> = Vec::new();
    if let Some(base) = platform_data_base(env) {
        candidates.push((
            DataDirSource::PlatformDefault,
            base.join("bugsee").join(&token),
            false,
        ));
    }
    candidates.push((
        DataDirSource::TempFallback,
        temp.join(private_temp_name()).join(&token),
        true,
    ));

    let mut skipped = Vec::new();
    let mut found = None;
    let mut recovery_dir = None;
    for (source, dir, private) in candidates {
        match prepare_dir(&dir, private) {
            Ok(()) => {
                found = Some((source, dir));
                break;
            }
            Err(e) => {
                skipped.push(format!("{}: {e}", dir.display()));
                // Only the user's own default location: the shared temp area is
                // exactly where a planted directory would be refused.
                if !private && recovery_dir.is_none() && dir.is_dir() {
                    recovery_dir = Some(dir);
                }
            }
        }
    }

    let legacy = temp.join("bugsee").join(&token);
    // Also with no usable directory: leftovers there are exactly what a launch
    // that cannot write anywhere new should still deliver.
    let chosen = found.as_ref().map(|(_, dir)| dir.as_path());
    let legacy_dir = legacy_is_adoptable(&legacy, chosen).then_some(legacy);
    match found {
        Some((source, dir)) => DataDirResolution {
            dir: Some(dir),
            source,
            skipped,
            explicit_rejected: None,
            recovery_dir: None,
            legacy_dir,
        },
        None => DataDirResolution {
            dir: None,
            source: DataDirSource::None,
            skipped,
            explicit_rejected: None,
            recovery_dir,
            legacy_dir,
        },
    }
}

/// The per-user data base directory for this platform, if its inputs are set.
fn platform_data_base(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        set("LOCALAPPDATA")
    } else if cfg!(target_vendor = "apple") {
        set("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        set("XDG_DATA_HOME").or_else(|| set("HOME").map(|h| h.join(".local").join("share")))
    }
}

/// The per-user directory name under the shared temp dir. The system temp
/// directory is world-writable on unix, so a fixed name would let another user
/// pre-create (or symlink) it and read or plant our reports.
#[cfg(unix)]
fn private_temp_name() -> String {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    format!("bugsee-{}", unsafe { libc::geteuid() })
}
#[cfg(not(unix))]
fn private_temp_name() -> String {
    "bugsee".to_string()
}

/// Create `dir` and prove a file can be written in it. Checking writability (not
/// just existence) is what catches a read-only root file system or a volume
/// mounted read-only. With `private` (the shared temp area) each created
/// component is `0700` and an existing one is refused unless it is a real
/// directory owned by us.
fn prepare_dir(dir: &Path, private: bool) -> std::io::Result<()> {
    if private {
        make_private(dir)?;
    } else {
        std::fs::create_dir_all(dir)?;
    }
    probe_writable(dir)
}

#[cfg(unix)]
fn make_private(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    // The shared temp dir itself is the system's; only the two components below
    // it are ours to vet.
    let leaf = dir;
    let top = leaf.parent().unwrap_or(leaf);
    if let Some(shared) = top.parent() {
        std::fs::create_dir_all(shared)?;
    }
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    for component in [top, leaf] {
        match std::fs::DirBuilder::new().mode(0o700).create(component) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        // `symlink_metadata`: a symlink planted at our name must not be followed.
        let meta = std::fs::symlink_metadata(component)?;
        if !meta.file_type().is_dir() {
            return Err(std::io::Error::other(
                "not a real directory (symlink or file at our private name)",
            ));
        }
        if meta.uid() != uid {
            return Err(std::io::Error::other("owned by another user"));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(component, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}
#[cfg(not(unix))]
fn make_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Whether `dir` exists and may be trusted to hold OUR leftovers: a real
/// directory (not a symlink) owned by the current user. The legacy location is
/// in the shared temp area, where anyone could have planted files.
fn trusted_existing_dir(dir: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    if !meta.file_type().is_dir() {
        return false;
    }
    owned_by_us(&meta)
}

#[cfg(unix)]
fn owned_by_us(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    meta.uid() == unsafe { libc::geteuid() }
}
#[cfg(not(unix))]
fn owned_by_us(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Whether `legacy` (`<temp>/bugsee/<token>`) may be recovered from. Both it AND
/// `<temp>/bugsee` must be real directories owned by us: a symlink at either
/// would let someone else's tree, or our own live data directory, be processed
/// (recovering our live session as a crash). Paths are compared canonicalised,
/// not as strings, so another spelling of our own directory is rejected too.
fn legacy_is_adoptable(legacy: &Path, chosen: Option<&Path>) -> bool {
    let Some(parent) = legacy.parent() else {
        return false;
    };
    if !trusted_existing_dir(parent) || !trusted_existing_dir(legacy) {
        return false;
    }
    let Ok(legacy) = legacy.canonicalize() else {
        return false;
    };
    match chosen {
        // No live directory of ours to collide with.
        None => true,
        Some(chosen) => chosen.canonicalize().is_ok_and(|c| c != legacy),
    }
}

/// Prove a file can be created in `dir`. `create_new` with an unpredictable name
/// refuses to follow or reuse anything pre-planted at that path.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let probe = dir.join(format!(
        ".bugsee-probe-{}-{}",
        std::process::id(),
        bugsee_core::util::random_hex(8)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = options.open(&probe).and_then(|mut f| f.write_all(b"1"));
    let _ = std::fs::remove_file(&probe);
    result
}

/// Where the SDK keeps its state right now; see
/// [`Bugsee::storage_status`](crate::Bugsee::storage_status).
///
/// On ephemeral storage, crash recovery only works if the directory outlives
/// the process (a volume mount). The SDK cannot tell a volume from a scratch
/// disk, so check [`may_be_ephemeral`](StorageStatus::may_be_ephemeral) and
/// your own deployment.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct StorageStatus {
    /// `Disk` when the data directory is usable; `Memory` when it is not.
    pub mode: bugsee_core::runtime::StorageMode,
    /// The directory in use; `None` in memory mode.
    pub data_dir: Option<PathBuf>,
    /// Which resolution step produced `data_dir`.
    pub source: DataDirSource,
    /// Default candidates rejected before the one in use, with the error for each.
    pub skipped: Vec<String>,
    /// Set when the directory the host asked for (`data_dir` or
    /// `BUGSEE_DATA_DIR`) was unusable. It is final: the SDK is in memory mode,
    /// not on some other disk location.
    pub explicit_rejected: Option<String>,
    /// Why the SDK is in memory mode, if it is.
    pub reason: Option<String>,
    /// A disk write has failed since launch (full, read-only, removed). Reports
    /// still go out, but crash recovery can no longer be relied on.
    pub degraded: bool,
}

impl StorageStatus {
    /// Whether a fatal crash can be recovered on the next launch: true only with
    /// a usable data directory that has not failed since launch. (It also needs
    /// that directory to survive the process; see
    /// [`may_be_ephemeral`](Self::may_be_ephemeral).)
    pub fn crash_recovery_available(&self) -> bool {
        self.mode == bugsee_core::runtime::StorageMode::Disk && !self.degraded
    }

    /// Whether the directory is the temp-dir fallback, which is commonly wiped
    /// on restart and so may not preserve a crash for the next launch. Set
    /// [`LaunchOptions::data_dir`] (or `BUGSEE_DATA_DIR`) to a volume to fix it.
    pub fn may_be_ephemeral(&self) -> bool {
        self.source == DataDirSource::TempFallback
    }
}

fn short_token(token: &str) -> String {
    token
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bugsee-resolve-{}",
            bugsee_core::util::random_hex(8)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A path no directory can be created at: a child of a regular file.
    fn blocked(scratch: &Path, name: &str) -> PathBuf {
        let file = scratch.join(name);
        std::fs::write(&file, b"x").unwrap();
        file.join("sub")
    }

    fn env_of<'a>(vars: &'a [(&'a str, OsString)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn home_env(home: &Path) -> Vec<(&'static str, OsString)> {
        vec![
            ("HOME", home.as_os_str().to_owned()),
            ("XDG_DATA_HOME", home.as_os_str().to_owned()),
            ("LOCALAPPDATA", home.as_os_str().to_owned()),
        ]
    }

    #[test]
    fn explicit_directory_wins_when_writable() {
        let s = scratch();
        let explicit = s.join("explicit");
        let env = [(DATA_DIR_ENV, s.join("from-env").into_os_string())];
        let r = resolve_data_dir(Some(&explicit), "tok", &env_of(&env), &|| s.join("tmp"));
        assert_eq!(r.source, DataDirSource::Explicit);
        assert_eq!(r.dir.as_deref(), Some(explicit.as_path()));
        assert!(r.skipped.is_empty() && r.explicit_rejected.is_none());
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn unwritable_explicit_is_final_and_never_falls_to_another_location() {
        let s = scratch();
        let good_env = s.join("from-env");
        let env = [(DATA_DIR_ENV, good_env.clone().into_os_string())];
        let mut full_env = env.to_vec();
        full_env.extend(home_env(&s.join("home")));
        let r = resolve_data_dir(Some(&blocked(&s, "f1")), "tok", &env_of(&full_env), &|| {
            s.join("tmp")
        });
        assert!(r.dir.is_none());
        assert_eq!(r.source, DataDirSource::None);
        assert!(r.explicit_rejected.is_some());
        assert!(
            !good_env.exists(),
            "the environment variable must not be tried"
        );
        assert!(!s.join("home").exists() && !s.join("tmp").exists());
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn unwritable_environment_directory_is_final_too() {
        let s = scratch();
        let mut env = vec![(DATA_DIR_ENV, blocked(&s, "f2").into_os_string())];
        env.extend(home_env(&s.join("home")));
        let r = resolve_data_dir(None, "tok", &env_of(&env), &|| s.join("tmp"));
        assert!(r.dir.is_none() && r.explicit_rejected.is_some());
        assert!(!s.join("home").exists() && !s.join("tmp").exists());
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn environment_variable_is_used_without_an_explicit_directory() {
        let s = scratch();
        let good = s.join("from-env");
        let env = [(DATA_DIR_ENV, good.clone().into_os_string())];
        let r = resolve_data_dir(None, "tok", &env_of(&env), &|| s.join("tmp"));
        assert_eq!(r.source, DataDirSource::Environment);
        assert_eq!(r.dir, Some(good));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn platform_default_is_used_when_home_is_set() {
        let s = scratch();
        let home = s.join("home");
        let r = resolve_data_dir(None, "tok-1", &env_of(&home_env(&home)), &|| s.join("tmp"));
        assert_eq!(r.source, DataDirSource::PlatformDefault);
        assert!(r.dir.unwrap().starts_with(&home));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn no_home_falls_back_to_a_private_temp_directory() {
        let s = scratch();
        let tmp = s.join("tmp");
        let r = resolve_data_dir(None, "tok-1", &env_of(&[]), &|| tmp.clone());
        assert_eq!(r.source, DataDirSource::TempFallback);
        let dir = r.dir.unwrap();
        assert_eq!(dir, tmp.join(private_temp_name()).join("tok1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for d in [dir.as_path(), dir.parent().unwrap()] {
                let mode = std::fs::metadata(d).unwrap().permissions().mode();
                assert_eq!(mode & 0o077, 0, "{} must be private", d.display());
            }
        }
        let _ = std::fs::remove_dir_all(&s);
    }

    #[cfg(unix)]
    #[test]
    fn temp_fallback_refuses_a_symlink_planted_at_the_private_name() {
        let s = scratch();
        let tmp = s.join("tmp");
        let elsewhere = s.join("elsewhere");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, tmp.join(private_temp_name())).unwrap();
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.dir.is_none(), "must not follow the symlink");
        assert_eq!(r.skipped.len(), 1);
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing may be created through the symlink"
        );
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn nothing_writable_resolves_to_none_and_lists_every_default_attempt() {
        let s = scratch();
        let blocked_home = blocked(&s, "f3");
        let r = resolve_data_dir(None, "tok", &env_of(&home_env(&blocked_home)), &|| {
            blocked(&s, "f4")
        });
        assert_eq!(r.source, DataDirSource::None);
        assert!(r.dir.is_none());
        assert_eq!(r.skipped.len(), 2);
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn an_empty_environment_variable_is_ignored() {
        let s = scratch();
        let env = [(DATA_DIR_ENV, OsString::new())];
        let r = resolve_data_dir(None, "tok", &env_of(&env), &|| s.join("tmp"));
        assert_eq!(r.source, DataDirSource::TempFallback);
        let _ = std::fs::remove_dir_all(&s);
    }

    // Unix only: elsewhere the temp fallback IS the old location.
    #[cfg(unix)]
    #[test]
    fn the_previous_default_location_is_offered_for_recovery_only_if_it_exists() {
        let s = scratch();
        let tmp = s.join("tmp");
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.legacy_dir.is_none(), "nothing to adopt yet");

        let legacy = tmp.join("bugsee").join("tok");
        std::fs::create_dir_all(&legacy).unwrap();
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert_eq!(r.legacy_dir, Some(legacy.clone()));

        // A host that chose its directory never used the legacy one.
        let explicit = s.join("explicit");
        let r = resolve_data_dir(Some(&explicit), "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.legacy_dir.is_none());
        let _ = std::fs::remove_dir_all(&s);
    }
    #[cfg(unix)]
    #[test]
    fn a_symlinked_legacy_location_is_never_adopted() {
        let s = scratch();
        let tmp = s.join("tmp");
        let other = s.join("other");
        std::fs::create_dir_all(other.join("tok")).unwrap();
        std::fs::create_dir_all(&tmp).unwrap();
        // `<temp>/bugsee` itself is a symlink to a tree that has our token dir.
        std::os::unix::fs::symlink(&other, tmp.join("bugsee")).unwrap();
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.dir.is_some() && r.legacy_dir.is_none());

        // The token directory itself being a symlink is refused as well.
        std::fs::remove_file(tmp.join("bugsee")).unwrap();
        std::fs::create_dir_all(tmp.join("bugsee")).unwrap();
        std::os::unix::fs::symlink(other.join("tok"), tmp.join("bugsee").join("tok")).unwrap();
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.legacy_dir.is_none());
        let _ = std::fs::remove_dir_all(&s);
    }

    // The platform default must be `<XDG_DATA_HOME>/bugsee/<token>` to collide.
    #[cfg(all(unix, not(target_vendor = "apple")))]
    #[test]
    fn the_legacy_location_is_skipped_when_it_is_our_own_directory_by_another_spelling() {
        let s = scratch();
        let tmp = s.join("tmp");
        let legacy = tmp.join("bugsee").join("tok");
        std::fs::create_dir_all(&legacy).unwrap();
        // The platform default resolves to the same directory, spelled with `..`.
        let base = tmp.join("elsewhere").join("..");
        std::fs::create_dir_all(tmp.join("elsewhere")).unwrap();
        let env = [
            ("HOME", base.clone().into_os_string()),
            ("XDG_DATA_HOME", base.into_os_string()),
        ];
        let r = resolve_data_dir(None, "tok", &env_of(&env), &|| tmp.clone());
        assert_eq!(r.source, DataDirSource::PlatformDefault);
        assert!(
            r.legacy_dir.is_none(),
            "our live data directory must not be recovered as a legacy one"
        );
        let _ = std::fs::remove_dir_all(&s);
    }

    #[cfg(unix)]
    #[test]
    fn a_rejected_directory_that_exists_is_kept_for_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let s = scratch();
        let ro = s.join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(ro.join("p"), b"x").is_ok() {
            let _ = std::fs::remove_dir_all(&s);
            return; // permission bits not enforced (root)
        }
        let r = resolve_data_dir(Some(&ro), "tok", &env_of(&[]), &|| s.join("tmp"));
        assert!(r.dir.is_none() && r.explicit_rejected.is_some());
        assert_eq!(
            r.recovery_dir.as_deref(),
            Some(ro.as_path()),
            "its queue and crashed sessions must still be drained"
        );

        // A missing directory has nothing to recover.
        let r = resolve_data_dir(Some(&blocked(&s, "f5")), "tok", &env_of(&[]), &|| {
            s.join("tmp")
        });
        assert!(r.recovery_dir.is_none());
        let _ = std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[cfg(unix)]
    #[test]
    fn the_legacy_location_is_adopted_even_when_no_new_default_is_usable() {
        let s = scratch();
        let tmp = s.join("tmp");
        let legacy = tmp.join("bugsee").join("tok");
        std::fs::create_dir_all(&legacy).unwrap();
        // The private temp name is occupied by a symlink, and there is no home.
        std::os::unix::fs::symlink(s.join("elsewhere"), tmp.join(private_temp_name())).unwrap();
        let r = resolve_data_dir(None, "tok", &env_of(&[]), &|| tmp.clone());
        assert!(r.dir.is_none(), "no new directory is usable");
        assert_eq!(r.legacy_dir, Some(legacy));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn an_explicit_directory_never_asks_for_the_temp_location() {
        let s = scratch();
        let r = resolve_data_dir(Some(&s.join("explicit")), "tok", &env_of(&[]), &|| {
            panic!("temp_dir must not be resolved eagerly")
        });
        assert_eq!(r.source, DataDirSource::Explicit);
        let _ = std::fs::remove_dir_all(&s);
    }
}

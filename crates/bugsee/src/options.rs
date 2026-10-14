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

    /// Pick the data directory: the first candidate in [`DataDirSource`] order
    /// that can actually be created and written to.
    pub(crate) fn resolve_data_dir(&self) -> DataDirResolution {
        resolve_data_dir(
            self.data_dir.as_deref(),
            &self.app_token,
            &|name| std::env::var_os(name),
            &std::env::temp_dir(),
        )
    }
}

/// Environment variable naming the data directory when
/// [`LaunchOptions::data_dir`] is not set.
pub const DATA_DIR_ENV: &str = "BUGSEE_DATA_DIR";

/// Which step of the resolution order produced the data directory.
///
/// Candidates are tried in this order and the first one that is writable wins;
/// an unusable one is skipped (and recorded in
/// [`StorageStatus::skipped`](crate::StorageStatus::skipped)), never fatal:
///
/// 1. [`Explicit`](DataDirSource::Explicit): [`LaunchOptions::data_dir`].
/// 2. [`Environment`](DataDirSource::Environment): `BUGSEE_DATA_DIR`.
/// 3. [`PlatformDefault`](DataDirSource::PlatformDefault): `bugsee/<token>` under
///    `$XDG_DATA_HOME` (else `~/.local/share`) on Linux, `~/Library/Application
///    Support` on macOS, `%LOCALAPPDATA%` on Windows. Skipped when the base is
///    unset, as in a container with no home directory.
/// 4. [`TempFallback`](DataDirSource::TempFallback): `bugsee/<token>` under the
///    system temp directory. Often wiped on restart, so crash recovery may not
///    survive it; [`StorageStatus::may_be_ephemeral`](crate::StorageStatus::may_be_ephemeral)
///    reports it.
///
/// If none is writable the SDK runs
/// [in memory](bugsee_core::runtime::StorageMode::Memory): handled errors and
/// manual reports are still sent, but there is no capture window and no crash
/// recovery. It logs one line to stderr when it degrades.
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
    /// Candidates tried and rejected before the winner, with the reason.
    pub skipped: Vec<String>,
}

/// Pure resolution core: `env` and `temp` are injected so tests need not touch
/// the process environment.
pub(crate) fn resolve_data_dir(
    explicit: Option<&Path>,
    app_token: &str,
    env: &dyn Fn(&str) -> Option<OsString>,
    temp: &Path,
) -> DataDirResolution {
    let mut candidates: Vec<(DataDirSource, PathBuf)> = Vec::new();
    if let Some(dir) = explicit {
        candidates.push((DataDirSource::Explicit, dir.to_path_buf()));
    }
    if let Some(dir) = env(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        candidates.push((DataDirSource::Environment, PathBuf::from(dir)));
    }
    // Isolate by app token so multiple apps don't collide.
    let app = |base: PathBuf| base.join("bugsee").join(short_token(app_token));
    if let Some(base) = platform_data_base(env) {
        candidates.push((DataDirSource::PlatformDefault, app(base)));
    }
    candidates.push((DataDirSource::TempFallback, app(temp.to_path_buf())));

    let mut skipped = Vec::new();
    for (source, dir) in candidates {
        match probe_writable(&dir) {
            Ok(()) => {
                return DataDirResolution {
                    dir: Some(dir),
                    source,
                    skipped,
                }
            }
            Err(e) => skipped.push(format!("{}: {e}", dir.display())),
        }
    }
    DataDirResolution {
        dir: None,
        source: DataDirSource::None,
        skipped,
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

/// Create `dir` and prove a file can be written in it. Checking writability (not
/// just existence) is what catches a read-only root file system or a volume
/// mounted read-only.
fn probe_writable(dir: &Path) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".bugsee-probe-{}", std::process::id()));
    let result = std::fs::File::create(&probe).and_then(|mut f| f.write_all(b"1"));
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
#[derive(Debug, Clone)]
pub struct StorageStatus {
    /// `Disk` when the data directory is usable; `Memory` when it is not.
    pub mode: bugsee_core::runtime::StorageMode,
    /// The directory in use; `None` in memory mode.
    pub data_dir: Option<PathBuf>,
    /// Which resolution step produced `data_dir`.
    pub source: DataDirSource,
    /// Candidates rejected before the one in use, with the error for each.
    pub skipped: Vec<String>,
    /// Why the SDK is in memory mode, if it is.
    pub reason: Option<String>,
}

impl StorageStatus {
    /// Whether a fatal crash can be recovered on the next launch: true only with
    /// a usable data directory. (It also needs that directory to survive the
    /// process; see [`may_be_ephemeral`](Self::may_be_ephemeral).)
    pub fn crash_recovery_available(&self) -> bool {
        self.mode == bugsee_core::runtime::StorageMode::Disk
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

    #[test]
    fn explicit_directory_wins_when_writable() {
        let s = scratch();
        let explicit = s.join("explicit");
        let env = [(DATA_DIR_ENV, s.join("from-env").into_os_string())];
        let r = resolve_data_dir(Some(&explicit), "tok", &env_of(&env), &s.join("tmp"));
        assert_eq!(r.source, DataDirSource::Explicit);
        assert_eq!(r.dir.as_deref(), Some(explicit.as_path()));
        assert!(r.skipped.is_empty());
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn unwritable_explicit_falls_through_to_the_environment_variable() {
        let s = scratch();
        let bad = blocked(&s, "f1");
        let good = s.join("from-env");
        let env = [(DATA_DIR_ENV, good.clone().into_os_string())];
        let r = resolve_data_dir(Some(&bad), "tok", &env_of(&env), &s.join("tmp"));
        assert_eq!(r.source, DataDirSource::Environment);
        assert_eq!(r.dir, Some(good));
        assert_eq!(r.skipped.len(), 1, "the rejected candidate is recorded");
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn platform_default_is_used_when_home_is_set() {
        let s = scratch();
        let home = s.join("home");
        let env = [
            ("HOME", home.clone().into_os_string()),
            ("XDG_DATA_HOME", home.clone().into_os_string()),
            ("LOCALAPPDATA", home.clone().into_os_string()),
        ];
        let r = resolve_data_dir(None, "tok-1", &env_of(&env), &s.join("tmp"));
        assert_eq!(r.source, DataDirSource::PlatformDefault);
        assert!(r.dir.unwrap().starts_with(&home));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn no_home_falls_back_to_the_temp_directory() {
        let s = scratch();
        let tmp = s.join("tmp");
        let r = resolve_data_dir(None, "tok-1", &env_of(&[]), &tmp);
        assert_eq!(r.source, DataDirSource::TempFallback);
        assert_eq!(r.dir, Some(tmp.join("bugsee").join("tok1")));
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn nothing_writable_resolves_to_none_and_lists_every_attempt() {
        let s = scratch();
        let env = [(DATA_DIR_ENV, blocked(&s, "f2").into_os_string())];
        let r = resolve_data_dir(
            Some(&blocked(&s, "f3")),
            "tok",
            &env_of(&env),
            &blocked(&s, "f4"),
        );
        assert_eq!(r.source, DataDirSource::None);
        assert!(r.dir.is_none());
        assert_eq!(r.skipped.len(), 3);
        let _ = std::fs::remove_dir_all(&s);
    }

    #[test]
    fn an_empty_environment_variable_is_ignored() {
        let s = scratch();
        let env = [(DATA_DIR_ENV, OsString::new())];
        let r = resolve_data_dir(None, "tok", &env_of(&env), &s.join("tmp"));
        assert_eq!(r.source, DataDirSource::TempFallback);
        let _ = std::fs::remove_dir_all(&s);
    }
}

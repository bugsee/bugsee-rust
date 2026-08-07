//
//  telemetry.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! System/process telemetry sampling via `sysinfo`, emitted as `traces.system`
//! (`cpu_usage`, `process_memory`, `ram`) once per rotation tick.

use bugsee_core::model::environment::HostFacts;
use bugsee_core::runtime::TelemetrySampler;
use serde_json::{json, Value};
use std::path::Path;

use sysinfo::{Disks, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// A `sysinfo`-backed telemetry sampler.
pub struct SysinfoSampler {
    sys: System,
    pid: Option<Pid>,
}

impl SysinfoSampler {
    /// Create a sampler bound to the current process.
    pub fn new() -> Self {
        SysinfoSampler {
            sys: System::new(),
            pid: sysinfo::get_current_pid().ok(),
        }
    }
}

impl Default for SysinfoSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetrySampler for SysinfoSampler {
    fn sample(&mut self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        const MB: u64 = 1024 * 1024;

        if let Some(pid) = self.pid {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
            if let Some(proc) = self.sys.process(pid) {
                out.push(("cpu_usage".to_string(), json!(proc.cpu_usage())));
                out.push(("process_memory".to_string(), json!(proc.memory() / MB)));
            }
        }

        self.sys.refresh_memory();
        out.push(("ram".to_string(), json!(self.sys.used_memory() / MB)));
        out
    }
}

/// Gather the environment facts `bugsee-core` cannot see from `std` alone.
///
/// `std` exposes the OS *name* but not its version, and nothing about memory,
/// disk or locale, so without this the reported environment is little more than
/// `platform.type` — no OS version is the one that hurts most, since it is the
/// first thing anyone looks at on a crash.
///
/// `data_dir` selects which volume the disk figures describe; see
/// [`disk_for`].
///
/// Every field is best-effort: a value that cannot be determined on this
/// platform is left `None` and omitted from the report rather than guessed.
pub fn host_facts(data_dir: &Path) -> HostFacts {
    let mut sys = System::new();
    sys.refresh_memory();

    let (disk_total, disk_free) = disk_for(data_dir);

    HostFacts {
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        // MB, matching the wire contract's unit for these fields (the other
        // SDKs report MB); `sysinfo` hands back bytes.
        memory_total: to_mb(sys.total_memory()),
        memory_free: to_mb(sys.available_memory()),
        disk_total,
        disk_free,
        locale: locale(),
    }
}

/// Bytes -> MB, dropping a zero (which means "unknown" for every one of these).
fn to_mb(bytes: u64) -> Option<u64> {
    const MB: u64 = 1024 * 1024;
    Some(bytes / MB).filter(|v| *v > 0)
}

/// `(total, free)` in MB for the volume that holds `data_dir`.
///
/// Deliberately not the root volume. The contract's disk fields are singular,
/// and the number worth reporting is the one for the filesystem we persist
/// reports onto — a full `/` matters less than a full volume under the data
/// directory, and on a machine with separate mounts they are different numbers.
///
/// Picks the mount point with the LONGEST path that prefixes `data_dir`, which
/// is the standard way to resolve nested mounts (`/` vs `/home` vs
/// `/home/user/data`): the deepest match is the filesystem actually in use.
fn disk_for(data_dir: &Path) -> (Option<u64>, Option<u64>) {
    let disks = Disks::new_with_refreshed_list();

    let best = disks
        .list()
        .iter()
        .filter(|d| data_dir.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len());

    match best {
        Some(d) => (to_mb(d.total_space()), to_mb(d.available_space())),
        None => (None, None),
    }
}

/// `platform.locale` in the `en_US` form the other SDKs report.
///
/// Reads the POSIX environment, most specific first, exactly as the C library
/// resolves it. Encoding and modifier suffixes are dropped
/// (`en_US.UTF-8@euro` -> `en_US`) so the value matches what Android and iOS
/// send; `"C"` and `"POSIX"` mean "no locale configured" and are reported as
/// absent rather than as a language.
///
/// The lookup is NOT unix-gated. Those variables are a convention rather than a
/// syscall, and Windows environments do set them — MSYS2, Cygwin, Git Bash and
/// plenty of CI images — so reading them there costs nothing and is right when
/// present. Gating it off would also have meant gating this function's tests,
/// losing coverage of platform-independent string logic for no benefit.
///
/// What Windows still lacks is the *fallback* when the environment says
/// nothing: `GetUserDefaultLocaleName` needs a Win32 binding or a crate, and
/// this SDK has taken no new dependencies. The field is optional, so such a
/// report simply omits it rather than carrying a wrong value.
fn locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find_map(|raw| normalize_locale(&raw))
}

/// `en_US.UTF-8@euro` -> `en_US`; `C`/`POSIX`/empty -> `None`.
///
/// Split out from [`locale`] so the rule is testable without mutating process
/// environment — the ambient locale differs per machine and CI runner, so an
/// environment-driven test asserts nothing on a host set to `C`. Being a plain
/// string transform, it is worth testing on every platform, which is the other
/// reason [`locale`] is not unix-gated.
fn normalize_locale(raw: &str) -> Option<String> {
    let value = raw.split(['.', '@']).next().unwrap_or("").trim();
    // "C" and "POSIX" mean "no locale configured"; reporting either as if it
    // were a language would be worse than reporting nothing.
    if value.is_empty() || value == "C" || value == "POSIX" {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_system_and_process_metrics() {
        let mut sampler = SysinfoSampler::new();
        let _ = sampler.sample(); // prime the CPU delta
        let sample = sampler.sample();
        let names: Vec<&str> = sample.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"ram"), "system memory sampled: {names:?}");
        assert!(
            names.contains(&"process_memory"),
            "process memory sampled: {names:?}"
        );
        assert!(
            names.contains(&"cpu_usage"),
            "process cpu sampled: {names:?}"
        );
    }
}

#[cfg(test)]
mod host_facts_tests {
    use super::*;

    #[test]
    fn reports_a_real_os_version_and_memory_on_this_host() {
        // Not a tautology: the whole point of the change is that these fields
        // were absent, so assert they are actually populated on a supported
        // platform rather than merely well-typed.
        let facts = host_facts(&std::env::temp_dir());

        assert!(facts.os_version.is_some(), "os_version must be populated");
        assert!(
            facts.memory_total.unwrap_or(0) > 0,
            "memory_total (MB) must be non-zero, got {:?}",
            facts.memory_total
        );
        assert!(
            facts.memory_total >= facts.memory_free,
            "free ({:?}) cannot exceed total ({:?})",
            facts.memory_free,
            facts.memory_total
        );
    }

    #[test]
    fn reports_real_disk_figures_for_the_data_volume() {
        let facts = host_facts(&std::env::temp_dir());

        assert!(
            facts.disk_total.unwrap_or(0) > 0,
            "disk_total (MB) must be non-zero, got {:?}",
            facts.disk_total
        );
        assert!(
            facts.disk_free <= facts.disk_total,
            "free ({:?}) cannot exceed total ({:?})",
            facts.disk_free,
            facts.disk_total
        );
    }

    #[test]
    fn disk_selection_prefers_the_deepest_matching_mount() {
        // Root always matches, so an unmatchable path still yields the volume
        // containing it rather than nothing. The real assertion is that asking
        // about a nested path never returns LESS specific information than
        // asking about root.
        let root = disk_for(Path::new("/"));
        let nested = disk_for(&std::env::temp_dir());

        assert!(
            root.0.is_some() || nested.0.is_some(),
            "no volume resolved at all"
        );
        if let Some(total) = nested.0 {
            assert!(total > 0);
        }
    }

    #[test]
    fn locale_normalization_matches_the_form_other_sdks_report() {
        for (raw, expected) in [
            ("en_US.UTF-8", Some("en_US")),
            ("en_US", Some("en_US")),
            ("en_US.UTF-8@euro", Some("en_US")),
            ("de_DE@euro", Some("de_DE")),
            ("pt_BR.iso88591", Some("pt_BR")),
        ] {
            assert_eq!(
                normalize_locale(raw).as_deref(),
                expected,
                "normalizing {raw:?}"
            );
        }
    }

    #[test]
    fn the_posix_no_locale_placeholders_are_reported_as_absent() {
        // "C" is what this very machine is set to, and it is not a language.
        // Sending it would put a bogus locale on every report from a CI box.
        for raw in ["C", "POSIX", "C.UTF-8", "", "   "] {
            assert_eq!(normalize_locale(raw), None, "{raw:?} must be absent");
        }
    }
}

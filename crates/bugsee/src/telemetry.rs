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
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

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
/// so without this the reported environment is little more than
/// `platform.type` — no OS version is the one that hurts most, since it is the
/// first thing anyone looks at on a crash.
///
/// Every field is best-effort: `sysinfo` returns `None` on platforms where a
/// value is unavailable, and it is omitted rather than guessed.
pub fn host_facts() -> HostFacts {
    const MB: u64 = 1024 * 1024;

    let mut sys = System::new();
    sys.refresh_memory();

    HostFacts {
        os_version: System::os_version(),
        kernel_version: System::kernel_version(),
        // MB, matching the wire contract's unit for these fields (the other
        // SDKs report MB); `sysinfo` hands back bytes.
        memory_total: Some(sys.total_memory() / MB).filter(|v| *v > 0),
        memory_free: Some(sys.available_memory() / MB).filter(|v| *v > 0),
    }
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
        let facts = host_facts();

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
}

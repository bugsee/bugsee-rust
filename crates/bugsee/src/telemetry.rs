//
//  telemetry.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! System/process telemetry sampling via `sysinfo`, emitted as `traces.system`
//! (`cpu_usage`, `process_memory`, `ram`) once per rotation tick.

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
        assert!(names.contains(&"process_memory"), "process memory sampled: {names:?}");
        assert!(names.contains(&"cpu_usage"), "process cpu sampled: {names:?}");
    }
}

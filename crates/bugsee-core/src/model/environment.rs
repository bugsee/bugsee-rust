//
//  environment.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The `environment` object of `request.json`: four sub-objects
//! (`platform`, `app`, `hardware`, `sdk`). The shape is shared across SDKs; the
//! key inventory inside each sub-object is platform-reported.
//!
//! Two fields carry the Rust-specific backend contract (DESIGN.md §15):
//! - `sdk.type` = `"rust"` — the authoritative routing discriminator; the worker
//!   dispatches on it before `platform.type` (see [`SDK_TYPE`]).
//! - `platform.type` — OS-typed (`linux`/`windows`/`macos`), unlike the mobile
//!   SDKs' `ios`/`android`. The appserver treats `rust` as an umbrella app type
//!   with the per-session OS riding here, exactly like the `javascript` type.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// OS identity and state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Platform {
    #[serde(rename = "type")]
    pub os_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_version: Option<String>,
    /// Minutes east of UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utc_offset: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// MB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_free: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_free: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total: Option<u64>,
}

/// Application identity and build.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct App {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Best-effort GPU info (`hardware.gpu`), Android-compatible placement.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Gpu {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renderer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vram_mb: Option<u64>,
}

/// Device / host identity and state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Hardware {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    /// Epoch ms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_time: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<Gpu>,
}

/// The SDK family this recording came from — the **authoritative backend routing
/// discriminator**, stamped on every recording (mirrors the JS SDK's
/// `environment.sdk.type == "javascript"`). The worker routes on this *before*
/// looking at `platform.type`, so a `macos` Rust report is never mistaken for an
/// Apple/iOS native crash. Must stay in sync with the worker's `crash/rust.py`
/// routing and the appserver's `rust` application type.
pub const SDK_TYPE: &str = "rust";

/// SDK identity and effective configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sdk {
    /// Always [`SDK_TYPE`]. Never skipped — the backend requires it to route.
    #[serde(rename = "type")]
    pub sdk_type: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub options: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapper: Option<Value>,
}

impl Default for Sdk {
    fn default() -> Self {
        Sdk {
            sdk_type: SDK_TYPE.to_string(),
            version: String::new(),
            build: None,
            options: Map::new(),
            wrapper: None,
        }
    }
}

/// The full `environment` object.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Environment {
    pub platform: Platform,
    pub app: App,
    pub hardware: Hardware,
    pub sdk: Sdk,
}

impl Environment {
    /// Build a baseline environment from `std`-available facts. Fuller telemetry
    /// (disk/mem free, boot time, GPU) is layered in by the Phase 4 sampler.
    pub fn detect(sdk_version: &str) -> Self {
        let os_type = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            "linux" => "linux",
            other => other,
        }
        .to_string();

        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .ok();

        Environment {
            platform: Platform {
                os_type,
                ..Default::default()
            },
            app: App {
                name: std::env::current_exe()
                    .ok()
                    .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())),
                path: std::env::current_exe()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned()),
                ..Default::default()
            },
            hardware: Hardware {
                arch: Some(std::env::consts::ARCH.to_string()),
                cpu_count,
                ..Default::default()
            },
            sdk: Sdk {
                sdk_type: SDK_TYPE.to_string(),
                version: sdk_version.to_string(),
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_type_is_always_emitted_for_backend_routing() {
        let env = Environment::detect("1.2.3");
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(
            v["sdk"]["type"], "rust",
            "the worker routes on environment.sdk.type; it must always be present"
        );
        assert_eq!(v["sdk"]["version"], "1.2.3");
        // A default-constructed Sdk must also carry the discriminator.
        assert_eq!(
            serde_json::to_value(Sdk::default()).unwrap()["type"],
            "rust"
        );
    }

    #[test]
    fn platform_type_is_os_typed() {
        let env = Environment::detect("1.0.0");
        let os = env.platform.os_type;
        // Assert membership only on the platforms we actually target/CI, rather
        // than `contains(..) || !is_empty()` — that disjunct is always satisfied
        // by the second clause, so it would pass for "ios" or any garbage.
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        assert!(
            ["linux", "windows", "macos"].contains(&os.as_str()),
            "platform.type must be OS-typed (linux/windows/macos), got {os}"
        );
        // Everywhere else we only require a non-empty identifier.
        assert!(!os.is_empty(), "platform.type must never be empty");
    }
}

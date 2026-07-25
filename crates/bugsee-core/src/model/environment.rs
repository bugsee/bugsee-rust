//
//  environment.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! The `environment` object of `request.json`: four sub-objects
//! (`platform`, `app`, `hardware`, `sdk`). The shape is shared across SDKs; the
//! key inventory inside each sub-object is platform-reported. Rust reports an
//! OS-typed `platform.type` (`linux`/`windows`/`macos`) — a backend
//! coordination item (DESIGN.md §15).

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

/// SDK identity and effective configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Sdk {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Map::is_empty", default)]
    pub options: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapper: Option<Value>,
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
                version: sdk_version.to_string(),
                ..Default::default()
            },
        }
    }
}

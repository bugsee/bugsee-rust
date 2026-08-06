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
    /// Jailbroken (iOS) / rooted (Android) — a privilege-escalated,
    /// otherwise-locked-down device.
    ///
    /// Always `false` on the desktop targets this SDK builds for: those OSes
    /// have no such notion, so this is a statement of fact rather than the
    /// result of a probe.
    ///
    /// It is emitted rather than skipped because the backend aggregates this
    /// field arithmetically and an absent value poisons the running total —
    /// see [`Environment::detect`].
    ///
    /// **Phase 6:** the iOS/Android FFI targets must replace this with real
    /// detection. Reporting `false` from a device we never inspected would be
    /// a wrong answer, which is worse than a missing one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jailbreak: Option<bool>,
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

/// The platform this build targets, as the backend names platforms:
/// `linux` / `windows` / `macos` / `android` / `ios`.
///
/// Single source of truth for both `environment.platform.type` and
/// `crash.json`'s `source_platform`, so the two can never disagree.
pub fn source_platform() -> &'static str {
    // `std::env::consts::OS` already uses the names the backend expects for
    // every target we ship (including `android` and `ios` via the FFI), so it
    // passes through; anything else is reported verbatim rather than guessed at.
    std::env::consts::OS
}

/// The CPU architecture, normalized to the naming the rest of Bugsee uses.
///
/// Rust spells 64-bit ARM `aarch64`, but every symbol file in the pipeline is
/// parsed by `symbolic`, which (like Apple and the Android NDK) spells it
/// `arm64`. Reporting the Rust spelling would mean a crash's arch never
/// string-matched the arch recorded on its own symbols — the worker's
/// `normalize_arch` only strips ISA suffixes, it does not translate between the
/// two vocabularies. Everything else already agrees, so it passes through.
///
/// Used for both `environment.hardware.arch` and `crash.json`'s `source_arch`.
pub fn source_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

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

/// Facts the host layer can obtain but pure-`std` core cannot.
///
/// `bugsee-core` deliberately has no host-integration dependencies, and `std`
/// exposes no OS version, kernel version or memory totals. Rather than pull a
/// platform crate into core — or reach for `libc`/Win32 and spread `unsafe`
/// across it — the host crate gathers these (via `sysinfo`) and passes them in,
/// mirroring how [`crate::runtime::TelemetrySampler`] is supplied.
///
/// Every field is optional and simply omitted when the host cannot supply it,
/// so an embedder that builds without the telemetry feature still gets a valid
/// environment — just a thinner one.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    /// `platform.version` — the OS release (e.g. `"15.3.1"`, `"22.04"`).
    pub os_version: Option<String>,
    /// `platform.kernel_version`.
    pub kernel_version: Option<String>,
    /// `platform.memory_total`, MB (the contract's unit, not bytes).
    pub memory_total: Option<u64>,
    /// `platform.memory_free`, MB.
    pub memory_free: Option<u64>,
    /// `platform.disk_total`, MB — the volume holding the SDK's data directory,
    /// not the root volume. That is the one whose exhaustion actually loses
    /// reports.
    pub disk_total: Option<u64>,
    /// `platform.disk_free`, MB, same volume.
    pub disk_free: Option<u64>,
    /// `platform.locale`, in the `en_US` form the other SDKs report.
    pub locale: Option<String>,
}

impl Environment {
    /// Build a baseline environment from `std`-available facts alone.
    ///
    /// Thin on purpose: `std` knows the OS *name* but not its version, and
    /// nothing about memory. Prefer [`Environment::detect_with`], which layers
    /// in what the host can see.
    pub fn detect(sdk_version: &str) -> Self {
        Self::detect_with(sdk_version, &HostFacts::default())
    }

    /// Build the environment, enriched with host-supplied [`HostFacts`].
    pub fn detect_with(sdk_version: &str, facts: &HostFacts) -> Self {
        let mut env = Self::detect_baseline(sdk_version);
        env.platform.version = facts.os_version.clone();
        env.platform.kernel_version = facts.kernel_version.clone();
        env.platform.memory_total = facts.memory_total;
        env.platform.memory_free = facts.memory_free;
        env.platform.disk_total = facts.disk_total;
        env.platform.disk_free = facts.disk_free;
        env.platform.locale = facts.locale.clone();
        env
    }

    fn detect_baseline(sdk_version: &str) -> Self {
        let os_type = source_platform().to_string();

        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .ok();

        Environment {
            platform: Platform {
                os_type,
                // Not optional in practice. The backend folds this into a
                // running counter with `(stats.jailbreak || 0) +
                // (env.platform.jailbreak && 1)`; in JS `undefined && 1` is
                // `undefined`, so an absent value makes the sum NaN and the
                // whole issue document fails validation — the report is
                // rejected outright, not merely recorded without the field.
                jailbreak: Some(false),
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
                arch: Some(source_arch().to_string()),
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

    #[test]
    fn jailbreak_is_emitted_because_the_backend_sums_it() {
        // Not a nicety. The appserver does
        //     stats.jailbreak = (stats.jailbreak || 0)
        //                     + (env.platform.jailbreak && 1);
        // and in JS `undefined && 1` evaluates to `undefined`, so an absent
        // value turns the running total into NaN. Mongoose then rejects the
        // entire issue document ("Cast to Number failed for value \"NaN\" ...
        // at path \"statistics.jailbreak\"", code 99006) and the crash report
        // is thrown away. Verified against a live deployment: the same report
        // is rejected without this field and accepted with it.
        let env = Environment::detect("1.0.0");
        let v = serde_json::to_value(&env).unwrap();
        assert!(
            v["platform"].get("jailbreak").is_some(),
            "platform.jailbreak must be PRESENT, not skipped — an absent value \
             makes the backend's running total NaN and the report is rejected"
        );
        assert_eq!(
            v["platform"]["jailbreak"], false,
            "desktop targets have no jailbreak/root notion, so false is a fact"
        );
    }

    #[test]
    fn host_facts_populate_the_platform_block() {
        let facts = HostFacts {
            os_version: Some("15.3.1".into()),
            kernel_version: Some("24.3.0".into()),
            memory_total: Some(32768),
            memory_free: Some(4096),
            disk_total: Some(500000),
            disk_free: Some(120000),
            locale: Some("en_US".into()),
        };
        let v = serde_json::to_value(Environment::detect_with("1.0.0", &facts)).unwrap();

        assert_eq!(v["platform"]["version"], "15.3.1");
        assert_eq!(v["platform"]["kernel_version"], "24.3.0");
        assert_eq!(v["platform"]["memory_total"], 32768);
        assert_eq!(v["platform"]["memory_free"], 4096);
        assert_eq!(v["platform"]["disk_total"], 500000);
        assert_eq!(v["platform"]["disk_free"], 120000);
        assert_eq!(v["platform"]["locale"], "en_US");
    }

    #[test]
    fn absent_host_facts_are_omitted_rather_than_nulled() {
        // An embedder building without the telemetry feature still gets a valid
        // environment; the keys simply are not there. Emitting nulls would make
        // consumers distinguish "unknown" from "absent" for no gain.
        let v = serde_json::to_value(Environment::detect("1.0.0")).unwrap();
        let platform = v["platform"].as_object().unwrap();

        for key in [
            "version",
            "kernel_version",
            "memory_total",
            "memory_free",
            "disk_total",
            "disk_free",
            "locale",
        ] {
            assert!(
                !platform.contains_key(key),
                "{key} should be omitted, not null"
            );
        }
    }

    #[test]
    fn detect_is_detect_with_no_facts() {
        let a = serde_json::to_value(Environment::detect("9.9.9")).unwrap();
        let b =
            serde_json::to_value(Environment::detect_with("9.9.9", &HostFacts::default())).unwrap();
        assert_eq!(a, b);
    }
}

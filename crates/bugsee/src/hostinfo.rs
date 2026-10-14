//
//  hostinfo.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Per-platform host facts that neither `std` nor `sysinfo` expose: the local
//! UTC offset, the hardware model, and the Windows user locale.
//!
//! This is the one place in the host crate that calls into the OS directly, so
//! the `unsafe` is confined here and kept to single FFI calls with no
//! invariants beyond their arguments. No binding crate is used: `libc` is
//! already in the tree, and the three Win32 calls are declared by hand (a
//! binding crate for three functions is a worse trade than 15 lines of
//! `extern`). Everything is best-effort and returns `None` rather than a guess.

/// Minutes east of UTC for the local time zone right now.
///
/// Not the `time` crate: it refuses to read the local offset once a process is
/// multithreaded (an SDK always is), because `localtime_r` can race a
/// concurrent `setenv("TZ")`. We accept that narrow race for a single
/// diagnostic number, which the crate's blanket refusal does not allow.
///
/// Sampled once when the environment is built, so a DST change during a very
/// long run is not reflected in that run's reports.
pub(crate) fn utc_offset_minutes() -> Option<i32> {
    imp::utc_offset_minutes()
}

/// The machine's model string, e.g. `MacBookPro18,3`, `iPhone14,2`,
/// `ThinkPad X1 Carbon`, `Raspberry Pi 4 Model B`.
///
/// On desktop Linux and Windows this is the firmware's product name, which on
/// white-box or DIY machines is often a placeholder; those are reported as
/// absent (see [`clean_model`]) rather than as a model.
pub(crate) fn hardware_model() -> Option<String> {
    imp::hardware_model()
}

/// The user's locale from the OS settings, in the `en_US` form. Only Windows
/// has a setting beyond the POSIX environment variables the caller already
/// reads; elsewhere this is `None`.
pub(crate) fn locale() -> Option<String> {
    imp::locale()
}

/// Trim, drop embedded NULs (device-tree strings are NUL-terminated), and
/// reject the placeholders firmware vendors ship when they never filled the
/// field in. Reporting "To Be Filled By O.E.M." as a hardware model would be
/// worse than reporting nothing.
fn clean_model(raw: &str) -> Option<String> {
    let value = raw.replace('\0', "");
    let value = value.trim();
    const PLACEHOLDERS: [&str; 8] = [
        "to be filled by o.e.m.",
        "system product name",
        "default string",
        "not specified",
        "not applicable",
        "unknown",
        "none",
        "n/a",
    ];
    if value.is_empty() || PLACEHOLDERS.iter().any(|p| value.eq_ignore_ascii_case(p)) {
        return None;
    }
    Some(value.to_string())
}

#[cfg(unix)]
mod imp {
    use super::clean_model;

    pub(super) fn utc_offset_minutes() -> Option<i32> {
        // SAFETY: `localtime_r` writes only into the `tm` we hand it and reads
        // the `time_t` by value; a zeroed `tm` is a valid out-parameter.
        let gmtoff = unsafe {
            let now = libc::time(std::ptr::null_mut());
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&now, &mut tm).is_null() {
                return None;
            }
            tm.tm_gmtoff
        };
        Some((gmtoff / 60) as i32)
    }

    #[cfg(target_vendor = "apple")]
    pub(super) fn hardware_model() -> Option<String> {
        // `hw.model` is the model identifier on both macOS ("MacBookPro18,3")
        // and iOS ("iPhone14,2").
        let name = c"hw.model";
        let mut len: libc::size_t = 0;
        // SAFETY: a null buffer asks for the required length only.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` is `len` bytes, the size the kernel just reported.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(len);
        clean_model(&String::from_utf8_lossy(&buf))
    }

    #[cfg(not(target_vendor = "apple"))]
    pub(super) fn hardware_model() -> Option<String> {
        // DMI covers x86 servers and laptops; the device tree covers ARM
        // boards (Raspberry Pi and friends), which have no DMI. Android exposes
        // neither to an app, so it reports nothing here.
        [
            "/sys/class/dmi/id/product_name",
            "/sys/firmware/devicetree/base/model",
        ]
        .into_iter()
        .find_map(|path| clean_model(&std::fs::read_to_string(path).ok()?))
    }

    pub(super) fn locale() -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod imp {
    use super::clean_model;

    // `std` already links kernel32; advapi32 is part of every Windows install.
    #[repr(C)]
    struct SystemTime([u16; 8]);

    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
        fn GetUserDefaultLocaleName(name: *mut u16, len: i32) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        fn RegGetValueW(
            hkey: isize,
            subkey: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut core::ffi::c_void,
            len: *mut u32,
        ) -> i32;
    }

    const TIME_ZONE_ID_INVALID: u32 = 0xFFFF_FFFF;
    const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
    const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
    const RRF_RT_REG_SZ: u32 = 0x0000_0002;

    /// Windows reports the *bias*: minutes WEST of UTC (`UTC = local + bias`),
    /// plus a correction while daylight time is in force. The contract wants
    /// minutes east, so negate.
    pub(super) fn offset_from_bias(
        bias: i32,
        standard_bias: i32,
        daylight_bias: i32,
        id: u32,
    ) -> i32 {
        let adjust = if id == TIME_ZONE_ID_DAYLIGHT {
            daylight_bias
        } else {
            standard_bias
        };
        -(bias + adjust)
    }

    pub(super) fn utc_offset_minutes() -> Option<i32> {
        // SAFETY: `info` is a correctly laid-out, writable out-parameter.
        let (id, info) = unsafe {
            let mut info: TimeZoneInformation = std::mem::zeroed();
            let id = GetTimeZoneInformation(&mut info);
            (id, info)
        };
        if id == TIME_ZONE_ID_INVALID {
            return None;
        }
        Some(offset_from_bias(
            info.bias,
            info.standard_bias,
            info.daylight_bias,
            id,
        ))
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(super) fn hardware_model() -> Option<String> {
        // The SMBIOS product name Windows caches at boot.
        let subkey = wide(r"HARDWARE\DESCRIPTION\System\BIOS");
        let value = wide("SystemProductName");
        let mut len: u32 = 0;
        // SAFETY: a null buffer asks for the required size (in bytes) only.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        if rc != 0 || len < 2 {
            return None;
        }
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        // SAFETY: `buf` holds at least `len` bytes, the size just reported.
        let rc = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return None;
        }
        buf.truncate(len as usize / 2);
        clean_model(&String::from_utf16_lossy(&buf))
    }

    /// `en-US` -> `en_US`; a script subtag (`zh-Hans-CN`) is dropped so the
    /// value matches the language_REGION form the other SDKs send.
    pub(super) fn locale_from_bcp47(tag: &str) -> Option<String> {
        let mut parts = tag.split('-');
        let lang = parts.next().filter(|l| !l.is_empty())?;
        match parts.next_back() {
            Some(region) => Some(format!("{lang}_{region}")),
            None => Some(lang.to_string()),
        }
    }

    pub(super) fn locale() -> Option<String> {
        // LOCALE_NAME_MAX_LENGTH is 85, including the terminator.
        let mut buf = [0u16; 85];
        // SAFETY: `buf` is writable for the `len` code units we pass.
        let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 1 {
            return None;
        }
        locale_from_bcp47(&String::from_utf16_lossy(&buf[..n as usize - 1]))
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub(super) fn utc_offset_minutes() -> Option<i32> {
        None
    }
    pub(super) fn hardware_model() -> Option<String> {
        None
    }
    pub(super) fn locale() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_and_blank_models_are_reported_as_absent() {
        for raw in [
            "",
            "   ",
            "\0",
            "To Be Filled By O.E.M.",
            "System Product Name",
            "Default string",
            "unknown",
        ] {
            assert_eq!(clean_model(raw), None, "{raw:?} must be absent");
        }
    }

    #[test]
    fn real_models_are_trimmed_and_nul_stripped() {
        assert_eq!(
            clean_model("MacBookPro18,3\n").as_deref(),
            Some("MacBookPro18,3")
        );
        // Device-tree `model` files end in a NUL.
        assert_eq!(
            clean_model("Raspberry Pi 4 Model B Rev 1.4\0").as_deref(),
            Some("Raspberry Pi 4 Model B Rev 1.4")
        );
    }

    #[test]
    fn utc_offset_is_a_plausible_whole_minute_value() {
        // Real offsets span UTC-12:00 to UTC+14:00. The exact value depends on
        // the runner, so this pins "resolved and sane", not a number.
        let off = utc_offset_minutes().expect("local offset must resolve on a desktop host");
        assert!((-12 * 60..=14 * 60).contains(&off), "implausible: {off}");
    }

    #[cfg(unix)]
    #[test]
    fn utc_offset_agrees_with_the_tz_environment() {
        // `date +%z` is the independent oracle: same zone database, different
        // code path. Compared only when `date` exists (it does on every CI
        // image we run).
        let Ok(out) = std::process::Command::new("date").arg("+%z").output() else {
            return;
        };
        let z = String::from_utf8_lossy(&out.stdout);
        let z = z.trim();
        if z.len() != 5 {
            return;
        }
        let sign = if z.starts_with('-') { -1 } else { 1 };
        let (h, m): (i32, i32) = (z[1..3].parse().unwrap(), z[3..5].parse().unwrap());
        assert_eq!(utc_offset_minutes(), Some(sign * (h * 60 + m)));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn macos_reports_a_hardware_model() {
        let model = hardware_model().expect("hw.model must resolve on Apple hosts");
        assert!(model.contains(','), "expected e.g. MacBookPro18,3: {model}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_bias_is_negated_and_daylight_adjusted() {
        use super::imp::offset_from_bias;
        // US Eastern: bias 300 west; DST bias -60 -> UTC-4 while in force.
        assert_eq!(offset_from_bias(300, 0, -60, 1), -300);
        assert_eq!(offset_from_bias(300, 0, -60, 2), -240);
        // India has no DST and a half-hour zone: UTC+5:30.
        assert_eq!(offset_from_bias(-330, 0, 0, 0), 330);
    }

    #[cfg(windows)]
    #[test]
    fn windows_locale_tags_map_to_the_posix_form() {
        use super::imp::locale_from_bcp47;
        assert_eq!(locale_from_bcp47("en-US").as_deref(), Some("en_US"));
        assert_eq!(locale_from_bcp47("zh-Hans-CN").as_deref(), Some("zh_CN"));
        assert_eq!(locale_from_bcp47("fr").as_deref(), Some("fr"));
        assert_eq!(locale_from_bcp47(""), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_resolves_locale_and_model_on_a_real_host() {
        let locale = locale().expect("user locale must resolve on Windows");
        assert!(locale.len() >= 2, "{locale}");
        // Model may legitimately be absent on a VM with placeholder SMBIOS, so
        // only assert it never comes back as a placeholder.
        if let Some(m) = hardware_model() {
            assert_eq!(clean_model(&m).as_deref(), Some(m.as_str()));
        }
    }
}

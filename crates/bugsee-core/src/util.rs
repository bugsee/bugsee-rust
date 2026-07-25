//
//  util.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Small dependency-light helpers: epoch-ms wall clock, random hex ids, and
//! ISO-8601 formatting (no `chrono`/`time` dependency).

use std::time::{SystemTime, UNIX_EPOCH};

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn epoch_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        // Clock before 1970 — clamp to 0 rather than panic.
        Err(_) => 0,
    }
}

/// A lowercase hex string of `n_bytes` random bytes (`2 * n_bytes` chars).
pub fn random_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    // getrandom only fails on platforms without an entropy source; fall back to
    // a time-seeded value so id generation never panics.
    if getrandom::getrandom(&mut buf).is_err() {
        let seed = epoch_ms() as u64;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (seed.rotate_left(i as u32 * 8) & 0xff) as u8;
        }
    }
    let mut s = String::with_capacity(n_bytes * 2);
    for b in buf {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// A random `f64` in `[0, 1)`. Fails open (returns `1.0`) if entropy is
/// unavailable, so sampling never accidentally drops a report on error.
pub fn random_unit_f64() -> f64 {
    let mut buf = [0u8; 8];
    if getrandom::getrandom(&mut buf).is_err() {
        return 1.0;
    }
    let v = u64::from_le_bytes(buf);
    // Use the top 53 bits for a uniform double in [0, 1).
    (v >> 11) as f64 / ((1u64 << 53) as f64)
}

/// Format an epoch-ms instant as ISO-8601 UTC with milliseconds and a `Z`
/// suffix (e.g. `2026-07-25T14:23:51.117Z`).
pub fn iso8601_ms(epoch_ms: i64) -> String {
    let (secs, millis) = (epoch_ms.div_euclid(1000), epoch_ms.rem_euclid(1000));
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, hh, mm, ss, millis
    )
}

/// Convert a count of days since the Unix epoch to a `(year, month, day)`
/// proleptic-Gregorian date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_known_instant() {
        // 1_700_000_000 s (a widely-known epoch) == 2023-11-14T22:13:20 UTC.
        assert_eq!(iso8601_ms(1_700_000_000_000), "2023-11-14T22:13:20.000Z");
        // Sub-second millis are preserved.
        assert_eq!(iso8601_ms(1_700_000_000_117), "2023-11-14T22:13:20.117Z");
    }

    #[test]
    fn iso8601_epoch_zero() {
        assert_eq!(iso8601_ms(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn random_hex_length_and_charset() {
        let s = random_hex(16);
        assert_eq!(s.len(), 32);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}

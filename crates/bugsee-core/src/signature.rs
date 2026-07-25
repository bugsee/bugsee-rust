//
//  signature.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Crash dedup signatures — the lowercase SHA-1 hex fed into
//! `request.json`/`crash.json` `signatures[]`. Two feeds mirror the Bugsee model
//! (DESIGN.md §9): a panic/handled-error feed over normalized Rust frames, and
//! a native feed over `signal + module + relative-PC`. Rust is a third
//! representation and is intentionally not byte-identical to iOS/Android.

use sha1::{Digest, Sha1};

fn hex(digest: impl AsRef<[u8]>) -> String {
    let mut s = String::with_capacity(40);
    for b in digest.as_ref() {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
}

/// Lowercase SHA-1 hex over the concatenation of `inputs`.
pub fn sha1_hex(inputs: &[&[u8]]) -> String {
    let mut h = Sha1::new();
    for chunk in inputs {
        h.update(chunk);
    }
    hex(h.finalize())
}

/// Normalize a Rust frame descriptor so grouping is stable across builds:
/// strip the trailing `::h<hex>` symbol hash and collapse closure markers.
pub fn normalize_frame(frame: &str) -> String {
    let mut f = frame.to_string();
    // Drop a trailing rustc symbol hash like `::h3a4b5c6d7e8f9012`.
    if let Some(idx) = f.rfind("::h") {
        let tail = &f[idx + 3..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            f.truncate(idx);
        }
    }
    f.replace("{{closure}}", "{closure}")
}

/// Normalize a panic/exception message so dynamic values (indices, ids) don't
/// fragment grouping: replace runs of digits with `#`.
pub fn normalize_reason(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut prev_digit = false;
    for c in reason.chars() {
        if c.is_ascii_digit() {
            if !prev_digit {
                out.push('#');
            }
            prev_digit = true;
        } else {
            out.push(c);
            prev_digit = false;
        }
    }
    out
}

/// Signature for a Rust panic or handled error. Feeds the exception name, the
/// normalized reason, each (non-hidden) frame, a handled marker, and the domain.
pub fn panic_signature(
    name: &str,
    reason: &str,
    frames: &[String],
    handled: bool,
    domain: Option<&str>,
) -> String {
    let mut h = Sha1::new();
    h.update(name.as_bytes());
    h.update(normalize_reason(reason).as_bytes());
    for frame in frames {
        h.update(normalize_frame(frame).as_bytes());
    }
    h.update(if handled { b"1" } else { b"0" });
    if let Some(d) = domain {
        h.update(d.as_bytes());
    }
    hex(h.finalize())
}

/// Signature for a native fatal crash: `signal + module + relative-PC` per frame
/// of the crashing thread.
pub fn native_signature(signal: &str, frames: &[(String, u64)]) -> String {
    let mut h = Sha1::new();
    h.update(signal.as_bytes());
    for (module, rel_pc) in frames {
        h.update(module.as_bytes());
        h.update(rel_pc.to_le_bytes());
    }
    hex(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_empty_is_known_vector() {
        assert_eq!(sha1_hex(&[]), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            sha1_hex(&[b"abc"]),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn signature_is_lowercase_40_hex_and_deterministic() {
        let frames = vec!["app::checkout::pay".to_string()];
        let a = panic_signature("index out of bounds", "len is 3", &frames, false, None);
        let b = panic_signature("index out of bounds", "len is 3", &frames, false, None);
        assert_eq!(a, b);
        assert_eq!(a.len(), 40);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn frame_hash_suffix_is_normalized_away() {
        assert_eq!(
            normalize_frame("app::checkout::pay::h3a4b5c6d7e8f9012"),
            "app::checkout::pay"
        );
        // A non-hash `::h...` tail is preserved.
        assert_eq!(normalize_frame("app::hasher::run"), "app::hasher::run");
    }

    #[test]
    fn dynamic_reason_values_do_not_change_signature() {
        let frames = vec!["a::b".to_string()];
        let s1 = panic_signature("E", "failed id 12345", &frames, true, None);
        let s2 = panic_signature("E", "failed id 98761", &frames, true, None);
        assert_eq!(
            s1, s2,
            "digit runs are normalized so ids don't fragment groups"
        );
    }

    #[test]
    fn different_stacks_differ() {
        let s1 = panic_signature("E", "r", &["a::one".into()], false, None);
        let s2 = panic_signature("E", "r", &["a::two".into()], false, None);
        assert_ne!(s1, s2);
    }

    #[test]
    fn handled_marker_and_domain_affect_signature() {
        let f = vec!["a::b".to_string()];
        assert_ne!(
            panic_signature("E", "r", &f, true, None),
            panic_signature("E", "r", &f, false, None)
        );
        assert_ne!(
            panic_signature("E", "r", &f, true, None),
            panic_signature("E", "r", &f, true, Some("AppHang::Fair"))
        );
    }
}

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
/// strip the rustc `::h<hex>` symbol hash and collapse closure markers.
///
/// Real frames are `"{symbol} ({file}:{line})"`, and `backtrace`'s `Display`
/// symbol RETAINS the hash (e.g. `app::pay::h1406d87bf3ffb336`). Because the
/// hash is followed by `" (file:line)"`, a "the whole tail is hex" test never
/// fires — so we strip every `::h<16-hex>` run *wherever* it appears, bounded by
/// a non-hex-digit boundary. That is exactly the rustc symbol-hash shape (16 hex
/// digits) and so leaves a real module segment such as `::hasher` untouched.
pub fn normalize_frame(frame: &str) -> String {
    strip_symbol_hashes(frame).replace("{{closure}}", "{closure}")
}

/// Remove every `::h<exactly 16 hex digits>` rustc symbol-hash occurrence,
/// preserving all other bytes (UTF-8 safe: it copies string slices and only
/// byte-matches the ASCII marker `::h` and ASCII hex digits).
fn strip_symbol_hashes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut copied = 0; // byte index up to which `out` already holds `s`
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"::h" {
            let start = i + 3;
            let mut j = start;
            while j < bytes.len() && bytes[j].is_ascii_hexdigit() {
                j += 1;
            }
            // A rustc symbol hash is exactly 16 hex digits terminated by a
            // non-hex boundary (space, `(`, `:`, `<`, end, …). Only then strip.
            if j - start == 16 {
                out.push_str(&s[copied..i]);
                copied = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
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
    // A `0x00` separator between every variable-length segment prevents the
    // `H(a ‖ b) == H(ab)` ambiguity (a boundary-shifted name/reason/frame must
    // not collide with a different split).
    const SEP: &[u8] = b"\x00";
    let mut h = Sha1::new();
    h.update(name.as_bytes());
    h.update(SEP);
    h.update(normalize_reason(reason).as_bytes());
    h.update(SEP);
    for frame in frames {
        h.update(normalize_frame(frame).as_bytes());
        h.update(SEP);
    }
    h.update(if handled { b"1" } else { b"0" });
    h.update(SEP);
    if let Some(d) = domain {
        h.update(d.as_bytes());
    }
    hex(h.finalize())
}

/// Signature for a native fatal crash: `signal + module + relative-PC` per frame
/// of the crashing thread.
pub fn native_signature(signal: &str, frames: &[(String, u64)]) -> String {
    const SEP: &[u8] = b"\x00";
    let mut h = Sha1::new();
    h.update(signal.as_bytes());
    h.update(SEP);
    for (module, rel_pc) in frames {
        h.update(module.as_bytes());
        h.update(SEP);
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
    fn real_frame_format_hash_is_stripped_and_build_stable() {
        // The actual producers emit "{symbol-with-hash} ({file}:{line})"; the
        // hash sits in the MIDDLE of the string, not at the end.
        assert_eq!(
            normalize_frame("sigcheck::main::h1406d87bf3ffb336 (src/main.rs:10)"),
            "sigcheck::main (src/main.rs:10)"
        );
        // Two builds differing only in the per-build symbol hash must produce the
        // same signature — otherwise dedup/blacklist break on every recompile.
        let a = panic_signature(
            "panic",
            "boom",
            &["app::pay::h1111111111111111 (a.rs:1)".into()],
            false,
            None,
        );
        let b = panic_signature(
            "panic",
            "boom",
            &["app::pay::h2222222222222222 (a.rs:1)".into()],
            false,
            None,
        );
        assert_eq!(a, b, "per-build symbol hash must not affect the signature");
        // A generic frame with an inner hash is also stripped.
        assert_eq!(
            normalize_frame("core::ptr::drop_in_place::<T>::h00ff00ff00ff00ff (lib.rs:1)"),
            "core::ptr::drop_in_place::<T> (lib.rs:1)"
        );
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

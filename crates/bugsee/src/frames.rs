//
//  frames.rs
//  bugsee
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Backtrace capture → `crash.json` frames. Leading SDK-internal frames are
//! marked `hidden` so they don't pollute grouping.

use bugsee_core::model::crash::{is_internal_frame, Frame, FrameData};

/// Capture the current call stack as `crash.json` frames, resolving symbols.
pub fn capture() -> Vec<Frame> {
    let mut frames = Vec::new();
    let bt = backtrace::Backtrace::new();
    for frame in bt.frames() {
        for symbol in frame.symbols() {
            let name = symbol
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let file = symbol.filename().map(|p| p.to_string_lossy().into_owned());
            let line = symbol.lineno().map(|l| l as i64).unwrap_or(-1);

            let hidden = is_internal_frame(&name);
            let (member_class, member) = split_symbol(&name);
            let trace = match (&file, line) {
                (Some(f), l) if l >= 0 => format!("{name} ({f}:{l})"),
                (Some(f), _) => format!("{name} ({f})"),
                (None, _) => name.clone(),
            };
            frames.push(Frame {
                trace,
                hidden,
                data: FrameData {
                    source: file,
                    member_class,
                    member: Some(member),
                    line,
                },
            });
        }
    }
    frames
}

/// Split a symbol path into `(module_path, function)`.
fn split_symbol(name: &str) -> (Option<String>, String) {
    // Strip a trailing rustc hash before splitting.
    let stripped = match name.rfind("::h") {
        Some(idx)
            if name[idx + 3..].chars().all(|c| c.is_ascii_hexdigit()) && idx + 3 < name.len() =>
        {
            &name[..idx]
        }
        _ => name,
    };
    match stripped.rfind("::") {
        Some(idx) => (
            Some(stripped[..idx].to_string()),
            stripped[idx + 2..].to_string(),
        ),
        None => (None, stripped.to_string()),
    }
}

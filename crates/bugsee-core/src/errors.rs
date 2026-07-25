//
//  errors.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Build handled-error / message reports: assemble a `crash.json` (managed
//! variant, `handled=true`) plus the `ReportMeta` (issue type `error`, trigger
//! `error`) with the computed dedup signature. Frame extraction (backtrace)
//! happens in the host layer; this module is pure and testable.

use serde_json::Map;

use crate::model::crash::{CrashReport, ExceptionInfo, Frame};
use crate::model::enums::{IssueType, Severity, TriggerType};
use crate::model::report::Source;
use crate::reporting::ReportMeta;
use crate::signature::panic_signature;

/// A built report: metadata plus the serialized `crash.json`.
pub struct BuiltReport {
    pub meta: ReportMeta,
    pub crash_json: Vec<u8>,
}

/// A link in an error's `source()` chain.
pub struct Cause {
    pub name: String,
    pub reason: String,
}

/// Build a handled-error report from an error's name, message, `source()` chain
/// (outermost-first), and captured frames.
pub fn build_handled_error(
    name: &str,
    reason: &str,
    causes: &[Cause],
    frames: Vec<Frame>,
    timestamp: i64,
) -> BuiltReport {
    let frame_sigs: Vec<String> = frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    let signature = panic_signature(name, reason, &frame_sigs, true, None);

    // Fold the cause chain from the innermost outward into nested `cause`.
    let mut cause: Option<Box<ExceptionInfo>> = None;
    for c in causes.iter().rev() {
        cause = Some(Box::new(ExceptionInfo {
            name: c.name.clone(),
            reason: c.reason.clone(),
            domain: None,
            frames: Vec::new(),
            cause: cause.take(),
        }));
    }

    let crash = CrashReport {
        uuid: None,
        timestamp,
        handled: true,
        obfuscated: false,
        ndk_crash: false,
        exception_type: "error".into(),
        signatures: vec![signature.clone()],
        exception: ExceptionInfo {
            name: name.into(),
            reason: reason.into(),
            domain: None,
            frames,
            cause,
        },
    };

    let meta = error_meta(signature);
    BuiltReport {
        meta,
        crash_json: crash.to_bytes().unwrap_or_default(),
    }
}

/// Build a `capture_message` report (no error type, a synthetic exception).
pub fn build_message(message: &str, frames: Vec<Frame>, timestamp: i64) -> BuiltReport {
    build_handled_error("Message", message, &[], frames, timestamp)
}

/// Build a Rust panic report. A caught panic is a `handled` `error`; an
/// uncaught panic is an unhandled `crash`.
pub fn build_panic(reason: &str, frames: Vec<Frame>, handled: bool, timestamp: i64) -> BuiltReport {
    let frame_sigs: Vec<String> = frames
        .iter()
        .filter(|f| !f.hidden)
        .map(|f| f.trace.clone())
        .collect();
    let signature = panic_signature("panic", reason, &frame_sigs, handled, None);

    let crash = CrashReport {
        uuid: None,
        timestamp,
        handled,
        obfuscated: false,
        ndk_crash: false,
        exception_type: "exception".into(),
        signatures: vec![signature.clone()],
        exception: ExceptionInfo {
            name: "panic".into(),
            reason: reason.into(),
            domain: None,
            frames,
            cause: None,
        },
    };

    let (issue_type, trigger) = if handled {
        (IssueType::Error, TriggerType::Error)
    } else {
        (IssueType::Crash, TriggerType::Crash)
    };
    let meta = ReportMeta {
        issue_type,
        summary: None,
        description: None,
        labels: Vec::new(),
        severity: Severity::default(),
        email: None,
        signatures: vec![signature],
        source: Source {
            trigger,
            origin: None,
        },
        attrs: Map::new(),
        attachments: Vec::new(),
    };
    BuiltReport {
        meta,
        crash_json: crash.to_bytes().unwrap_or_default(),
    }
}

fn error_meta(signature: String) -> ReportMeta {
    ReportMeta {
        issue_type: IssueType::Error,
        summary: None,
        description: None,
        labels: Vec::new(),
        severity: Severity::default(),
        email: None,
        signatures: vec![signature],
        source: Source {
            trigger: TriggerType::Error,
            origin: None,
        },
        attrs: Map::new(),
        attachments: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::crash::FrameData;
    use serde_json::Value;

    fn frame(trace: &str, hidden: bool) -> Frame {
        Frame {
            trace: trace.into(),
            hidden,
            data: FrameData {
                source: None,
                member_class: None,
                member: None,
                line: -1,
            },
        }
    }

    #[test]
    fn handled_error_shapes_crash_json() {
        let frames = vec![frame("app::pay", false)];
        let built = build_handled_error("PaymentError", "declined", &[], frames, 1000);
        let v: Value = serde_json::from_slice(&built.crash_json).unwrap();
        assert_eq!(v["handled"], true);
        assert_eq!(v["ndkCrash"], false);
        assert_eq!(v["exception_type"], "error");
        assert_eq!(v["exception"]["name"], "PaymentError");
        assert_eq!(v["exception"]["reason"], "declined");
        assert_eq!(v["signatures"].as_array().unwrap().len(), 1);
        assert_eq!(built.meta.issue_type, IssueType::Error);
        assert_eq!(built.meta.source.trigger, TriggerType::Error);
    }

    #[test]
    fn source_chain_nests_causes() {
        let causes = vec![
            Cause {
                name: "IoError".into(),
                reason: "broken pipe".into(),
            },
            Cause {
                name: "Os".into(),
                reason: "EPIPE".into(),
            },
        ];
        let built = build_handled_error("TopError", "request failed", &causes, vec![], 1);
        let v: Value = serde_json::from_slice(&built.crash_json).unwrap();
        assert_eq!(v["exception"]["cause"]["name"], "IoError");
        assert_eq!(v["exception"]["cause"]["cause"]["name"], "Os");
        assert!(v["exception"]["cause"]["cause"]["cause"].is_null());
    }

    #[test]
    fn signature_matches_report_and_crash() {
        let built = build_handled_error("E", "boom", &[], vec![frame("a::b", false)], 1);
        let v: Value = serde_json::from_slice(&built.crash_json).unwrap();
        assert_eq!(
            v["signatures"][0].as_str().unwrap(),
            built.meta.signatures[0]
        );
    }
}

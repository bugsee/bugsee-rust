//
//  lib.rs
//  bugsee-tracing
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `tracing` integration.
//!
//! [`BugseeLayer`] is a [`tracing_subscriber::Layer`] that turns `tracing`
//! events into Bugsee log entries (level-mapped, `target` as the tag, fields as
//! structured data), populating the report timeline. Optionally, error-level
//! events are also captured as non-fatal error reports.
//!
//! ```no_run
//! use tracing_subscriber::prelude::*;
//! tracing_subscriber::registry().with(bugsee_tracing::BugseeLayer::new()).init();
//! ```

use std::fmt::Debug;

use bugsee::core::model::entry::LogEntry;
use bugsee::core::model::enums::{LogLevel, LogSource};
use bugsee::core::util::epoch_ms;
use bugsee::Bugsee;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

/// A tracing layer that forwards events to Bugsee as log entries.
pub struct BugseeLayer {
    min_level: Level,
    capture_errors: bool,
}

impl BugseeLayer {
    /// A layer capturing `INFO` and more-severe events.
    pub fn new() -> Self {
        BugseeLayer {
            min_level: Level::INFO,
            capture_errors: false,
        }
    }

    /// Set the minimum level to capture (more-verbose events are ignored).
    pub fn with_min_level(mut self, level: Level) -> Self {
        self.min_level = level;
        self
    }

    /// Also raise a non-fatal error report for each `ERROR` event.
    pub fn capture_errors(mut self, enabled: bool) -> Self {
        self.capture_errors = enabled;
        self
    }
}

impl Default for BugseeLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Subscriber> Layer<S> for BugseeLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let level = *meta.level();
        // Level ordering: TRACE > DEBUG > INFO > WARN > ERROR. Skip anything more
        // verbose than the configured minimum.
        if level > self.min_level {
            return;
        }

        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message;

        Bugsee::capture_log(LogEntry {
            timestamp: epoch_ms(),
            level: map_level(level),
            source: LogSource::Custom,
            tag: Some(meta.target().to_string()),
            message: Some(message.clone()),
            custom: visitor.fields,
        });

        if self.capture_errors && level == Level::ERROR {
            Bugsee::capture_message(LogLevel::Error, message);
        }
    }
}

fn map_level(level: Level) -> LogLevel {
    match level {
        Level::ERROR => LogLevel::Error,
        Level::WARN => LogLevel::Warning,
        Level::INFO => LogLevel::Info,
        Level::DEBUG => LogLevel::Debug,
        Level::TRACE => LogLevel::Verbose,
    }
}

/// Collects a tracing event's `message` field and other fields.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Map<String, Value>,
}

impl FieldVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            if let Value::String(s) = value {
                self.message = s;
            } else {
                self.message = value.to_string();
            }
        } else {
            self.fields.insert(field.name().to_string(), value);
        }
    }
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.put(field, Value::from(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::from(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }
}

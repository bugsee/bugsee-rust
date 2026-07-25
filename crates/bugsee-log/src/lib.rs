//
//  lib.rs
//  bugsee-log
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! `log` crate integration.
//!
//! [`BugseeLogger`] forwards `log` records to Bugsee as log entries (level
//! mapped, `target` as the tag). Install it as the global logger with [`init`].
//!
//! ```no_run
//! bugsee_log::init().unwrap();
//! log::info!("hello from log");
//! ```

use bugsee::core::model::entry::LogEntry;
use bugsee::core::model::enums::{LogLevel, LogSource};
use bugsee::core::util::epoch_ms;
use bugsee::Bugsee;
use log::{Level, Log, Metadata, Record};

/// A `log::Log` implementation that forwards records to Bugsee.
pub struct BugseeLogger {
    level: Level,
}

impl BugseeLogger {
    /// A logger capturing `Info` and more-severe records.
    pub fn new() -> Self {
        BugseeLogger { level: Level::Info }
    }

    /// A logger capturing `level` and more-severe records.
    pub fn with_level(level: Level) -> Self {
        BugseeLogger { level }
    }
}

impl Default for BugseeLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl Log for BugseeLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        Bugsee::capture_log(LogEntry {
            timestamp: epoch_ms(),
            level: map_level(record.level()),
            source: LogSource::Custom,
            tag: Some(record.target().to_string()),
            message: Some(record.args().to_string()),
            custom: Default::default(),
        });
    }

    fn flush(&self) {}
}

fn map_level(level: Level) -> LogLevel {
    match level {
        Level::Error => LogLevel::Error,
        Level::Warn => LogLevel::Warning,
        Level::Info => LogLevel::Info,
        Level::Debug => LogLevel::Debug,
        Level::Trace => LogLevel::Verbose,
    }
}

/// Install a [`BugseeLogger`] as the global logger at `Info` level.
pub fn init() -> Result<(), log::SetLoggerError> {
    init_with_level(Level::Info)
}

/// Install a [`BugseeLogger`] as the global logger at `level`.
pub fn init_with_level(level: Level) -> Result<(), log::SetLoggerError> {
    log::set_boxed_logger(Box::new(BugseeLogger::with_level(level)))?;
    log::set_max_level(level.to_level_filter());
    Ok(())
}

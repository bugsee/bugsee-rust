//! Time sources.

/// Wall + monotonic clocks used by capture/recovery.
pub trait Clock: Send + Sync {
    /// Unix epoch milliseconds (wall).
    fn unix_time_ms(&self) -> i64;
    /// Monotonic milliseconds (arbitrary epoch; only deltas are meaningful).
    fn mono_time_ms(&self) -> i64;
}

/// Default `std` clock for desktop.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_time_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn mono_time_ms(&self) -> i64 {
        // std Instant is not convertible to a stable integer; use a process-local
        // offset from first call for monotonic deltas within this process.
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        let start = START.get_or_init(Instant::now);
        start.elapsed().as_millis() as i64
    }
}

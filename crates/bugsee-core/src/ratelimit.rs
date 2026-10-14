//
//  ratelimit.rs
//  bugsee-core
//
//  Copyright © 2026 Bugsee. All rights reserved.
//

//! Report-volume limiter: token buckets that stop a noisy error loop from
//! flooding the backend or the local disk.
//!
//! Applied in the capture worker after `before_send`/sampling and **before** any
//! disk work (snapshot, ZIP, durable queue). A rejected report costs one lock
//! acquisition and a hash lookup; the signature key is only copied (one small
//! allocation) the first time a signature is seen, never on a repeat or a
//! rejection of a known signature.
//!
//! * **Errors** (`IssueType::Error`) must pass both a *per-signature* bucket
//!   (so one hot failure cannot starve the rest) and a *global* bucket.
//! * **Crashes** are never subject to those buckets. They draw from a separate,
//!   small *crash floor* bucket that exists only to bound a pathological
//!   in-process crash-report loop; its burst is large enough that a real crash
//!   is always delivered.
//! * **Manual bug reports** (`IssueType::Bug`) are user-initiated and exempt.
//!
//! Memory is bounded: at most [`RateLimitConfig::max_signatures`] per-signature
//! buckets are tracked; beyond that, unseen signatures share one overflow
//! bucket. Suppressed reports are counted and the count is handed to the next
//! admitted *non-crash* report (crashes never consume it) so the backend sees
//! that volume was cut; if that report then fails to assemble the caller returns
//! the count with [`RateLimiter::restore_suppressed`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::fork::ForkLock;
use crate::model::enums::IssueType;

/// A token bucket shape: `burst` tokens available at once, one more every
/// `refill_every`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub burst: u32,
    pub refill_every: Duration,
}

impl Rate {
    pub const fn new(burst: u32, refill_every: Duration) -> Self {
        Rate {
            burst,
            refill_every,
        }
    }
}

/// Rate-limit configuration. The default is on, with limits generous enough
/// that a normal app never sees them.
///
/// Non-exhaustive so options can be added without a breaking change; start from
/// [`RateLimitConfig::default`] and use the chainable setters (or assign fields).
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Master switch. `false` admits everything.
    pub enabled: bool,
    /// All non-crash error reports together. Default: burst 60, 1 per second.
    pub global: Rate,
    /// Error reports sharing one signature. Default: burst 5, 1 per 10 s.
    pub per_signature: Rate,
    /// Crash floor. Default: burst 20, 1 per 6 s.
    pub crash_floor: Rate,
    /// Distinct signatures tracked before overflow sharing. Default 256.
    pub max_signatures: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        RateLimitConfig {
            enabled: true,
            global: Rate::new(60, Duration::from_secs(1)),
            per_signature: Rate::new(5, Duration::from_secs(10)),
            crash_floor: Rate::new(20, Duration::from_secs(6)),
            max_signatures: 256,
        }
    }
}

impl RateLimitConfig {
    /// A configuration that never limits.
    pub fn disabled() -> Self {
        RateLimitConfig {
            enabled: false,
            ..Default::default()
        }
    }

    /// Set the global (all non-crash errors) bucket.
    pub fn global(mut self, rate: Rate) -> Self {
        self.global = rate;
        self
    }

    /// Set the per-signature bucket.
    pub fn per_signature(mut self, rate: Rate) -> Self {
        self.per_signature = rate;
        self
    }

    /// Set the crash-floor bucket (its burst is clamped to at least 1).
    pub fn crash_floor(mut self, rate: Rate) -> Self {
        self.crash_floor = rate;
        self
    }

    /// Set how many distinct signatures are tracked.
    pub fn max_signatures(mut self, n: usize) -> Self {
        self.max_signatures = n;
        self
    }
}

/// The verdict for one report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Deliver. Carries how many reports were suppressed since the last
    /// admitted non-crash one (to summarise on this report; always 0 for a
    /// crash).
    Allow { suppressed_before: u64 },
    /// Drop: a bucket is empty.
    Deny,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Self {
        Bucket {
            tokens: rate.burst as f64,
            last: now,
        }
    }

    fn refill(&mut self, rate: Rate, now: Instant) {
        let dt = now.saturating_duration_since(self.last);
        self.last = now;
        if rate.refill_every.is_zero() {
            self.tokens = rate.burst as f64;
        } else {
            let add = dt.as_secs_f64() / rate.refill_every.as_secs_f64();
            self.tokens = (self.tokens + add).min(rate.burst as f64);
        }
    }

    fn has(&self) -> bool {
        self.tokens >= 1.0
    }

    fn is_full(&self, rate: Rate) -> bool {
        self.tokens >= rate.burst as f64
    }
}

struct State {
    global: Bucket,
    crash: Bucket,
    overflow: Bucket,
    sigs: HashMap<String, Bucket>,
    suppressed_since_allow: u64,
    suppressed_total: u64,
}

/// The limiter. `Send + Sync`; its lock takes part in the `fork()` protocol.
pub struct RateLimiter {
    cfg: RateLimitConfig,
    state: ForkLock<State>,
}

impl RateLimiter {
    pub fn new(mut cfg: RateLimitConfig) -> Self {
        // A crash must always be deliverable: the floor never rounds to zero.
        cfg.crash_floor.burst = cfg.crash_floor.burst.max(1);
        let now = Instant::now();
        let state = State {
            global: Bucket::full(cfg.global, now),
            crash: Bucket::full(cfg.crash_floor, now),
            overflow: Bucket::full(cfg.per_signature, now),
            sigs: HashMap::new(),
            suppressed_since_allow: 0,
            suppressed_total: 0,
        };
        RateLimiter {
            cfg,
            state: ForkLock::new(state),
        }
    }

    /// Decide a report of `kind` with dedup signature `signature` (`""` when it
    /// has none) arriving now.
    pub fn check(&self, kind: IssueType, signature: &str) -> Decision {
        self.check_at(kind, signature, Instant::now())
    }

    /// [`check`](Self::check) with an explicit clock, for tests.
    pub fn check_at(&self, kind: IssueType, signature: &str, now: Instant) -> Decision {
        if !self.cfg.enabled || kind == IssueType::Bug {
            return Decision::Allow {
                suppressed_before: 0,
            };
        }
        let cfg = &self.cfg;
        let mut st = self.state.lock();
        let st = &mut *st;
        let allowed = if kind == IssueType::Crash {
            st.crash.refill(cfg.crash_floor, now);
            if st.crash.has() {
                st.crash.tokens -= 1.0;
                true
            } else {
                false
            }
        } else {
            st.global.refill(cfg.global, now);
            // Make room for a new signature before borrowing its bucket.
            if !st.sigs.contains_key(signature) && st.sigs.len() >= cfg.max_signatures {
                let rate = cfg.per_signature;
                st.sigs.retain(|_, b| {
                    b.refill(rate, now);
                    !b.is_full(rate)
                });
            }
            // Look up without allocating; only a first-seen signature is copied.
            if !st.sigs.contains_key(signature) && st.sigs.len() < cfg.max_signatures {
                st.sigs
                    .insert(signature.to_owned(), Bucket::full(cfg.per_signature, now));
            }
            let bucket = match st.sigs.get_mut(signature) {
                Some(b) => b,
                None => &mut st.overflow,
            };
            bucket.refill(cfg.per_signature, now);
            // Spend from both buckets or neither.
            if bucket.has() && st.global.has() {
                bucket.tokens -= 1.0;
                st.global.tokens -= 1.0;
                true
            } else {
                false
            }
        };
        if allowed {
            // A crash never consumes the summary: it belongs to the error
            // stream that was cut.
            let suppressed_before = if kind == IssueType::Crash {
                0
            } else {
                std::mem::take(&mut st.suppressed_since_allow)
            };
            Decision::Allow { suppressed_before }
        } else {
            st.suppressed_since_allow += 1;
            st.suppressed_total += 1;
            Decision::Deny
        }
    }

    /// Give back a suppressed count that was handed out by [`Decision::Allow`]
    /// but could not be delivered, so the next admitted report carries it.
    pub fn restore_suppressed(&self, n: u64) {
        self.state.lock().suppressed_since_allow += n;
    }

    /// Total reports suppressed since launch.
    pub fn suppressed_total(&self) -> u64 {
        self.state.lock().suppressed_total
    }

    /// Number of per-signature buckets currently tracked (bounded).
    pub fn tracked_signatures(&self) -> usize {
        self.state.lock().sigs.len()
    }

    pub(crate) fn fork_lock(&self) {
        self.state.fork_lock();
    }

    pub(crate) fn fork_unlock(&self) {
        self.state.fork_unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter() -> RateLimiter {
        RateLimiter::new(RateLimitConfig {
            global: Rate::new(10, Duration::from_secs(1)),
            per_signature: Rate::new(3, Duration::from_secs(10)),
            crash_floor: Rate::new(4, Duration::from_secs(60)),
            max_signatures: 4,
            ..Default::default()
        })
    }

    fn allowed(d: Decision) -> bool {
        matches!(d, Decision::Allow { .. })
    }

    #[test]
    fn per_signature_burst_then_deny_then_refill() {
        let l = limiter();
        let t0 = Instant::now();
        // Another signature is unaffected.
        assert!(allowed(l.check_at(IssueType::Error, "b", t0)));
        let n = (0..10)
            .filter(|_| allowed(l.check_at(IssueType::Error, "a", t0)))
            .count();
        assert_eq!(n, 3);
        // 10 s later one token is back, and the summary reports the 7 drops.
        let d = l.check_at(IssueType::Error, "a", t0 + Duration::from_secs(10));
        assert_eq!(
            d,
            Decision::Allow {
                suppressed_before: 7
            }
        );
        assert_eq!(l.suppressed_total(), 7);
    }

    #[test]
    fn global_limit_applies_across_signatures() {
        let l = RateLimiter::new(RateLimitConfig {
            global: Rate::new(10, Duration::from_secs(1)),
            max_signatures: 1000,
            ..Default::default()
        });
        let t0 = Instant::now();
        let n = (0..100)
            .filter(|i| allowed(l.check_at(IssueType::Error, &format!("s{i}"), t0)))
            .count();
        assert_eq!(n, 10);
    }

    #[test]
    fn crashes_bypass_error_buckets_and_bugs_are_exempt() {
        let l = limiter();
        let t0 = Instant::now();
        for _ in 0..100 {
            l.check_at(IssueType::Error, "x", t0);
        }
        // Errors are exhausted globally, yet crashes up to the floor pass.
        for _ in 0..4 {
            assert!(allowed(l.check_at(IssueType::Crash, "x", t0)));
        }
        assert!(!allowed(l.check_at(IssueType::Crash, "x", t0)));
        for _ in 0..50 {
            assert!(allowed(l.check_at(IssueType::Bug, "x", t0)));
        }
    }

    #[test]
    fn memory_is_bounded_under_signature_churn() {
        let l = limiter();
        let t0 = Instant::now();
        for i in 0..10_000 {
            l.check_at(IssueType::Error, &format!("sig{i}"), t0);
        }
        assert!(l.tracked_signatures() <= 4);
    }

    #[test]
    fn crash_floor_never_rounds_to_zero_and_crashes_leave_the_summary_alone() {
        let l = RateLimiter::new(RateLimitConfig {
            crash_floor: Rate::new(0, Duration::from_secs(60)),
            per_signature: Rate::new(1, Duration::from_secs(60)),
            ..Default::default()
        });
        let t0 = Instant::now();
        assert!(allowed(l.check_at(IssueType::Error, "a", t0)));
        assert_eq!(l.check_at(IssueType::Error, "a", t0), Decision::Deny);
        assert_eq!(
            l.check_at(IssueType::Crash, "a", t0),
            Decision::Allow {
                suppressed_before: 0
            }
        );
        // The summary is still pending for the next error, and survives a restore.
        assert_eq!(
            l.check_at(IssueType::Error, "b", t0),
            Decision::Allow {
                suppressed_before: 1
            }
        );
        l.restore_suppressed(1);
        assert_eq!(
            l.check_at(IssueType::Error, "c", t0),
            Decision::Allow {
                suppressed_before: 1
            }
        );
    }

    #[test]
    fn disabled_admits_everything() {
        let l = RateLimiter::new(RateLimitConfig::disabled());
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert!(allowed(l.check_at(IssueType::Error, "a", t0)));
        }
    }
}

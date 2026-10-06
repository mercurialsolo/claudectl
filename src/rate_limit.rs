//! In-process token-bucket rate limiter (#344, RFC v2 §9; #431, open-cluster
//! RFC §4.8).
//!
//! Two callers, one shape. The bus MCP server keys on `sender_role`
//! (`src/bus/mcp.rs`); the query surface keys on a *verified* grant id
//! (`src/query/core.rs`). Both are single-process surfaces — every call lands
//! in one handler — so a single-process bucket is enough and no cross-process
//! coordination is needed.
//!
//! This module started as `src/bus/rate_limit.rs`. #431 moved it up to the
//! crate root because the query surface is gated on `relay` while this lived
//! behind `bus`, and the alternative was forcing `bus` onto an HTTP surface
//! that has no other use for rmcp, Tokio or SQLite. It is gated on
//! `any(bus, relay)` rather than left ungated so the minimal
//! `--no-default-features --features hive` build does not carry it as dead
//! code.
//!
//! The bucket refills continuously at `capacity / window_secs` tokens per
//! second, so a burst can drain it immediately and is then held to the
//! steady-state rate. No background timer: refill is computed from elapsed
//! time on each call.
//!
//! **Per-key capacity.** The bus uses one capacity for every role; a grant
//! carries its own `rate_limit_per_min`, so [`RateLimiter::try_acquire_with_capacity`]
//! takes it per call. A bucket keeps whatever capacity it was created with for
//! the life of the process, which means editing a grant file changes its limit
//! on the next restart rather than on the next request. That is the right
//! trade: re-reading capacity per call would let a bucket's ceiling move under
//! it mid-window, and the limit is the owner's knob, not a hot-path input.
//!
//! **No eviction, deliberately.** The map grows one entry per distinct key.
//! For the bus that is the bound set of bound roles. For the query surface the
//! key is only ever a grant id that has already *passed* verification — which
//! is why `QueryCore` checks the limit after `verify` and not before. Keyed on
//! a parsed-but-unverified id instead, someone enumerating 24-bit grant ids
//! could pin 16M buckets in memory.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Default bucket capacity (60 messages) over the default window (60s).
///
/// Matches a comfortable supervisor cadence — one message per second per
/// sender. The query surface passes a grant's own limit instead, which the
/// RFC defaults to 20/min.
pub const DEFAULT_CAPACITY: u32 = 60;
pub const DEFAULT_WINDOW_SECS: u32 = 60;

#[derive(Debug)]
struct Bucket {
    /// Available tokens at the last refill.
    tokens: f64,
    /// When the bucket was last refilled. Used to compute the steady-state
    /// gain on each call without a background timer thread.
    last_refill: Instant,
    /// This key's ceiling, fixed when the bucket was created.
    capacity: f64,
    /// `capacity / window_secs`, cached so the hot path is one multiply.
    refill_per_sec: f64,
}

pub struct RateLimiter {
    capacity: u32,
    window_secs: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(capacity: u32, window_secs: u32) -> Self {
        Self {
            capacity: capacity.max(1),
            window_secs: window_secs.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_WINDOW_SECS)
    }

    /// Consume one token for `key` at this limiter's default capacity.
    ///
    /// Returns true when the call is permitted (a token was available and has
    /// been deducted), false when `key` has exhausted its window.
    ///
    /// `now` is injected so tests can drive the clock without sleeping. The
    /// production caller passes `Instant::now()`.
    pub fn try_acquire(&self, key: &str, now: Instant) -> bool {
        self.try_acquire_with_capacity(key, self.capacity, now)
    }

    /// Consume one token for `key`, creating its bucket at `capacity` if this
    /// is the first call for that key.
    ///
    /// `capacity` applies only at creation — see the per-key capacity note in
    /// the module docs. A capacity of 0 is raised to 1 rather than locking the
    /// key out entirely, matching [`Self::new`]: a grant file with
    /// `rate_limit_per_min: 0` is far more likely to be a mistake than a
    /// deliberate denial of the grant, and revoking is the way to say that.
    pub fn try_acquire_with_capacity(&self, key: &str, capacity: u32, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");
        let bucket = buckets.entry(key.to_string()).or_insert_with(|| {
            let cap = capacity.max(1) as f64;
            Bucket {
                tokens: cap,
                last_refill: now,
                capacity: cap,
                refill_per_sec: cap / self.window_secs as f64,
            }
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_per_sec).min(bucket.capacity);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Seconds until `key` would have a token again, for a `Retry-After`
    /// header. `None` when a token is available now or the key is unknown.
    ///
    /// Read-only: it never deducts, so calling it on the refusal path cannot
    /// make the refusal worse.
    pub fn retry_after_secs(&self, key: &str, now: Instant) -> Option<u64> {
        let buckets = self.buckets.lock().expect("rate limiter mutex poisoned");
        let bucket = buckets.get(key)?;
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        let tokens = (bucket.tokens + elapsed * bucket.refill_per_sec).min(bucket.capacity);
        if tokens >= 1.0 {
            return None;
        }
        let deficit = 1.0 - tokens;
        Some((deficit / bucket.refill_per_sec).ceil().max(1.0) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_burst_up_to_capacity() {
        let rl = RateLimiter::new(5, 60);
        let t = Instant::now();
        for _ in 0..5 {
            assert!(rl.try_acquire("backend", t));
        }
        assert!(
            !rl.try_acquire("backend", t),
            "sixth call within zero elapsed time must be denied"
        );
    }

    #[test]
    fn refills_at_steady_state() {
        // capacity 60, window 60s → refill 1/sec. After 10s of zero traffic
        // we should regain ~10 tokens.
        let rl = RateLimiter::new(60, 60);
        let t0 = Instant::now();
        // Drain the bucket.
        for _ in 0..60 {
            assert!(rl.try_acquire("backend", t0));
        }
        assert!(!rl.try_acquire("backend", t0));
        // Ten seconds later — should be able to take ten more, then stall.
        let t1 = t0 + Duration::from_secs(10);
        for _ in 0..10 {
            assert!(rl.try_acquire("backend", t1));
        }
        assert!(!rl.try_acquire("backend", t1));
    }

    #[test]
    fn limits_are_per_role() {
        let rl = RateLimiter::new(2, 60);
        let t = Instant::now();
        assert!(rl.try_acquire("a", t));
        assert!(rl.try_acquire("a", t));
        assert!(!rl.try_acquire("a", t));
        // 'b' has its own bucket.
        assert!(rl.try_acquire("b", t));
        assert!(rl.try_acquire("b", t));
        assert!(!rl.try_acquire("b", t));
    }

    #[test]
    fn never_exceeds_capacity_during_long_idle() {
        // A role that idled for a year should not accumulate a year's
        // worth of tokens — capacity is a hard ceiling.
        let rl = RateLimiter::new(5, 60);
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(365 * 24 * 3600);
        for _ in 0..5 {
            assert!(rl.try_acquire("backend", t1));
        }
        assert!(!rl.try_acquire("backend", t1));
    }

    // ── #431: per-key capacity and the retry hint ───────────────────────────

    #[test]
    fn each_key_gets_the_capacity_it_was_created_with() {
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        // Two grants with different limits on one limiter.
        for _ in 0..2 {
            assert!(rl.try_acquire_with_capacity("gr_small", 2, t));
        }
        assert!(!rl.try_acquire_with_capacity("gr_small", 2, t));

        for _ in 0..5 {
            assert!(rl.try_acquire_with_capacity("gr_big", 5, t));
        }
        assert!(!rl.try_acquire_with_capacity("gr_big", 5, t));
    }

    #[test]
    fn a_keys_capacity_is_fixed_at_creation_not_read_per_call() {
        // Documented behaviour: editing a grant file changes its limit on the
        // next restart, not mid-window. Re-reading capacity per call would let
        // a bucket's ceiling move under it.
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        assert!(rl.try_acquire_with_capacity("gr_x", 1, t));
        assert!(
            !rl.try_acquire_with_capacity("gr_x", 1_000, t),
            "the raised capacity must not take effect on an existing bucket"
        );
    }

    #[test]
    fn a_zero_capacity_is_raised_to_one_rather_than_locking_the_key_out() {
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        assert!(
            rl.try_acquire_with_capacity("gr_zero", 0, t),
            "revoking is how you deny a grant; a 0 in the file is a mistake"
        );
        assert!(!rl.try_acquire_with_capacity("gr_zero", 0, t));
    }

    #[test]
    fn retry_after_is_none_while_tokens_remain_and_positive_once_empty() {
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        assert_eq!(
            rl.retry_after_secs("gr_unknown", t),
            None,
            "a key with no bucket is not throttled"
        );
        assert!(rl.try_acquire_with_capacity("gr_y", 2, t));
        assert_eq!(rl.retry_after_secs("gr_y", t), None, "one token left");
        assert!(rl.try_acquire_with_capacity("gr_y", 2, t));
        let wait = rl.retry_after_secs("gr_y", t).expect("empty bucket");
        // 2 tokens over 60s refills one every 30s.
        assert_eq!(wait, 30, "got {wait}");
    }

    #[test]
    fn retry_after_does_not_consume_a_token() {
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        assert!(rl.try_acquire_with_capacity("gr_z", 2, t));
        for _ in 0..10 {
            let _ = rl.retry_after_secs("gr_z", t);
        }
        assert!(
            rl.try_acquire_with_capacity("gr_z", 2, t),
            "asking when to retry must not spend the token being waited for"
        );
    }

    #[test]
    fn a_waited_out_bucket_admits_again() {
        let rl = RateLimiter::new(60, 60);
        let t = Instant::now();
        assert!(rl.try_acquire_with_capacity("gr_w", 1, t));
        assert!(!rl.try_acquire_with_capacity("gr_w", 1, t));
        let wait = rl.retry_after_secs("gr_w", t).expect("empty");
        assert!(rl.try_acquire_with_capacity("gr_w", 1, t + Duration::from_secs(wait)));
    }
}

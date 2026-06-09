//! Per-role token-bucket rate limiter for `publish` (#344, RFC v2 §9).
//!
//! The bus MCP server is a single in-process surface — every `publish` call
//! lands in one `BusServer::publish` handler holding a mutex on its state.
//! So a single-process token bucket keyed on `sender_role` is enough; we
//! don't need cross-process coordination here.
//!
//! Default capacity matches a comfortable supervisor cadence: 60 messages
//! per minute per sender (one per second). The bucket refills continuously
//! at `capacity / window_secs` tokens per second; a burst can drain it
//! immediately but is then forced to wait for steady-state refill. Senders
//! without a role still pass through — anonymous traffic is rare and the
//! reserved-role guard already shields the privileged endpoints.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Default bucket capacity (60 messages) over the default window (60s).
pub const DEFAULT_CAPACITY: u32 = 60;
pub const DEFAULT_WINDOW_SECS: u32 = 60;

#[derive(Debug)]
struct Bucket {
    /// Available tokens at the last refill.
    tokens: f64,
    /// When the bucket was last refilled. Used to compute the steady-state
    /// gain on each call without a background timer thread.
    last_refill: Instant,
}

pub struct RateLimiter {
    capacity: u32,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(capacity: u32, window_secs: u32) -> Self {
        let cap = capacity.max(1) as f64;
        let win = window_secs.max(1) as f64;
        Self {
            capacity,
            refill_per_sec: cap / win,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_CAPACITY, DEFAULT_WINDOW_SECS)
    }

    /// Consume one token for `role`. Returns true when the call is permitted
    /// (the token was available and has been deducted), false when the role
    /// has exhausted its budget for the current window.
    ///
    /// `now` is injected so tests can drive the clock without sleeping. The
    /// production caller passes `Instant::now()`.
    pub fn try_acquire(&self, role: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");
        let bucket = buckets.entry(role.to_string()).or_insert_with(|| Bucket {
            tokens: self.capacity as f64,
            last_refill: now,
        });
        let elapsed = now
            .saturating_duration_since(bucket.last_refill)
            .as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity as f64);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
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
}

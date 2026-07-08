//! Token-bucket rate limiter (IMP-15).
//!
//! A std-only, deterministic global limiter for the proxy. The default
//! deployment is localhost-only (I5), so this is off by default; it exists to
//! make *exposed* deployments (`PASTURE_LISTEN_ADDR=0.0.0.0:…`) safe against a
//! request flood. The bucket refills continuously so a steady rate is allowed
//! while bursts are capped at the per-minute budget.

use std::time::Instant;

/// A continuously-refilling token bucket. One token per allowed request.
pub struct RateLimiter {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimiter {
    /// Allow up to `per_minute` requests, with a burst capacity of `per_minute`.
    pub fn per_minute(per_minute: u32) -> Self {
        let cap = per_minute as f64;
        Self {
            capacity: cap,
            tokens: cap,
            refill_per_sec: cap / 60.0,
            last: Instant::now(),
        }
    }

    /// Try to admit one request now. Refills based on elapsed wall-clock time.
    pub fn allow(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.step(elapsed)
    }

    /// Refill by `elapsed_secs` then attempt to take one token. Pure and
    /// deterministic — the unit under test (separated from the clock).
    fn step(&mut self, elapsed_secs: f64) -> bool {
        self.tokens = (self.tokens + elapsed_secs * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            // Clamp to 0.0 so floating-point rounding can never leave tokens
            // slightly negative, which would produce an inflated retry_after_secs.
            self.tokens = (self.tokens - 1.0).max(0.0);
            true
        } else {
            false
        }
    }

    /// Whole seconds until at least one token is available again. Call after
    /// `allow()` returns `false` to populate a `Retry-After` header so a polite
    /// client backs off exactly long enough instead of hammering (RFC 7231
    /// §7.1.3). Returns 0 when a token is already available, and never less than
    /// 1 once the bucket is empty (clients must wait a measurable interval).
    pub fn retry_after_secs(&self) -> u64 {
        if self.tokens >= 1.0 {
            return 0;
        }
        if self.refill_per_sec <= 0.0 {
            // A zero-rate bucket never refills; advise a conservative minute.
            return 60;
        }
        let needed = 1.0 - self.tokens;
        (needed / self.refill_per_sec).ceil().max(1.0) as u64
    }

    /// Refill to now (consuming nothing) and report the state for `X-RateLimit-*`
    /// response headers: `(limit, whole tokens remaining, seconds until reset)`.
    /// `limit` is the per-minute request budget, `remaining` the requests that
    /// would still be admitted right now, and `reset` the seconds until the bucket
    /// next admits a request (0 when one is already available). Lets clients
    /// self-throttle proactively rather than only reacting to a 429.
    pub fn snapshot(&mut self) -> (u32, u32, u64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        let limit = self.capacity.max(0.0) as u32;
        let remaining = self.tokens.floor().max(0.0) as u32;
        (limit, remaining, self.retry_after_secs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_burst_up_to_capacity_then_denied() {
        let mut rl = RateLimiter::per_minute(3);
        // Three immediate requests fit the burst; the fourth (no time passed) is denied.
        assert!(rl.step(0.0));
        assert!(rl.step(0.0));
        assert!(rl.step(0.0));
        assert!(!rl.step(0.0));
    }

    #[test]
    fn test_refill_after_time() {
        let mut rl = RateLimiter::per_minute(60); // 1 token/sec
        for _ in 0..60 {
            assert!(rl.step(0.0));
        }
        assert!(!rl.step(0.0)); // empty
        assert!(rl.step(1.0)); // ~1 token refilled after 1s
        assert!(!rl.step(0.0)); // and it's gone again
    }

    #[test]
    fn test_refill_caps_at_capacity() {
        let mut rl = RateLimiter::per_minute(10);
        // A long idle does not let the bucket exceed its capacity.
        assert!(rl.step(10_000.0));
        for _ in 0..9 {
            assert!(rl.step(0.0));
        }
        assert!(!rl.step(0.0)); // only `capacity` tokens, not more
    }

    #[test]
    fn test_allow_uses_wall_clock() {
        // Smoke test the public method: first call admits, immediate second denies.
        let mut rl = RateLimiter::per_minute(1);
        assert!(rl.allow());
        assert!(!rl.allow());
    }

    #[test]
    fn test_retry_after_zero_when_token_available() {
        let rl = RateLimiter::per_minute(10);
        // Fresh bucket is full, so a request would be admitted -> no wait.
        assert_eq!(rl.retry_after_secs(), 0);
    }

    #[test]
    fn test_retry_after_one_second_at_one_per_sec() {
        let mut rl = RateLimiter::per_minute(60); // 1 token/sec
        for _ in 0..60 {
            assert!(rl.step(0.0));
        }
        assert!(!rl.step(0.0)); // empty
                                // Need one whole token at 1/sec -> 1 second.
        assert_eq!(rl.retry_after_secs(), 1);
    }

    #[test]
    fn test_retry_after_rounds_up_for_slow_refill() {
        let mut rl = RateLimiter::per_minute(30); // 0.5 token/sec
        for _ in 0..30 {
            assert!(rl.step(0.0));
        }
        assert!(!rl.step(0.0)); // empty
                                // Need one token at 0.5/sec -> 2 seconds.
        assert_eq!(rl.retry_after_secs(), 2);
    }

    #[test]
    fn test_snapshot_full_bucket() {
        let mut rl = RateLimiter::per_minute(10);
        let (limit, remaining, reset) = rl.snapshot();
        assert_eq!(limit, 10);
        assert_eq!(remaining, 10); // full bucket, all requests available
        assert_eq!(reset, 0); // a token is available now
    }

    #[test]
    fn test_snapshot_empty_bucket_reports_reset() {
        let mut rl = RateLimiter::per_minute(10); // 1 token / 6s
        for _ in 0..10 {
            assert!(rl.step(0.0));
        }
        let (limit, remaining, reset) = rl.snapshot();
        assert_eq!(limit, 10);
        assert_eq!(remaining, 0); // drained
        assert!(reset >= 1, "empty bucket must report a positive reset");
    }
}

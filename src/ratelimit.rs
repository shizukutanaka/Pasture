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
            self.tokens -= 1.0;
            true
        } else {
            false
        }
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
}

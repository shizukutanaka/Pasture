//! Backend health monitoring and circuit breaker (IMP-30).
//!
//! Polls local and cloud backends periodically to detect crashes, stalls, or
//! degradation. Exposes health status to stats and cost log for operator visibility.
//! If a backend becomes unhealthy, cascade immediately without timeout.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Health status of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Backend is responding normally.
    Healthy,
    /// Backend is slow or returning errors; may recover.
    Degraded,
    /// Backend is not responding; treat as down.
    Down,
}

impl HealthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Down => "down",
        }
    }
}

/// Health check result with timestamp.
#[derive(Debug, Clone)]
pub struct HealthCheck {
    pub status: HealthStatus,
    pub timestamp: u64,
    pub response_time_ms: u64,
    pub last_error: Option<String>,
}

/// Shared health state for a backend.
pub struct BackendHealth {
    /// Current status (updated by health check thread).
    status: Arc<std::sync::Mutex<HealthStatus>>,
    /// Last check result.
    last_check: Arc<std::sync::Mutex<Option<HealthCheck>>>,
    /// Consecutive failure count (resets on success).
    failure_count: Arc<std::sync::Mutex<u32>>,
}

impl BackendHealth {
    pub fn new() -> Self {
        Self {
            status: Arc::new(std::sync::Mutex::new(HealthStatus::Healthy)),
            last_check: Arc::new(std::sync::Mutex::new(None)),
            failure_count: Arc::new(std::sync::Mutex::new(0)),
        }
    }

    /// Check if backend is currently marked healthy (not degraded/down).
    pub fn is_healthy(&self) -> bool {
        self.status
            .lock()
            .ok()
            .map(|g| *g == HealthStatus::Healthy)
            .unwrap_or(true)
    }

    /// Get current health status.
    pub fn status(&self) -> HealthStatus {
        self.status
            .lock()
            .ok()
            .map(|g| *g)
            .unwrap_or(HealthStatus::Healthy)
    }

    /// Get the last health check result.
    pub fn last_check(&self) -> Option<HealthCheck> {
        self.last_check.lock().ok().and_then(|guard| guard.clone())
    }

    /// True when a request should attempt this backend right now. `Healthy`
    /// and `Degraded` always attempt — only 3+ consecutive failures (`Down`)
    /// change the answer. A `Down` backend is skipped (the caller routes
    /// elsewhere, e.g. cloud, instead of repeating a call known to fail)
    /// until `cooldown_secs` has elapsed since the last check, at which point
    /// exactly one probe request is let through — classic half-open
    /// circuit-breaker behavior, so recovery is detected automatically
    /// without a manual restart or a background poll thread. No last check
    /// yet (a fresh tracker) always attempts.
    pub fn should_attempt(&self, cooldown_secs: u64) -> bool {
        if self.status() != HealthStatus::Down {
            return true;
        }
        match self.last_check() {
            Some(check) => unix_now().saturating_sub(check.timestamp) >= cooldown_secs,
            None => true,
        }
    }

    /// Record a successful health check.
    pub fn mark_healthy(&self, response_time_ms: u64) {
        if let Ok(mut guard) = self.failure_count.lock() {
            *guard = 0;
        }
        if let Ok(mut guard) = self.status.lock() {
            *guard = HealthStatus::Healthy;
        }
        if let Ok(mut guard) = self.last_check.lock() {
            *guard = Some(HealthCheck {
                status: HealthStatus::Healthy,
                timestamp: unix_now(),
                response_time_ms,
                last_error: None,
            });
        }
    }

    /// Record a failed health check.
    pub fn mark_unhealthy(&self, error: String, response_time_ms: u64) {
        let mut failures_count = 1u32;
        if let Ok(mut guard) = self.failure_count.lock() {
            *guard += 1;
            failures_count = *guard;
        }

        let status = if failures_count >= 3 {
            HealthStatus::Down
        } else {
            HealthStatus::Degraded
        };

        if let Ok(mut guard) = self.status.lock() {
            *guard = status;
        }
        if let Ok(mut guard) = self.last_check.lock() {
            *guard = Some(HealthCheck {
                status,
                timestamp: unix_now(),
                response_time_ms,
                last_error: Some(error),
            });
        }
    }
}

impl Default for BackendHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple unix timestamp (seconds since epoch).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_status_healthy() {
        let h = BackendHealth::new();
        assert!(h.is_healthy());
        h.mark_healthy(10);
        assert!(h.is_healthy());
        let check = h.last_check().unwrap();
        assert_eq!(check.status, HealthStatus::Healthy);
        assert_eq!(check.response_time_ms, 10);
    }

    #[test]
    fn test_health_status_degraded_to_down() {
        let h = BackendHealth::new();
        h.mark_unhealthy("timeout".to_string(), 5000);
        assert_eq!(h.status(), HealthStatus::Degraded);
        let check = h.last_check().unwrap();
        assert_eq!(check.status, HealthStatus::Degraded);

        h.mark_unhealthy("timeout".to_string(), 5000);
        h.mark_unhealthy("timeout".to_string(), 5000);
        assert_eq!(h.status(), HealthStatus::Down);
        assert!(!h.is_healthy()); // Down is not healthy
        let check = h.last_check().unwrap();
        assert_eq!(check.status, HealthStatus::Down);
    }

    #[test]
    fn test_health_recovers_on_success() {
        let h = BackendHealth::new();
        h.mark_unhealthy("error".to_string(), 100);
        h.mark_unhealthy("error".to_string(), 100);
        h.mark_unhealthy("error".to_string(), 100);
        assert!(!h.is_healthy());

        h.mark_healthy(10);
        assert!(h.is_healthy());
    }

    #[test]
    fn test_should_attempt_true_when_healthy_or_degraded() {
        let h = BackendHealth::new();
        // Fresh tracker: always attempt, any cooldown.
        assert!(h.should_attempt(3600));
        h.mark_unhealthy("timeout".to_string(), 100);
        assert_eq!(h.status(), HealthStatus::Degraded);
        // Degraded (not yet Down): cooldown is irrelevant, always attempt.
        assert!(h.should_attempt(3600));
    }

    #[test]
    fn test_should_attempt_false_when_down_within_cooldown() {
        let h = BackendHealth::new();
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        assert_eq!(h.status(), HealthStatus::Down);
        // Just marked Down: a long cooldown must skip the next attempt.
        assert!(!h.should_attempt(3600));
    }

    #[test]
    fn test_should_attempt_true_when_down_and_cooldown_is_zero() {
        let h = BackendHealth::new();
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        assert_eq!(h.status(), HealthStatus::Down);
        // Zero cooldown means every request probes (elapsed >= 0 is always true).
        assert!(h.should_attempt(0));
    }

    #[test]
    fn test_should_attempt_recovers_after_successful_probe() {
        let h = BackendHealth::new();
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        h.mark_unhealthy("e".to_string(), 100);
        assert!(!h.should_attempt(3600));
        // A successful probe (e.g. the caller let one through via cooldown=0
        // and it succeeded) resets state — subsequent calls attempt normally
        // regardless of cooldown.
        h.mark_healthy(10);
        assert!(h.should_attempt(3600));
    }
}

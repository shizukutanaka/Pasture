//! Routing decision audit logging (IMP-29).
//!
//! Records per-request routing decisions to an optional JSONL log for debugging
//! and analysis. Contains only decision metadata (signals, thresholds, route choice),
//! never prompt content or PII values—safe for long-term retention.

use crate::routing::Decision;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

/// A single routing decision log entry (PII-safe).
#[derive(Debug, Clone)]
pub struct DecisionEntry {
    /// Request ID (fingerprint or unique ID).
    pub request_id: String,
    /// Unix timestamp of the decision.
    pub timestamp: u64,
    /// Hard routing signals detected (e.g., "sensitive", "code", "tools").
    pub hard_signals: Vec<String>,
    /// Token count of the request.
    pub token_count: u64,
    /// Routing threshold applied.
    pub threshold: u64,
    /// Final route chosen (Local, Cloud, or LocalOnly).
    pub final_route: String,
    /// Human-readable reason (no PII).
    pub reason: String,
}

impl DecisionEntry {
    /// Serialize to JSON-compatible format for JSONL.
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"request_id":"{}","timestamp":{},"hard_signals":{},"token_count":{},"threshold":{},"final_route":"{}","reason":"{}"}}"#,
            escape_json_string(&self.request_id),
            self.timestamp,
            format_json_array(&self.hard_signals),
            self.token_count,
            self.threshold,
            escape_json_string(&self.final_route),
            escape_json_string(&self.reason)
        )
    }
}

/// Shared decision logger (thread-safe append).
pub struct DecisionLogger {
    /// Optional file handle (if logging is enabled).
    file: Option<Mutex<std::fs::File>>,
}

impl DecisionLogger {
    /// Create a new decision logger. If `log_path` is provided, opens it for appending.
    pub fn new(log_path: Option<&str>) -> std::io::Result<Self> {
        let file = if let Some(path) = log_path {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            Some(Mutex::new(f))
        } else {
            None
        };
        Ok(Self { file })
    }

    /// Log a routing decision if logging is enabled.
    pub fn log_decision(&self, decision: &Decision, token_count: u64, threshold: u64) {
        if let Some(ref file_mutex) = self.file {
            let entry = DecisionEntry {
                request_id: format_request_id(&decision.reason),
                timestamp: unix_now(),
                hard_signals: extract_signals(&decision.reason),
                token_count,
                threshold,
                final_route: decision.route.as_str().to_string(),
                reason: decision.reason.clone(),
            };

            if let Ok(mut file) = file_mutex.lock() {
                let json = entry.to_json();
                let _ = writeln!(file, "{}", json);
            }
        }
    }

    /// Check if logging is enabled.
    pub fn is_enabled(&self) -> bool {
        self.file.is_some()
    }
}

/// Simple Unix timestamp (seconds since epoch).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Extract signal keywords from reason string (naive parsing).
fn extract_signals(reason: &str) -> Vec<String> {
    let mut signals = Vec::new();
    let keywords = [
        "sensitive",
        "code",
        "tools",
        "math",
        "reason",
        "summarize",
        "translate",
        "hard_signal",
        "cascade",
        "local_only",
        "injection",
    ];
    for kw in &keywords {
        if reason.to_lowercase().contains(kw) {
            signals.push(kw.to_string());
        }
    }
    signals
}

/// Generate a request ID from the reason string (deterministic).
fn format_request_id(reason: &str) -> String {
    format!("req_{:08x}", hash_string(reason))
}

/// Simple FNV-1a hash for deterministic request IDs.
fn hash_string(s: &str) -> u32 {
    let mut hash = 2166136261u32;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

/// JSON-safe string escaping.
fn escape_json_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Format a vector of strings as a JSON array.
fn format_json_array(items: &[String]) -> String {
    let escaped: Vec<String> = items.iter().map(|s| format!("\"{}\"", escape_json_string(s))).collect();
    format!("[{}]", escaped.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_entry_to_json() {
        let entry = DecisionEntry {
            request_id: "req_12345678".to_string(),
            timestamp: 1234567890,
            hard_signals: vec!["code".to_string(), "math".to_string()],
            token_count: 500,
            threshold: 400,
            final_route: "cloud".to_string(),
            reason: "hard signal(s): code, math".to_string(),
        };

        let json = entry.to_json();
        assert!(json.contains("req_12345678"));
        assert!(json.contains("1234567890"));
        assert!(json.contains("\"code\""));
        assert!(json.contains("\"cloud\""));
    }

    #[test]
    fn test_escape_json_string() {
        assert_eq!(escape_json_string("hello"), "hello");
        assert_eq!(escape_json_string("hello\"world"), "hello\\\"world");
        assert_eq!(escape_json_string("a\\b"), "a\\\\b");
        assert_eq!(escape_json_string("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn test_extract_signals() {
        let reason = "hard signal(s): code, math, tools";
        let signals = extract_signals(reason);
        assert!(signals.contains(&"code".to_string()));
        assert!(signals.contains(&"math".to_string()));
        assert!(signals.contains(&"tools".to_string()));
    }

    #[test]
    fn test_format_request_id_deterministic() {
        let id1 = format_request_id("same input");
        let id2 = format_request_id("same input");
        assert_eq!(id1, id2);
    }
}

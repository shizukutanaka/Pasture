//! Optional OpenTelemetry-compatible GenAI trace log (IMP-23).
//!
//! Writes one JSONL span record per request using the OpenTelemetry GenAI
//! semantic conventions (gen_ai.* attributes, OTel SemConv 1.28+). Format is
//! compatible with the OTel Collector JSON file receiver and can be imported
//! into Jaeger, Tempo, and other OTel-native tools.
//!
//! Enabled via `PASTURE_OTEL_LOG=<path>`; completely off (zero overhead) when
//! unset. Span/trace IDs are generated without an external RNG by mixing
//! monotonic nanosecond time with a global atomic counter — unique within a
//! process lifetime (not cryptographically random, which is fine for trace
//! correlation). No PII is written: only token counts, model names, route, and
//! timing (I5).

use crate::json::escape_string;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SPAN_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A GenAI span with OTel semantic convention attributes.
/// Create with `Span::start()`, fill fields, then call `finish()` + `append_to()`.
#[derive(Debug)]
pub struct Span {
    /// 128-bit trace ID (32 hex chars) — OTel-compliant.
    pub trace_id: String,
    /// 64-bit span ID (16 hex chars) — OTel-compliant.
    pub span_id: String,
    pub start_time_unix_nano: u128,
    pub end_time_unix_nano: u128,
    /// `gen_ai.system`: e.g. `"openai"`, `"anthropic"`, `"ollama"`.
    pub system: String,
    /// `gen_ai.request.model`
    pub request_model: String,
    /// `gen_ai.response.model`
    pub response_model: String,
    /// `gen_ai.usage.input_tokens`
    pub input_tokens: u64,
    /// `gen_ai.usage.output_tokens`
    pub output_tokens: u64,
    /// Pasture extension: `pasture.route` — `"local"`, `"cloud"`, `"cache"`.
    pub route: &'static str,
    /// `gen_ai.response.finish_reason` (optional).
    pub finish_reason: Option<String>,
}

impl Span {
    /// Start a new span (stamps `start_time_unix_nano`).
    /// `system` is the `gen_ai.system` value, e.g. `"openai"`.
    pub fn start(system: &str, request_model: &str) -> Self {
        let now = now_nanos();
        let seq = SPAN_COUNTER.fetch_add(2, Ordering::Relaxed);
        Self {
            trace_id: gen_trace_id(now, seq),
            span_id: gen_span_id(now, seq + 1),
            start_time_unix_nano: now,
            end_time_unix_nano: now,
            system: system.to_string(),
            request_model: request_model.to_string(),
            response_model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            route: "local",
            finish_reason: None,
        }
    }

    /// Stamp `end_time_unix_nano` with the current time.
    pub fn finish(&mut self) {
        self.end_time_unix_nano = now_nanos();
    }

    /// Serialize to a single JSONL line (no trailing newline).
    /// All attributes use the GenAI semantic convention keys so any
    /// OTel-aware tool can understand them without a custom schema.
    pub fn to_jsonl(&self) -> String {
        let finish = match &self.finish_reason {
            Some(r) => format!(
                ",\"gen_ai.response.finish_reason\":\"{}\"",
                escape_string(r)
            ),
            None => String::new(),
        };
        format!(
            "{{\"name\":\"gen_ai.chat\",\"trace_id\":\"{}\",\"span_id\":\"{}\",\
             \"start_time_unix_nano\":{},\"end_time_unix_nano\":{},\"status\":\"ok\",\
             \"attributes\":{{\"gen_ai.system\":\"{}\",\"gen_ai.operation.name\":\"chat\",\
             \"gen_ai.request.model\":\"{}\",\"gen_ai.response.model\":\"{}\",\
             \"gen_ai.usage.input_tokens\":{},\"gen_ai.usage.output_tokens\":{},\
             \"pasture.route\":\"{}\"{}\
             }}}}",
            self.trace_id,
            self.span_id,
            self.start_time_unix_nano,
            self.end_time_unix_nano,
            escape_string(&self.system),
            escape_string(&self.request_model),
            escape_string(&self.response_model),
            self.input_tokens,
            self.output_tokens,
            escape_string(self.route),
            finish,
        )
    }

    /// Append this span to the given JSONL log file, creating it if needed.
    pub fn append_to(&self, path: &str) -> std::io::Result<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{}", self.to_jsonl())
    }
}

/// Current time as Unix nanoseconds (u128).
fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 128-bit trace ID (32 hex chars) from time + counter.
fn gen_trace_id(time_nanos: u128, counter: u64) -> String {
    let lo = mix64(time_nanos as u64, counter);
    let hi = mix64((time_nanos >> 64) as u64, counter.wrapping_add(1));
    format!("{hi:016x}{lo:016x}")
}

/// 64-bit span ID (16 hex chars) from time + counter.
fn gen_span_id(time_nanos: u128, counter: u64) -> String {
    let v = mix64(time_nanos as u64, counter);
    format!("{v:016x}")
}

/// Non-cryptographic 64-bit mixing function (finalizer from MurmurHash3).
fn mix64(a: u64, b: u64) -> u64 {
    let mut x = a ^ b ^ 0x9e3779b97f4a7c15;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_span_to_jsonl_is_valid_json() {
        let mut span = Span::start("openai", "gpt-4o-mini");
        span.response_model = "gpt-4o-mini".to_string();
        span.input_tokens = 50;
        span.output_tokens = 100;
        span.route = "cloud";
        span.finish();
        let line = span.to_jsonl();
        let v = crate::json::parse(&line);
        assert!(v.is_ok(), "span JSONL must be valid JSON: {line}");
        let v = v.unwrap();
        assert_eq!(
            v.get("name").and_then(|x| x.as_str()),
            Some("gen_ai.chat")
        );
    }

    #[test]
    fn test_span_attributes_present() {
        let mut span = Span::start("anthropic", "claude-3-5-sonnet");
        span.response_model = "claude-3-5-sonnet-20241022".to_string();
        span.input_tokens = 10;
        span.output_tokens = 5;
        span.route = "cloud";
        let line = span.to_jsonl();
        assert!(
            line.contains("\"gen_ai.system\":\"anthropic\""),
            "{line}"
        );
        assert!(line.contains("\"pasture.route\":\"cloud\""), "{line}");
        assert!(
            line.contains("\"gen_ai.usage.input_tokens\":10"),
            "{line}"
        );
        assert!(line.contains("\"status\":\"ok\""), "{line}");
    }

    #[test]
    fn test_span_ids_unique_across_calls() {
        let s1 = Span::start("openai", "m");
        let s2 = Span::start("openai", "m");
        assert_ne!(s1.span_id, s2.span_id, "span IDs must be unique");
        assert_ne!(s1.trace_id, s2.trace_id, "trace IDs must be unique");
    }

    #[test]
    fn test_span_id_is_16_hex_chars() {
        let s = Span::start("local", "llama3");
        assert_eq!(s.span_id.len(), 16, "span_id must be 16 hex chars");
        assert!(
            s.span_id.chars().all(|c| c.is_ascii_hexdigit()),
            "span_id must be hex"
        );
    }

    #[test]
    fn test_trace_id_is_32_hex_chars() {
        let s = Span::start("local", "llama3");
        assert_eq!(s.trace_id.len(), 32, "trace_id must be 32 hex chars");
        assert!(
            s.trace_id.chars().all(|c| c.is_ascii_hexdigit()),
            "trace_id must be hex"
        );
    }

    #[test]
    fn test_span_finish_reason_in_json() {
        let mut span = Span::start("openai", "m");
        span.finish_reason = Some("stop".to_string());
        span.finish();
        let line = span.to_jsonl();
        assert!(
            line.contains("\"gen_ai.response.finish_reason\":\"stop\""),
            "{line}"
        );
    }

    #[test]
    fn test_span_append_to_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pasture-otel-test-{}.jsonl", now_nanos()));
        let p = path.to_str().unwrap();
        let mut span = Span::start("openai", "m");
        span.finish();
        span.append_to(p).unwrap();
        let content = std::fs::read_to_string(p).unwrap();
        assert!(content.trim().ends_with('}'), "JSONL must end with }}");
        let _ = std::fs::remove_file(p);
    }
}

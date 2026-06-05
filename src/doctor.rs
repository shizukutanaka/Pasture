//! Environment diagnostics for first-time users. `pasture doctor` checks the
//! things beginners get stuck on (is Ollama running? is a model pulled? is the
//! port free?) and prints the exact command to fix each problem.
//!
//! The JSON parsing is pure and unit-tested; the TCP probe is a thin wrapper.

use crate::json::{parse, JsonValue};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Result of probing a local Ollama server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OllamaStatus {
    pub reachable: bool,
    pub models: Vec<String>,
}

/// Extract model names from an Ollama `/api/tags` response.
/// Shape: `{"models":[{"name":"llama3:latest",...}, ...]}`.
pub fn parse_model_names(tags_json: &str) -> Vec<String> {
    let Ok(v) = parse(tags_json) else {
        return Vec::new();
    };
    v.get("models")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// True if `want` matches one of the installed models, allowing for the
/// implicit `:latest` tag (so "llama3" matches "llama3:latest").
pub fn has_model(models: &[String], want: &str) -> bool {
    models.iter().any(|m| {
        m == want || m.split(':').next() == Some(want) || m.as_str() == format!("{want}:latest")
    })
}

/// Probe a local Ollama server for reachability and installed models.
pub fn probe_ollama(host: &str, port: u16) -> OllamaStatus {
    match tcp_get(host, port, "/api/tags", Duration::from_secs(2)) {
        Some(body) => OllamaStatus {
            reachable: true,
            models: parse_model_names(&body),
        },
        None => OllamaStatus {
            reachable: false,
            models: Vec::new(),
        },
    }
}

/// Split an OpenAI-style base URL into (host, port, base_path).
/// e.g. "http://127.0.0.1:1234/v1" -> ("127.0.0.1", 1234, "/v1").
/// Missing port defaults to 1234 (LM Studio's default).
pub fn parse_base_url(url: &str) -> (String, u16, String) {
    let no_scheme = url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (authority, path) = match no_scheme.find('/') {
        Some(i) => (&no_scheme[..i], &no_scheme[i..]),
        None => (no_scheme, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(1234)),
        None => (authority.to_string(), 1234),
    };
    let base_path = path.trim_end_matches('/').to_string();
    (host, port, base_path)
}

/// Extract model ids from an OpenAI `/v1/models` response.
/// Shape: `{"data":[{"id":"..."}, ...]}`.
pub fn parse_openai_model_names(models_json: &str) -> Vec<String> {
    let Ok(v) = parse(models_json) else {
        return Vec::new();
    };
    v.get("data")
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Probe a local OpenAI-compatible server (LM Studio, etc.) via `/models`.
/// `base_path` is the URL base such as "/v1".
pub fn probe_openai(host: &str, port: u16, base_path: &str) -> OllamaStatus {
    let path = format!("{base_path}/models");
    match tcp_get(host, port, &path, Duration::from_secs(2)) {
        Some(body) => OllamaStatus {
            reachable: true,
            models: parse_openai_model_names(&body),
        },
        None => OllamaStatus {
            reachable: false,
            models: Vec::new(),
        },
    }
}

/// True if a TCP listen address can be bound (i.e. the port is free).
pub fn port_available(addr: &str) -> bool {
    TcpListener::bind(addr).is_ok()
}

/// Minimal plain-HTTP GET returning the response body, or None on any failure.
fn tcp_get(host: &str, port: u16, path: &str, timeout: Duration) -> Option<String> {
    let mut stream = TcpStream::connect((host, port)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nAccept: */*\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_model_names() {
        let json = r#"{"models":[{"name":"llama3:latest"},{"name":"qwen2:7b"}]}"#;
        assert_eq!(parse_model_names(json), vec!["llama3:latest", "qwen2:7b"]);
    }

    #[test]
    fn test_parse_model_names_empty_or_bad() {
        assert!(parse_model_names("{}").is_empty());
        assert!(parse_model_names("not json").is_empty());
        assert!(parse_model_names(r#"{"models":[]}"#).is_empty());
    }

    #[test]
    fn test_has_model_exact_and_latest() {
        let models = vec!["llama3:latest".to_string(), "qwen2:7b".to_string()];
        assert!(has_model(&models, "llama3")); // implicit :latest
        assert!(has_model(&models, "llama3:latest"));
        assert!(has_model(&models, "qwen2")); // prefix before ':'
        assert!(!has_model(&models, "mistral"));
    }

    #[test]
    fn test_port_available_roundtrip() {
        // Bind an ephemeral port, then it should report unavailable while held.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(!port_available(&addr));
        drop(listener);
        assert!(port_available(&addr));
    }

    #[test]
    fn test_probe_ollama_unreachable() {
        // Port 1 is reserved and not listening; probe must report unreachable.
        let st = probe_ollama("127.0.0.1", 1);
        assert!(!st.reachable);
        assert!(st.models.is_empty());
    }

    #[test]
    fn test_parse_base_url() {
        assert_eq!(
            parse_base_url("http://127.0.0.1:1234/v1"),
            ("127.0.0.1".to_string(), 1234, "/v1".to_string())
        );
        assert_eq!(
            parse_base_url("https://host:8080/api/v1/"),
            ("host".to_string(), 8080, "/api/v1".to_string())
        );
        assert_eq!(
            parse_base_url("localhost/v1"),
            ("localhost".to_string(), 1234, "/v1".to_string())
        );
    }

    #[test]
    fn test_parse_openai_model_names() {
        let json = r#"{"data":[{"id":"llama-3.2-3b-instruct"},{"id":"qwen2.5-7b"}]}"#;
        assert_eq!(
            parse_openai_model_names(json),
            vec!["llama-3.2-3b-instruct", "qwen2.5-7b"]
        );
        assert!(parse_openai_model_names("{}").is_empty());
    }
}

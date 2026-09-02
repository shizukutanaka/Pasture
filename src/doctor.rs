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

/// Extract one string field from each object of a top-level JSON array,
/// e.g. `{"models":[{"name":"x"},…]}` with keys ("models","name") → `["x"]`.
/// Malformed JSON or a missing array yields `[]`. Shared by the Ollama
/// `/api/tags` and OpenAI `/v1/models` response shapes.
fn extract_string_field(json: &str, array_key: &str, field_key: &str) -> Vec<String> {
    let Ok(v) = parse(json) else {
        return Vec::new();
    };
    v.get(array_key)
        .and_then(JsonValue::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get(field_key).and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Extract model names from an Ollama `/api/tags` response.
/// Shape: `{"models":[{"name":"llama3:latest",...}, ...]}`.
pub fn parse_model_names(tags_json: &str) -> Vec<String> {
    extract_string_field(tags_json, "models", "name")
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

/// The first Ollama release that reports per-token logprobs on `/api/chat`
/// (v0.12.11, published 2025-11-12, PR #12899). Below this, `"logprobs":true`
/// is silently ignored and the cascade falls back to its text heuristic.
pub const OLLAMA_LOGPROBS_MIN: (u64, u64, u64) = (0, 12, 11);

/// Ask a running Ollama for its version via `GET /api/version`
/// (`{"version":"0.12.11"}`). `None` when unreachable or unparsable.
pub fn probe_ollama_version(host: &str, port: u16) -> Option<String> {
    tcp_get(host, port, "/api/version", Duration::from_secs(2)).and_then(|b| parse_version(&b))
}

/// Extract the `version` string from an `/api/version` body.
pub fn parse_version(body: &str) -> Option<String> {
    let v = crate::json::parse(body).ok()?;
    v.get("version")?.as_str().map(|s| s.trim().to_string())
}

/// Parse `"0.12.11"`, `"v0.12.11"`, or `"0.13.0-rc2"` into (major, minor,
/// patch). Pre-release suffixes are dropped: an rc of 0.12.11 already carries
/// the feature. Anything that does not start with three numbers is `None`.
pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Whether an Ollama version string reports logprobs. `None` when the version
/// cannot be parsed - say "unknown", never guess.
pub fn ollama_supports_logprobs(version: &str) -> Option<bool> {
    parse_semver(version).map(|v| v >= OLLAMA_LOGPROBS_MIN)
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
    extract_string_field(models_json, "data", "id")
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

/// True when `name` is an executable on `PATH` (ADR-265).
///
/// Lets `doctor` tell "Ollama isn't installed" from "Ollama isn't running" —
/// previously one TCP probe produced a single message carrying both fixes, so a
/// new user had to guess which applied to them.
pub fn on_path(name: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    // Windows needs the extension; checking the bare name too is harmless.
    let candidates = [name.to_string(), format!("{name}.exe")];
    std::env::split_paths(&path).any(|dir| {
        candidates.iter().any(|c| {
            std::fs::metadata(dir.join(c))
                .map(|m| m.is_file())
                .unwrap_or(false)
        })
    })
}

/// Why the cost log cannot be written, or `None` when it can (ADR-265).
///
/// Accounting is one of Pasture's four jobs, and it failed *silently*: the only
/// signal was one stderr line per request that scrolls off a running server.
/// Worse, `read_log` maps `NotFound` to "no records", and a missing *directory*
/// is also `NotFound` — so an unwritable path was indistinguishable from
/// "no data yet", and `stats` reported it as such forever.
///
/// Probes without creating the log itself: an existing file must be openable
/// for append; otherwise the parent directory must exist and accept a temp file,
/// which is removed again.
pub fn cost_log_problem(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    if p.exists() {
        return match std::fs::OpenOptions::new().append(true).open(p) {
            Ok(_) => None,
            Err(e) => Some(format!("cannot append to {path}: {e}")),
        };
    }
    // Empty parent means a bare filename — the current directory.
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => std::path::Path::new("."),
    };
    if !dir.exists() {
        return Some(format!("directory does not exist: {}", dir.display()));
    }
    let probe = dir.join(".pasture-write-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(e) => Some(format!("directory is not writable: {}: {e}", dir.display())),
    }
}

/// Minimal plain-HTTP GET returning the response body on 200 OK, or None on
/// any failure (connection error, non-200 status). A non-200 response means
/// the server exists but is not a recognized Ollama/OpenAI-compat endpoint;
/// treating it as "reachable" would produce a false-positive doctor report
/// ("running but no models") for proxies or other services on the same port.
fn tcp_get(host: &str, port: u16, path: &str, timeout: Duration) -> Option<String> {
    // RFC 3986: an IPv6 address in a URL authority is bracketed, e.g. `[::1]`.
    // TcpStream::connect((&str, u16)) expects a bare address (`::1`), not the
    // bracketed form. Strip the brackets before connecting.
    let connect_host = host.trim_matches(|c| c == '[' || c == ']');
    let mut stream = TcpStream::connect((connect_host, port)).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    // RFC 7230 §5.4: the Host header must include the port for non-standard
    // ports. Keep the original `host` (with brackets if IPv6) for the header.
    let host_header = format!("{host}:{port}");
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .unwrap_or(0);
    if status == 200 {
        Some(body.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_cost_log_problem_detects_missing_directory() {
        // ADR-265: the case that used to be reported as "no cost log yet".
        let why = cost_log_problem("/nonexistent-dir-xyz/pasture.jsonl");
        assert!(why.is_some(), "missing directory must be a problem");
        assert!(
            why.unwrap().contains("does not exist"),
            "message must name the cause"
        );
    }

    #[test]
    fn test_cost_log_problem_none_for_writable_path() {
        // A writable directory is fine, and probing must NOT leave the log
        // behind (doctor is a diagnostic, not a side effect).
        let dir = std::env::temp_dir().join("pasture_costlog_probe_test");
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join("cost.jsonl");
        assert_eq!(cost_log_problem(&target.to_string_lossy()), None);
        assert!(!target.exists(), "probe must not create the cost log");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cost_log_problem_none_for_existing_writable_file() {
        let path = std::env::temp_dir().join("pasture_costlog_existing.jsonl");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(cost_log_problem(&path.to_string_lossy()), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_on_path_finds_real_binary_and_rejects_nonsense() {
        // `sh` exists on every platform this is built for; the other cannot.
        assert!(on_path("sh"), "sh must be found on PATH");
        assert!(!on_path("definitely-not-a-real-binary-xyzzy"));
    }
    use super::*;

    #[test]
    fn test_parse_version_reads_ollama_shape() {
        // routes.go: r.GET("/api/version", ...) -> {"version": version.Version}
        assert_eq!(
            parse_version(r#"{"version":"0.12.11"}"#).as_deref(),
            Some("0.12.11")
        );
        assert_eq!(
            parse_version(r#"{"version":" v0.13.0-rc2 "}"#).as_deref(),
            Some("v0.13.0-rc2")
        );
        assert_eq!(parse_version("{}"), None);
        assert_eq!(parse_version("not json"), None);
    }

    #[test]
    fn test_parse_semver_tolerates_prefix_and_prerelease() {
        assert_eq!(parse_semver("0.12.11"), Some((0, 12, 11)));
        assert_eq!(parse_semver("v0.12.11"), Some((0, 12, 11)));
        assert_eq!(parse_semver("0.12.11-rc0"), Some((0, 12, 11)));
        assert_eq!(parse_semver("0.33.2+build.7"), Some((0, 33, 2)));
        assert_eq!(parse_semver("0.12"), None);
        assert_eq!(parse_semver("dev"), None);
        assert_eq!(parse_semver(""), None);
    }

    #[test]
    fn test_ollama_supports_logprobs_boundary() {
        // v0.12.11 (2025-11-12) is the first release with PR #12899.
        assert_eq!(ollama_supports_logprobs("0.12.10"), Some(false));
        assert_eq!(ollama_supports_logprobs("0.12.11"), Some(true));
        assert_eq!(ollama_supports_logprobs("0.12.11-rc0"), Some(true));
        assert_eq!(ollama_supports_logprobs("0.13.0"), Some(true));
        assert_eq!(ollama_supports_logprobs("1.0.0"), Some(true));
        assert_eq!(ollama_supports_logprobs("0.9.99"), Some(false));
        assert_eq!(ollama_supports_logprobs("garbage"), None);
    }

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
    fn test_parse_base_url_ipv6() {
        // ADR-106: IPv6 addresses in URL authority are bracketed per RFC 3986.
        // parse_base_url must preserve the brackets in the host field so tcp_get
        // can use them for the Host header while stripping them for TcpStream::connect.
        let (host, port, path) = parse_base_url("http://[::1]:8080/v1");
        assert_eq!(host, "[::1]");
        assert_eq!(port, 8080);
        assert_eq!(path, "/v1");
        // tcp_get's bracket-strip logic: connect_host must be bare for ToSocketAddrs.
        let connect_host = host.trim_matches(|c| c == '[' || c == ']');
        assert_eq!(connect_host, "::1");
        // Host header must include port (non-standard) and keep IPv6 brackets.
        let host_header = format!("{host}:{port}");
        assert_eq!(host_header, "[::1]:8080");
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

    #[test]
    fn test_probe_ollama_non_200_is_unreachable() {
        // A server that exists but returns 404 must not be reported as reachable.
        // Pre-existing behaviour: tcp_get returned the body regardless of status,
        // so `probe_ollama` set reachable:true even for 404/500 responses (a proxy
        // or wrong service on the port would appear as "Ollama running, no models").
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}\n");
            }
        });
        let st = probe_ollama("127.0.0.1", addr.port());
        assert!(
            !st.reachable,
            "404 response must not be treated as reachable"
        );
        assert!(st.models.is_empty());
    }
}

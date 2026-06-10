//! Cloud backends (ADR-005). The request/response shaping is pure and always
//! compiled & tested. The HTTPS transport is gated behind the `cloud` feature
//! because TLS requires a dependency (native-tls / system OpenSSL), which the
//! default zero-dependency build deliberately omits.
//!
//! BYOK: API keys come from environment variables and are never logged (I5):
//!   OpenAI    -> PASTURE_OPENAI_API_KEY
//!   Anthropic -> PASTURE_ANTHROPIC_API_KEY

use crate::backend::{BackendError, CompletionRequest};
use crate::json::{escape_string, parse, JsonValue};

/// Supported cloud providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
}

impl Provider {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "openai" => Some(Provider::OpenAI),
            "anthropic" | "claude" => Some(Provider::Anthropic),
            _ => None,
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Provider::OpenAI => "api.openai.com",
            Provider::Anthropic => "api.anthropic.com",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Provider::OpenAI => "/v1/chat/completions",
            Provider::Anthropic => "/v1/messages",
        }
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Provider::OpenAI => "PASTURE_OPENAI_API_KEY",
            Provider::Anthropic => "PASTURE_ANTHROPIC_API_KEY",
        }
    }

    /// Build the JSON request body for this provider.
    pub fn build_body(self, req: &CompletionRequest) -> String {
        let msgs: Vec<String> = req
            .messages
            .iter()
            .map(|m| {
                format!(
                    "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                    escape_string(&m.role),
                    escape_string(&m.content)
                )
            })
            .collect();
        match self {
            Provider::OpenAI => format!(
                "{{\"model\":\"{}\",\"messages\":[{}]{}}}",
                escape_string(&req.model),
                msgs.join(","),
                req.sampling.openai_fields()
            ),
            Provider::Anthropic => {
                // Anthropic requires max_tokens; honour the client's value (else
                // keep the prior 1024 default). temperature/top_p/stop_sequences
                // map across; OpenAI-only penalties are not supported here.
                let max_tokens = req.sampling.max_tokens.unwrap_or(1024);
                let mut extra = String::new();
                if let Some(t) = req.sampling.temperature {
                    extra.push_str(&format!(",\"temperature\":{t}"));
                }
                if let Some(p) = req.sampling.top_p {
                    extra.push_str(&format!(",\"top_p\":{p}"));
                }
                if !req.sampling.stop.is_empty() {
                    let items: Vec<String> = req
                        .sampling
                        .stop
                        .iter()
                        .map(|s| format!("\"{}\"", escape_string(s)))
                        .collect();
                    extra.push_str(&format!(",\"stop_sequences\":[{}]", items.join(",")));
                }
                format!(
                    "{{\"model\":\"{}\",\"max_tokens\":{max_tokens},\"messages\":[{}]{}}}",
                    escape_string(&req.model),
                    msgs.join(","),
                    extra
                )
            }
        }
    }

    /// Like `build_body` but with `"stream": true` for SSE streaming.
    pub fn build_body_stream(self, req: &CompletionRequest) -> String {
        let mut s = self.build_body(req);
        // Insert the stream flag before the closing brace.
        debug_assert!(s.ends_with('}'));
        s.pop();
        s.push_str(",\"stream\":true}");
        s
    }

    /// Like `build_body` but requesting per-token log-probabilities (OpenAI
    /// `logprobs`), used to score local-answer confidence for the cascade.
    pub fn build_body_logprobs(self, req: &CompletionRequest) -> String {
        let mut s = self.build_body(req);
        debug_assert!(s.ends_with('}'));
        s.pop();
        s.push_str(",\"logprobs\":true}");
        s
    }

    /// HTTP headers (name, value) for an authenticated request.
    pub fn headers(self, api_key: &str) -> Vec<(String, String)> {
        match self {
            Provider::OpenAI => vec![("Authorization".into(), format!("Bearer {api_key}"))],
            Provider::Anthropic => vec![
                ("x-api-key".into(), api_key.to_string()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ],
        }
    }

    /// Extract (content, prompt_tokens, completion_tokens) from a success body.
    pub fn parse_response(self, body: &str) -> Result<(String, u64, u64), BackendError> {
        let v = parse(body).map_err(|e| BackendError::Protocol(e.to_string()))?;
        // Surface server-reported errors (e.g. LM Studio returns an error object
        // with a 500 when the model id does not match a loaded model).
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err.as_str())
                .unwrap_or("unknown error");
            return Err(BackendError::Protocol(format!("server error: {msg}")));
        }
        match self {
            Provider::OpenAI => {
                let content = v
                    .get("choices")
                    .and_then(JsonValue::as_array)
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| {
                        BackendError::Protocol("missing choices[0].message.content".into())
                    })?
                    .to_string();
                let (p, c) = usage(&v, "prompt_tokens", "completion_tokens");
                Ok((content, p, c))
            }
            Provider::Anthropic => {
                let content = v
                    .get("content")
                    .and_then(JsonValue::as_array)
                    .and_then(|a| a.first())
                    .and_then(|b| b.get("text"))
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| BackendError::Protocol("missing content[0].text".into()))?
                    .to_string();
                let (p, c) = usage(&v, "input_tokens", "output_tokens");
                Ok((content, p, c))
            }
        }
    }
}

/// A parsed SSE line from an OpenAI-style streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiStreamEvent {
    Delta(String),
    Done,
}

/// Mean per-token log-probability from an OpenAI chat-completions response
/// (`choices[0].logprobs.content[].logprob`). `None` if absent or empty.
/// A higher (closer to 0) value means the model was more confident.
pub fn mean_logprob_from_openai(body: &str) -> Option<f64> {
    let v = parse(body).ok()?;
    let tokens = v
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("logprobs"))
        .and_then(|l| l.get("content"))
        .and_then(JsonValue::as_array)?;
    let lps: Vec<f64> = tokens
        .iter()
        .filter_map(|t| t.get("logprob").and_then(|x| x.as_f64()))
        .collect();
    if lps.is_empty() {
        return None;
    }
    Some(lps.iter().sum::<f64>() / lps.len() as f64)
}

/// Parse one SSE `data:` line from an OpenAI-compatible streaming response.
pub fn parse_openai_stream_line(line: &str) -> Option<OpenAiStreamEvent> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(OpenAiStreamEvent::Done);
    }
    let v = parse(data).ok()?;
    let content = v
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str());
    match content {
        Some(c) if !c.is_empty() => Some(OpenAiStreamEvent::Delta(c.to_string())),
        _ => None,
    }
}

/// Parse one SSE `data:` line from an Anthropic Messages streaming response.
/// Text arrives as `content_block_delta` events; `message_stop` signals the end.
pub fn parse_anthropic_stream_line(line: &str) -> Option<OpenAiStreamEvent> {
    let data = line.trim().strip_prefix("data:")?.trim();
    let v = parse(data).ok()?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("content_block_delta") => v
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| OpenAiStreamEvent::Delta(s.to_string())),
        Some("message_stop") => Some(OpenAiStreamEvent::Done),
        _ => None,
    }
}

impl Provider {
    /// Parse an SSE line in this provider's streaming dialect.
    pub fn parse_stream_line(self, line: &str) -> Option<OpenAiStreamEvent> {
        match self {
            Provider::OpenAI => parse_openai_stream_line(line),
            Provider::Anthropic => parse_anthropic_stream_line(line),
        }
    }
}

/// Map a non-2xx HTTP status to a backend error. A 5xx is a server-side,
/// typically transient failure → `Transport` (retryable, IMP-9); other codes
/// (e.g. 4xx auth/bad-request) are not retryable → `Protocol`.
pub fn http_status_error(status: u16, message: String) -> BackendError {
    if (500..600).contains(&status) {
        BackendError::Transport(message)
    } else {
        BackendError::Protocol(message)
    }
}

/// Read an HTTP SSE response from `reader`, invoking `on_delta` for each text
/// delta as it arrives, and return the assembled content. Provider-agnostic and
/// transport-agnostic (works over any `Read`, so the same logic is unit-tested
/// offline and reused over the real TLS stream). Chunked-transfer size lines are
/// ignored because they never start with `data:`; this assumes one SSE event per
/// chunk, which OpenAI and Anthropic both honour.
pub fn read_sse_body<R: std::io::Read>(
    reader: &mut R,
    provider: Provider,
    on_delta: &mut dyn FnMut(&str),
) -> Result<String, BackendError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until the headers terminator.
    let header_end = loop {
        if let Some(p) = find_crlf2(&buf) {
            break p;
        }
        let n = reader
            .read(&mut chunk)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if n == 0 {
            return Err(BackendError::Protocol("stream ended before headers".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65_536 {
            return Err(BackendError::Protocol("oversized response headers".into()));
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let status = header_text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let pending = buf[header_end + 4..].to_vec();
    if !(200..300).contains(&status) {
        let mut rest = pending;
        let _ = reader.read_to_end(&mut rest);
        let snippet: String = String::from_utf8_lossy(&rest).chars().take(200).collect();
        return Err(http_status_error(
            status,
            format!("HTTP {status}: {snippet}"),
        ));
    }

    let mut content = String::new();
    let mut line_buf: Vec<u8> = Vec::new();
    emit_sse_lines(provider, &mut line_buf, &pending, &mut content, on_delta);
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        emit_sse_lines(provider, &mut line_buf, &chunk[..n], &mut content, on_delta);
    }
    if content.is_empty() {
        return Err(BackendError::Protocol("empty stream".into()));
    }
    Ok(content)
}

fn emit_sse_lines(
    provider: Provider,
    line_buf: &mut Vec<u8>,
    incoming: &[u8],
    content: &mut String,
    on_delta: &mut dyn FnMut(&str),
) {
    line_buf.extend_from_slice(incoming);
    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = line_buf.drain(..=pos).collect();
        let s = String::from_utf8_lossy(&line);
        if let Some(OpenAiStreamEvent::Delta(d)) = provider.parse_stream_line(s.trim()) {
            content.push_str(&d);
            on_delta(&d);
        }
    }
}

fn find_crlf2(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn usage(v: &JsonValue, pk: &str, ck: &str) -> (u64, u64) {
    let u = v.get("usage");
    let get = |key: &str| {
        u.and_then(|x| x.get(key))
            .and_then(|n| match n {
                JsonValue::Number(f) => Some(*f as u64),
                _ => None,
            })
            .unwrap_or(0)
    };
    (get(pk), get(ck))
}

/// Parse a raw HTTP response into (status, body), handling Content-Length and
/// chunked transfer encoding. Pure and unit-tested.
pub fn parse_http_response(raw: &str) -> Result<(u16, String), BackendError> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| BackendError::Protocol("no header separator".into()))?;
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| BackendError::Protocol("bad status line".into()))?;
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_string()
    };
    Ok((status, body))
}

/// Decode a chunked transfer-encoding body (RFC 7230 §4.1).
/// Returns Err if a declared chunk size exceeds available bytes — that means
/// the response was truncated in transit and the data would be silently wrong.
fn dechunk(body: &str) -> Result<String, BackendError> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size_line, after)) = rest.split_once("\r\n") else {
            break;
        };
        let size =
            usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0").trim(), 16)
                .unwrap_or(0);
        if size == 0 {
            break;
        }
        // ADR-107: truncated chunk → protocol error, not silent data loss.
        if after.len() < size {
            return Err(BackendError::Protocol(format!(
                "chunked body truncated: declared {size} bytes, got {}",
                after.len()
            )));
        }
        out.push_str(&after[..size]);
        // Skip the chunk data and its trailing CRLF.
        rest = after.get(size + 2..).unwrap_or("");
    }
    Ok(out)
}

/// Resolve the API key for a provider from the environment. The value is
/// trimmed so keys set via `export KEY=$(cat file)` or shell substitution
/// (which often append a trailing newline) still work correctly.
pub fn api_key_from_env(provider: Provider) -> Option<String> {
    std::env::var(provider.env_key())
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

#[cfg(feature = "cloud")]
pub use transport::HttpsCloudBackend;

#[cfg(feature = "cloud")]
mod transport {
    use super::*;
    use crate::backend::{Backend, CompletionResponse};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Cloud backend that performs real HTTPS requests (feature = "cloud").
    pub struct HttpsCloudBackend {
        provider: Provider,
        api_key: String,
        model: String,
    }

    impl HttpsCloudBackend {
        pub fn new(provider: Provider, api_key: &str, model: &str) -> Self {
            Self {
                provider,
                api_key: api_key.to_string(),
                model: model.to_string(),
            }
        }

        /// Build from environment; returns None if the API key is unset.
        pub fn from_env(provider: Provider, model: &str) -> Option<Self> {
            api_key_from_env(provider).map(|k| Self::new(provider, &k, model))
        }
    }

    impl Backend for HttpsCloudBackend {
        fn name(&self) -> &str {
            "cloud"
        }

        fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
            let host = self.provider.host();
            let path = self.provider.path();
            let body = self.provider.build_body(req);
            let mut header_lines = String::new();
            for (k, v) in self.provider.headers(&self.api_key) {
                header_lines.push_str(&format!("{k}: {v}\r\n"));
            }
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{header_lines}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let raw = https_exchange(host, request.as_bytes())?;
            let (status, resp_body) = parse_http_response(&raw)?;
            if !(200..300).contains(&status) {
                let snippet: String = resp_body.chars().take(200).collect();
                return Err(http_status_error(
                    status,
                    format!("HTTP {status}: {snippet}"),
                ));
            }
            let (content, prompt_tokens, completion_tokens) =
                self.provider.parse_response(&resp_body)?;
            Ok(CompletionResponse {
                content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
            })
        }

        fn stream_complete(
            &self,
            req: &CompletionRequest,
            on_delta: &mut dyn FnMut(&str),
        ) -> Result<CompletionResponse, BackendError> {
            let host = self.provider.host();
            let path = self.provider.path();
            let body = self.provider.build_body_stream(req);
            let mut header_lines = String::new();
            for (k, v) in self.provider.headers(&self.api_key) {
                header_lines.push_str(&format!("{k}: {v}\r\n"));
            }
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{header_lines}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let connector = native_tls::TlsConnector::new()
                .map_err(|e| BackendError::Transport(format!("tls init: {e}")))?;
            let tcp = TcpStream::connect((host, 443))
                .map_err(|e| BackendError::Transport(format!("connect {host}: {e}")))?;
            let mut stream = connector
                .connect(host, tcp)
                .map_err(|e| BackendError::Transport(format!("tls handshake: {e}")))?;
            stream
                .write_all(request.as_bytes())
                .map_err(|e| BackendError::Transport(e.to_string()))?;
            let content = read_sse_body(&mut stream, self.provider, on_delta)?;
            let prompt_tokens = crate::routing::estimate_tokens(&req.routing_text()) as u64;
            let completion_tokens = crate::routing::estimate_tokens(&content) as u64;
            Ok(CompletionResponse {
                content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
            })
        }
    }

    /// Perform an HTTPS request over TLS, returning the raw response text.
    fn https_exchange(host: &str, request: &[u8]) -> Result<String, BackendError> {
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| BackendError::Transport(format!("tls init: {e}")))?;
        let tcp = TcpStream::connect((host, 443))
            .map_err(|e| BackendError::Transport(format!("connect {host}: {e}")))?;
        let mut stream = connector
            .connect(host, tcp)
            .map_err(|e| BackendError::Transport(format!("tls handshake: {e}")))?;
        stream
            .write_all(request)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            has_tools: false,
            sampling: Default::default(),
        }
    }

    #[test]
    fn test_provider_from_str() {
        assert_eq!(Provider::from_name("openai"), Some(Provider::OpenAI));
        assert_eq!(Provider::from_name("claude"), Some(Provider::Anthropic));
        assert_eq!(Provider::from_name("bogus"), None);
    }

    #[test]
    fn test_openai_body_and_headers() {
        let b = Provider::OpenAI.build_body(&req());
        assert!(b.contains("\"model\":\"test-model\""));
        assert!(b.contains("\"role\":\"user\""));
        let h = Provider::OpenAI.headers("KEY");
        assert_eq!(h[0].0, "Authorization");
        assert_eq!(h[0].1, "Bearer KEY");
    }

    #[test]
    fn test_anthropic_body_and_headers() {
        let b = Provider::Anthropic.build_body(&req());
        assert!(b.contains("\"max_tokens\""));
        let h = Provider::Anthropic.headers("KEY");
        assert!(h.iter().any(|(k, v)| k == "x-api-key" && v == "KEY"));
        assert!(h.iter().any(|(k, _)| k == "anthropic-version"));
    }

    #[test]
    fn test_parse_openai_response() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#;
        let (c, p, comp) = Provider::OpenAI.parse_response(body).unwrap();
        assert_eq!(c, "hello");
        assert_eq!((p, comp), (5, 3));
    }

    #[test]
    fn test_parse_anthropic_response() {
        let body = r#"{"content":[{"type":"text","text":"hi there"}],"usage":{"input_tokens":7,"output_tokens":2}}"#;
        let (c, p, comp) = Provider::Anthropic.parse_response(body).unwrap();
        assert_eq!(c, "hi there");
        assert_eq!((p, comp), (7, 2));
    }

    #[test]
    fn test_parse_response_missing_content_errors() {
        assert!(Provider::OpenAI
            .parse_response(r#"{"choices":[]}"#)
            .is_err());
    }

    #[test]
    fn test_parse_response_surfaces_server_error() {
        let body =
            r#"{"error":{"message":"model 'foo' not found","type":"invalid_request_error"}}"#;
        let err = Provider::OpenAI.parse_response(body).unwrap_err();
        assert!(err.to_string().contains("model 'foo' not found"), "{err}");
    }

    #[test]
    fn test_parse_response_surfaces_string_error() {
        let body = r#"{"error":"bad request"}"#;
        let err = Provider::Anthropic.parse_response(body).unwrap_err();
        assert!(err.to_string().contains("bad request"), "{err}");
    }

    #[test]
    fn test_build_body_logprobs_sets_flag() {
        let b = Provider::OpenAI.build_body_logprobs(&req());
        assert!(b.contains("\"logprobs\":true"), "{b}");
        assert!(crate::json::parse(&b).is_ok());
    }

    #[test]
    fn test_mean_logprob_from_openai() {
        let body = r#"{"choices":[{"logprobs":{"content":[{"logprob":-0.1},{"logprob":-0.3}]}}]}"#;
        let m = mean_logprob_from_openai(body).unwrap();
        assert!((m - (-0.2)).abs() < 1e-9, "{m}");
        // Absent logprobs -> None.
        assert_eq!(
            mean_logprob_from_openai(r#"{"choices":[{"message":{"content":"hi"}}]}"#),
            None
        );
    }

    #[test]
    fn test_build_body_stream_sets_flag() {
        let b = Provider::OpenAI.build_body_stream(&req());
        assert!(b.contains("\"stream\":true"), "{b}");
        assert!(b.contains("\"model\":\"test-model\""));
        // Still valid JSON.
        assert!(crate::json::parse(&b).is_ok());
    }

    #[test]
    fn test_openai_body_threads_sampling() {
        let mut r = req();
        r.sampling = crate::backend::SamplingParams {
            temperature: Some(0.0),
            max_tokens: Some(256),
            ..Default::default()
        };
        let b = Provider::OpenAI.build_body(&r);
        assert!(b.contains("\"temperature\":0"), "{b}");
        assert!(b.contains("\"max_tokens\":256"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "{b}");
    }

    #[test]
    fn test_anthropic_body_honors_max_tokens_override() {
        let mut r = req();
        r.sampling = crate::backend::SamplingParams {
            max_tokens: Some(50),
            temperature: Some(0.3),
            stop: vec!["END".to_string()],
            ..Default::default()
        };
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"max_tokens\":50"), "{b}");
        assert!(!b.contains("\"max_tokens\":1024"), "{b}");
        assert!(b.contains("\"temperature\":0.3"), "{b}");
        // Anthropic uses stop_sequences, not stop.
        assert!(b.contains("\"stop_sequences\":[\"END\"]"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "{b}");
    }

    #[test]
    fn test_anthropic_body_default_max_tokens_when_absent() {
        let b = Provider::Anthropic.build_body(&req());
        assert!(b.contains("\"max_tokens\":1024"), "{b}");
    }

    #[test]
    fn test_parse_openai_stream_line() {
        let d = r#"data: {"choices":[{"delta":{"content":"He"}}]}"#;
        assert_eq!(
            parse_openai_stream_line(d),
            Some(OpenAiStreamEvent::Delta("He".to_string()))
        );
        assert_eq!(
            parse_openai_stream_line("data: [DONE]"),
            Some(OpenAiStreamEvent::Done)
        );
        assert_eq!(parse_openai_stream_line(""), None);
        assert_eq!(
            parse_openai_stream_line(r#"data: {"choices":[{"delta":{}}]}"#),
            None
        );
    }

    #[test]
    fn test_parse_anthropic_stream_line() {
        let d = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        assert_eq!(
            parse_anthropic_stream_line(d),
            Some(OpenAiStreamEvent::Delta("Hi".to_string()))
        );
        assert_eq!(
            parse_anthropic_stream_line(r#"data: {"type":"message_stop"}"#),
            Some(OpenAiStreamEvent::Done)
        );
        assert_eq!(
            parse_anthropic_stream_line(r#"data: {"type":"message_start"}"#),
            None
        );
    }

    #[test]
    fn test_read_sse_body_openai() {
        let resp =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\", world\"}}]}\n\n\
data: [DONE]\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let mut got = String::new();
        let content = read_sse_body(&mut cur, Provider::OpenAI, &mut |d| got.push_str(d)).unwrap();
        assert_eq!(content, "Hello, world");
        assert_eq!(got, "Hello, world"); // deltas were delivered incrementally
    }

    #[test]
    fn test_read_sse_body_anthropic_and_chunk_lines_ignored() {
        // Interleave a chunked-size line ("1a") to prove non-data lines are skipped.
        let resp = "HTTP/1.1 200 OK\r\n\r\n\
1a\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let content = read_sse_body(&mut cur, Provider::Anthropic, &mut |_| {}).unwrap();
        assert_eq!(content, "AB");
    }

    #[test]
    fn test_read_sse_body_error_status() {
        let resp = "HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":{\"message\":\"bad key\"}}";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let err = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[test]
    fn test_http_status_error_5xx_is_retryable() {
        use crate::backend::is_retryable;
        // 5xx -> Transport (retryable, IMP-9); 4xx -> Protocol (not).
        assert!(is_retryable(&http_status_error(503, "x".into())));
        assert!(is_retryable(&http_status_error(500, "x".into())));
        assert!(!is_retryable(&http_status_error(401, "x".into())));
        assert!(!is_retryable(&http_status_error(400, "x".into())));
    }

    #[test]
    fn test_read_sse_body_5xx_is_transport() {
        use crate::backend::is_retryable;
        let resp = "HTTP/1.1 503 Service Unavailable\r\n\r\n{\"error\":\"busy\"}";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let err = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap_err();
        assert!(is_retryable(&err), "503 should be retryable: {err}");
    }

    #[test]
    fn test_parse_http_response_content_length() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
    }

    #[test]
    fn test_parse_http_response_chunked() {
        // Two chunks: "Hello" (5) and " world" (6), then terminating 0 chunk.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n6\r\n world\r\n0\r\n\r\n";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "Hello world");
    }

    #[test]
    fn test_parse_http_response_status_error() {
        let raw = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}";
        let (status, _) = parse_http_response(raw).unwrap();
        assert_eq!(status, 401);
    }

    #[test]
    fn test_dechunk_single() {
        let raw =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nhello world\r\n0\r\n\r\n";
        let (_, body) = parse_http_response(raw).unwrap();
        assert_eq!(body, "hello world");
    }

    #[test]
    fn test_dechunk_truncated_returns_error() {
        // ADR-107: a chunked body that declares more bytes than are present must
        // return a protocol error, not silently return partial data.
        // "a\r\n" declares 10 bytes but only 5 ("hello") follow.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\nhello";
        let err = parse_http_response(raw);
        assert!(err.is_err(), "truncated chunk must be an error, not silent partial data");
    }

    #[test]
    fn test_api_key_from_env_trims_whitespace() {
        // api_key_from_env must strip surrounding whitespace so keys written via
        // `export KEY=$(cat file)` (which appends a newline) still work.
        std::env::set_var("PASTURE_OPENAI_API_KEY", "sk-test\n");
        let k = api_key_from_env(Provider::OpenAI).unwrap();
        assert_eq!(k, "sk-test", "trailing newline should be trimmed");
        std::env::remove_var("PASTURE_OPENAI_API_KEY");

        std::env::set_var("PASTURE_OPENAI_API_KEY", "  sk-padded  ");
        let k2 = api_key_from_env(Provider::OpenAI).unwrap();
        assert_eq!(k2, "sk-padded", "surrounding spaces should be trimmed");
        std::env::remove_var("PASTURE_OPENAI_API_KEY");

        std::env::set_var("PASTURE_ANTHROPIC_API_KEY", "   \n  ");
        assert!(
            api_key_from_env(Provider::Anthropic).is_none(),
            "all-whitespace key should be None after trimming"
        );
        std::env::remove_var("PASTURE_ANTHROPIC_API_KEY");
    }
}

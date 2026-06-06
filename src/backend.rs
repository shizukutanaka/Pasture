//! Inference backends.
//!
//! A `Backend` turns a chat request into a completion. The trait keeps the
//! routing layer decoupled and testable (mockable). The local Ollama backend
//! speaks plain HTTP over `std::net` to `localhost` (no TLS needed). The cloud
//! backend requires HTTPS and is gated until a vetted TLS dependency is added
//! (ADR-005); for now it reports `Unsupported` clearly rather than failing
//! silently (US-3).

use crate::cloud::Provider;
use crate::json::{escape_string, parse, JsonValue};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// A single chat message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub content: String,
}

/// A normalised chat-completion request.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub stream: bool,
    /// True when the request carries tool/function-calling fields
    /// (`tools` / `functions` / a non-"none" `tool_choice`). Used as a hard
    /// routing signal — tool use is reliable on the stronger model (IMP-10).
    pub has_tools: bool,
}

impl CompletionRequest {
    /// The concatenated user-visible text, used for routing decisions.
    pub fn routing_text(&self) -> String {
        self.messages
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A completed response plus token accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResponse {
    pub content: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// Why a backend could not produce a completion.
#[derive(Debug)]
pub enum BackendError {
    /// Transport/IO failure talking to the backend.
    Transport(String),
    /// The backend returned a malformed or error response.
    Protocol(String),
    /// The backend is not implemented in this build (e.g. cloud needs TLS).
    Unsupported(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Transport(m) => write!(f, "transport error: {m}"),
            BackendError::Protocol(m) => write!(f, "protocol error: {m}"),
            BackendError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// Whether a backend failure is worth retrying (IMP-9). Transport/IO failures
/// are typically transient (timeouts, connection resets, 5xx); protocol errors
/// and unsupported-build errors are not — retrying them just wastes time.
pub fn is_retryable(err: &BackendError) -> bool {
    matches!(err, BackendError::Transport(_))
}

/// An inference backend.
pub trait Backend: Send + Sync {
    fn name(&self) -> &str;
    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError>;

    /// Stream a completion, invoking `on_delta` for each content chunk.
    /// The default implementation calls `complete` and emits the whole answer
    /// as a single chunk, so non-streaming backends still work transparently.
    fn stream_complete(
        &self,
        req: &CompletionRequest,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<CompletionResponse, BackendError> {
        let resp = self.complete(req)?;
        if !resp.content.is_empty() {
            on_delta(&resp.content);
        }
        Ok(resp)
    }

    /// Like `complete`, but also returns a confidence signal when the backend
    /// can provide one: the mean token log-probability of the answer (<= 0;
    /// higher = more confident). Used by the cascade to decide escalation
    /// (arXiv 2605.02241 finds mean log-prob a strong training-free signal).
    /// Default: delegate to `complete` with no signal.
    fn complete_scored(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, Option<f64>), BackendError> {
        Ok((self.complete(req)?, None))
    }
}

/// A parsed line from an Ollama streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OllamaStreamEvent {
    Delta(String),
    Done,
}

/// Parse one NDJSON line from Ollama's streaming `/api/chat` response.
pub fn parse_ollama_stream_line(line: &str) -> Option<OllamaStreamEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v = parse(trimmed).ok()?;
    if v.get("done").and_then(JsonValue::as_bool).unwrap_or(false) {
        return Some(OllamaStreamEvent::Done);
    }
    let content = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str());
    match content {
        Some(c) if !c.is_empty() => Some(OllamaStreamEvent::Delta(c.to_string())),
        _ => None,
    }
}

/// A deterministic in-memory backend for tests.
pub struct MockBackend {
    name: String,
    reply: String,
}

impl MockBackend {
    pub fn new(name: &str, reply: &str) -> Self {
        Self {
            name: name.to_string(),
            reply: reply.to_string(),
        }
    }
}

impl Backend for MockBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
        let prompt_tokens = crate::routing::estimate_tokens(&req.routing_text()) as u64;
        Ok(CompletionResponse {
            content: self.reply.clone(),
            model: req.model.clone(),
            prompt_tokens,
            completion_tokens: crate::routing::estimate_tokens(&self.reply) as u64,
        })
    }
}

/// Local backend that talks to an Ollama server over plain HTTP.
pub struct OllamaBackend {
    host: String,
    port: u16,
    model: String,
}

impl OllamaBackend {
    pub fn new(host: &str, port: u16, model: &str) -> Self {
        Self {
            host: host.to_string(),
            port,
            model: model.to_string(),
        }
    }

    /// Build the Ollama `/api/chat` request body (non-streaming).
    pub fn build_body(req: &CompletionRequest) -> String {
        Self::build_body_inner(req, false)
    }

    /// Build the Ollama `/api/chat` request body with streaming enabled.
    pub fn build_body_streaming(req: &CompletionRequest) -> String {
        Self::build_body_inner(req, true)
    }

    fn build_body_inner(req: &CompletionRequest, stream: bool) -> String {
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
        format!(
            "{{\"model\":\"{}\",\"stream\":{stream},\"messages\":[{}]}}",
            escape_string(&req.model),
            msgs.join(",")
        )
    }

    /// Extract the assistant content from an Ollama `/api/chat` response body.
    pub fn parse_response(body: &str) -> Result<String, BackendError> {
        let v = parse(body).map_err(|e| BackendError::Protocol(e.to_string()))?;
        v.get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| BackendError::Protocol("missing message.content".to_string()))
    }
}

impl Backend for OllamaBackend {
    fn name(&self) -> &str {
        "ollama"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
        let body = Self::build_body(req);
        let response = http_post(
            &self.host,
            self.port,
            "/api/chat",
            &body,
            Duration::from_secs(120),
        )?;
        let content = Self::parse_response(&response)?;
        let prompt_tokens = crate::routing::estimate_tokens(&req.routing_text()) as u64;
        let completion_tokens = crate::routing::estimate_tokens(&content) as u64;
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
        let body = Self::build_body_streaming(req);
        let mut content = String::new();
        http_post_streaming(
            &self.host,
            self.port,
            "/api/chat",
            &body,
            Duration::from_secs(120),
            &mut |line| {
                if let Some(OllamaStreamEvent::Delta(d)) = parse_ollama_stream_line(line) {
                    content.push_str(&d);
                    on_delta(&d);
                }
            },
        )?;
        if content.is_empty() {
            return Err(BackendError::Protocol("empty stream".to_string()));
        }
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

/// Cloud backend placeholder. Real HTTPS support requires a vetted TLS
/// dependency (ADR-005); until then it fails loudly instead of silently.
pub struct CloudBackend {
    provider: String,
}

impl CloudBackend {
    pub fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
        }
    }
}

impl Backend for CloudBackend {
    fn name(&self) -> &str {
        "cloud"
    }

    fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
        Err(BackendError::Unsupported(format!(
            "cloud provider '{}' needs HTTPS/TLS, planned for the next iteration (ADR-005)",
            self.provider
        )))
    }
}

/// Minimal blocking HTTP/1.1 POST over a raw TCP socket (plain HTTP only).
/// A generic OpenAI-compatible local backend: LM Studio, llama.cpp server,
/// vLLM, LocalAI, etc. Plain HTTP (localhost), reusing the OpenAI request and
/// response shaping. The configured model id is always sent (some servers,
/// e.g. LM Studio, reject a mismatched model name).
pub struct OpenAiCompatBackend {
    host: String,
    port: u16,
    path: String,
    model: String,
}

impl OpenAiCompatBackend {
    pub fn new(host: &str, port: u16, path: &str, model: &str) -> Self {
        Self {
            host: host.to_string(),
            port,
            path: path.to_string(),
            model: model.to_string(),
        }
    }
}

impl Backend for OpenAiCompatBackend {
    fn name(&self) -> &str {
        "local"
    }

    fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
        let mut shaped = req.clone();
        shaped.model = self.model.clone();
        let body = Provider::OpenAI.build_body(&shaped);
        let resp_body = http_post(
            &self.host,
            self.port,
            &self.path,
            &body,
            Duration::from_secs(120),
        )?;
        let (content, prompt_tokens, completion_tokens) =
            Provider::OpenAI.parse_response(&resp_body)?;
        Ok(CompletionResponse {
            content,
            model: self.model.clone(),
            prompt_tokens,
            completion_tokens,
        })
    }

    fn complete_scored(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, Option<f64>), BackendError> {
        let mut shaped = req.clone();
        shaped.model = self.model.clone();
        let body = Provider::OpenAI.build_body_logprobs(&shaped);
        let resp_body = http_post(
            &self.host,
            self.port,
            &self.path,
            &body,
            Duration::from_secs(120),
        )?;
        let (content, prompt_tokens, completion_tokens) =
            Provider::OpenAI.parse_response(&resp_body)?;
        let confidence = crate::cloud::mean_logprob_from_openai(&resp_body);
        Ok((
            CompletionResponse {
                content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
            },
            confidence,
        ))
    }

    fn stream_complete(
        &self,
        req: &CompletionRequest,
        on_delta: &mut dyn FnMut(&str),
    ) -> Result<CompletionResponse, BackendError> {
        let mut shaped = req.clone();
        shaped.model = self.model.clone();
        let body = Provider::OpenAI.build_body_stream(&shaped);
        let mut content = String::new();
        http_post_streaming(
            &self.host,
            self.port,
            &self.path,
            &body,
            Duration::from_secs(120),
            &mut |line| {
                if let Some(crate::cloud::OpenAiStreamEvent::Delta(d)) =
                    crate::cloud::parse_openai_stream_line(line)
                {
                    content.push_str(&d);
                    on_delta(&d);
                }
            },
        )?;
        if content.is_empty() {
            return Err(BackendError::Protocol("empty stream".to_string()));
        }
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

fn http_post(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<String, BackendError> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| BackendError::Transport(format!("connect {addr}: {e}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    split_http_body(&text)
        .map(|b| b.to_string())
        .ok_or_else(|| BackendError::Protocol("no HTTP body separator found".to_string()))
}

/// Return the body portion of a raw HTTP response (after the header break).
fn split_http_body(response: &str) -> Option<&str> {
    response.split_once("\r\n\r\n").map(|(_, body)| body)
}

/// Minimal streaming HTTP POST: invokes `on_line` for each `\n`-terminated body
/// line as it arrives (plain HTTP, localhost). Used for Ollama NDJSON streams.
fn http_post_streaming(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
    on_line: &mut dyn FnMut(&str),
) -> Result<(), BackendError> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| BackendError::Transport(format!("connect {addr}: {e}")))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| BackendError::Transport(e.to_string()))?;

    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut header_done = false;

    loop {
        let n = stream
            .read(&mut chunk)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);

        if !header_done {
            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                // Drop headers; keep only the body bytes onward.
                buf.drain(..pos + 4);
                header_done = true;
            } else {
                continue;
            }
        }
        emit_lines(&mut buf, on_line);
    }
    // Flush any trailing partial line (no newline at stream end).
    if header_done && !buf.is_empty() {
        on_line(&String::from_utf8_lossy(&buf));
    }
    Ok(())
}

/// Emit and remove every complete `\n`-terminated line from `buf`.
fn emit_lines(buf: &mut Vec<u8>, on_line: &mut dyn FnMut(&str)) {
    while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=nl).collect();
        let text = String::from_utf8_lossy(&line);
        on_line(text.trim_end_matches(['\r', '\n']));
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: "test".to_string(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: "be brief".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: "hello".to_string(),
                },
            ],
            stream: false,
            has_tools: false,
        }
    }

    #[test]
    fn test_routing_text_joins_messages() {
        assert_eq!(req().routing_text(), "be brief\nhello");
    }

    #[test]
    fn test_mock_backend_returns_reply_and_tokens() {
        let b = MockBackend::new("mock", "hi there");
        let r = b.complete(&req()).unwrap();
        assert_eq!(r.content, "hi there");
        assert_eq!(r.model, "test");
        assert!(r.prompt_tokens > 0);
        assert!(r.completion_tokens > 0);
    }

    #[test]
    fn test_ollama_build_body_contains_model_and_messages() {
        let body = OllamaBackend::build_body(&req());
        assert!(body.contains("\"model\":\"test\""));
        assert!(body.contains("\"stream\":false"));
        assert!(body.contains("\"role\":\"user\""));
        assert!(body.contains("\"content\":\"hello\""));
    }

    #[test]
    fn test_ollama_build_body_escapes_content() {
        let mut r = req();
        r.messages[1].content = "quote \" and \\ slash".to_string();
        let body = OllamaBackend::build_body(&r);
        assert!(body.contains("quote \\\" and \\\\ slash"));
    }

    #[test]
    fn test_ollama_parse_response_extracts_content() {
        let body = r#"{"model":"x","message":{"role":"assistant","content":"answer"}}"#;
        assert_eq!(OllamaBackend::parse_response(body).unwrap(), "answer");
    }

    #[test]
    fn test_ollama_parse_response_missing_content_errors() {
        let body = r#"{"model":"x"}"#;
        assert!(OllamaBackend::parse_response(body).is_err());
    }

    #[test]
    fn test_cloud_backend_reports_unsupported() {
        let b = CloudBackend::new("openai");
        match b.complete(&req()) {
            Err(BackendError::Unsupported(_)) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn test_split_http_body_after_headers() {
        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true}";
        assert_eq!(split_http_body(resp), Some("{\"ok\":true}"));
    }

    #[test]
    fn test_split_http_body_none_when_no_break() {
        assert_eq!(split_http_body("HTTP/1.1 200 OK"), None);
    }

    #[test]
    fn test_parse_ollama_stream_delta() {
        let line = r#"{"model":"x","message":{"role":"assistant","content":"He"},"done":false}"#;
        assert_eq!(
            parse_ollama_stream_line(line),
            Some(OllamaStreamEvent::Delta("He".to_string()))
        );
    }

    #[test]
    fn test_parse_ollama_stream_done() {
        let line = r#"{"model":"x","done":true}"#;
        assert_eq!(
            parse_ollama_stream_line(line),
            Some(OllamaStreamEvent::Done)
        );
    }

    #[test]
    fn test_parse_ollama_stream_empty_and_blank() {
        assert_eq!(parse_ollama_stream_line(""), None);
        let line = r#"{"message":{"content":""},"done":false}"#;
        assert_eq!(parse_ollama_stream_line(line), None);
    }

    #[test]
    fn test_parse_ollama_stream_malformed() {
        assert_eq!(parse_ollama_stream_line("{not json"), None);
    }

    #[test]
    fn test_mock_default_stream_emits_once() {
        let b = MockBackend::new("mock", "hello world");
        let mut chunks: Vec<String> = Vec::new();
        let resp = b
            .stream_complete(&req(), &mut |d| chunks.push(d.to_string()))
            .unwrap();
        assert_eq!(chunks, vec!["hello world".to_string()]);
        assert_eq!(resp.content, "hello world");
    }

    #[test]
    fn test_emit_lines_splits_and_keeps_remainder() {
        let mut buf = b"a\nb\npartial".to_vec();
        let mut lines: Vec<String> = Vec::new();
        emit_lines(&mut buf, &mut |l| lines.push(l.to_string()));
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(buf, b"partial");
    }

    #[test]
    fn test_build_body_streaming_sets_stream_true() {
        assert!(OllamaBackend::build_body_streaming(&req()).contains("\"stream\":true"));
        assert!(OllamaBackend::build_body(&req()).contains("\"stream\":false"));
    }
}

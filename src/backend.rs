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
    /// Sampling parameters the client supplied (temperature, max_tokens, …).
    /// Forwarded to the backend so client intent (determinism, length, stop
    /// sequences) is honoured rather than silently dropped — parity with peer
    /// gateways (LiteLLM/OpenRouter/Ollama/LM Studio/vLLM).
    pub sampling: SamplingParams,
}

/// Client-supplied sampling parameters. All optional; absent fields are left to
/// the backend's own defaults. Non-finite numbers are rejected at parse time, so
/// builders may assume every present value serialises to valid JSON.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SamplingParams {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u64>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    /// Structured-output request (`response_format`), forwarded so JSON mode /
    /// JSON-schema output works through the proxy. Stored as the raw JSON value.
    pub response_format: Option<JsonValue>,
    /// Tool/function definitions (`tools`), forwarded verbatim so the backend can
    /// emit tool calls (IMP-10 / ADR-177). Stored as the raw JSON array value.
    /// Kept here (alongside `response_format`) so every CompletionRequest carries
    /// it without touching the many request-construction sites; `has_tools` on the
    /// request is the derived routing signal, this is the payload that is forwarded.
    pub tools: Option<JsonValue>,
    /// Tool-selection control (`tool_choice`), forwarded verbatim (ADR-177).
    pub tool_choice: Option<JsonValue>,
}

impl SamplingParams {
    /// True when no parameter is set (so builders can omit the wrapper entirely).
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.top_p.is_none()
            && self.max_tokens.is_none()
            && self.stop.is_empty()
            && self.seed.is_none()
            && self.presence_penalty.is_none()
            && self.frequency_penalty.is_none()
    }

    /// OpenAI-style top-level fields, each prefixed with a comma (empty if none).
    /// Suitable to splice before the closing brace of a request object.
    pub fn openai_fields(&self) -> String {
        let mut out = String::new();
        if let Some(t) = self.temperature {
            out.push_str(&format!(",\"temperature\":{t}"));
        }
        if let Some(p) = self.top_p {
            out.push_str(&format!(",\"top_p\":{p}"));
        }
        if let Some(m) = self.max_tokens {
            out.push_str(&format!(",\"max_tokens\":{m}"));
        }
        if let Some(s) = self.seed {
            out.push_str(&format!(",\"seed\":{s}"));
        }
        if let Some(pp) = self.presence_penalty {
            out.push_str(&format!(",\"presence_penalty\":{pp}"));
        }
        if let Some(fp) = self.frequency_penalty {
            out.push_str(&format!(",\"frequency_penalty\":{fp}"));
        }
        if !self.stop.is_empty() {
            out.push_str(&format!(",\"stop\":{}", json_string_array(&self.stop)));
        }
        if let Some(rf) = &self.response_format {
            out.push_str(&format!(",\"response_format\":{}", rf.to_json_string()));
        }
        // Forward tool definitions and selection verbatim so the backend can make
        // tool calls (ADR-177). The proxy already escalated to the stronger model
        // (IMP-10); dropping the payload here would make tool calls impossible.
        if let Some(tools) = &self.tools {
            out.push_str(&format!(",\"tools\":{}", tools.to_json_string()));
        }
        if let Some(tc) = &self.tool_choice {
            out.push_str(&format!(",\"tool_choice\":{}", tc.to_json_string()));
        }
        out
    }

    /// Ollama puts structured-output control in a top-level `format` field (not
    /// in `options`). Map OpenAI's `response_format` onto it, comma-prefixed
    /// (empty when absent or unrecognized):
    /// `{"type":"json_object"}` → `"json"`; `{"type":"json_schema",...}` → the schema.
    pub fn ollama_format_field(&self) -> String {
        let Some(rf) = &self.response_format else {
            return String::new();
        };
        match rf.get("type").and_then(JsonValue::as_str) {
            Some("json_object") => ",\"format\":\"json\"".to_string(),
            Some("json_schema") => rf
                .get("json_schema")
                .and_then(|j| j.get("schema"))
                .map(|schema| format!(",\"format\":{}", schema.to_json_string()))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Ollama top-level `tools` field, comma-prefixed (empty if none). Ollama's
    /// `/api/chat` accepts the same `tools` array shape as OpenAI (ADR-177); it
    /// has no `tool_choice`, so only `tools` is forwarded here.
    pub fn ollama_tools_field(&self) -> String {
        match &self.tools {
            Some(tools) => format!(",\"tools\":{}", tools.to_json_string()),
            None => String::new(),
        }
    }

    /// Ollama `options` object, comma-prefixed (empty if none). Ollama nests
    /// sampling under `options` and calls the length cap `num_predict`.
    pub fn ollama_options(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(t) = self.temperature {
            parts.push(format!("\"temperature\":{t}"));
        }
        if let Some(p) = self.top_p {
            parts.push(format!("\"top_p\":{p}"));
        }
        if let Some(m) = self.max_tokens {
            parts.push(format!("\"num_predict\":{m}"));
        }
        if let Some(s) = self.seed {
            parts.push(format!("\"seed\":{s}"));
        }
        if let Some(pp) = self.presence_penalty {
            parts.push(format!("\"presence_penalty\":{pp}"));
        }
        if let Some(fp) = self.frequency_penalty {
            parts.push(format!("\"frequency_penalty\":{fp}"));
        }
        if !self.stop.is_empty() {
            parts.push(format!("\"stop\":{}", json_string_array(&self.stop)));
        }
        format!(",\"options\":{{{}}}", parts.join(","))
    }
}

/// Serialise a string slice as a JSON array of escaped strings.
fn json_string_array(items: &[String]) -> String {
    let parts: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", escape_string(s)))
        .collect();
    format!("[{}]", parts.join(","))
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionResponse {
    pub content: String,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Raw `tool_calls` JSON array from the assistant message, when the model
    /// chose to call a tool instead of (or alongside) emitting text (ADR-177).
    /// `None` for an ordinary text completion. Forwarded verbatim to the client.
    pub tool_calls: Option<String>,
}

/// Embedding vectors for one or more inputs, plus token accounting (IMP-8).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingsResponse {
    pub model: String,
    pub vectors: Vec<Vec<f64>>,
    pub prompt_tokens: u64,
}

/// Extract a `Vec<f64>` from a JSON array of numbers (non-numbers dropped).
fn json_f64_vec(v: &JsonValue) -> Option<Vec<f64>> {
    v.as_array()
        .map(|a| a.iter().filter_map(JsonValue::as_f64).collect())
}

/// Map an OpenAI-compatible chat path to its sibling embeddings path
/// (`…/chat/completions` → `…/embeddings`), defaulting to `/v1/embeddings`.
fn embeddings_path(chat_path: &str) -> String {
    match chat_path.strip_suffix("/chat/completions") {
        Some(base) => format!("{base}/embeddings"),
        None => "/v1/embeddings".to_string(),
    }
}

/// Build an embeddings request body: `{"model":..,"input":[strings]}`.
fn embeddings_body(model: &str, inputs: &[String]) -> String {
    let arr: Vec<String> = inputs
        .iter()
        .map(|s| format!("\"{}\"", escape_string(s)))
        .collect();
    format!(
        "{{\"model\":\"{}\",\"input\":[{}]}}",
        escape_string(model),
        arr.join(",")
    )
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

    /// Produce embeddings for `inputs` (IMP-8). Default: unsupported — backends
    /// that expose an embeddings endpoint override this.
    fn embeddings(&self, _inputs: &[String]) -> Result<EmbeddingsResponse, BackendError> {
        Err(BackendError::Unsupported(
            "embeddings not supported by this backend".to_string(),
        ))
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
            tool_calls: None,
        })
    }

    fn embeddings(&self, inputs: &[String]) -> Result<EmbeddingsResponse, BackendError> {
        // Deterministic stand-in vectors for tests: [char_count, 0.0] per input.
        let vectors = inputs
            .iter()
            .map(|s| vec![s.chars().count() as f64, 0.0])
            .collect();
        Ok(EmbeddingsResponse {
            model: self.name.clone(),
            vectors,
            prompt_tokens: inputs.len() as u64,
        })
    }
}

/// Local backend that talks to an Ollama server over plain HTTP.
pub struct OllamaBackend {
    host: String,
    port: u16,
    model: String,
    timeout: Duration,
}

impl OllamaBackend {
    pub fn new(host: &str, port: u16, model: &str, timeout: Duration) -> Self {
        Self {
            host: host.to_string(),
            port,
            model: model.to_string(),
            timeout,
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
            "{{\"model\":\"{}\",\"stream\":{stream},\"messages\":[{}]{}{}{}}}",
            escape_string(&req.model),
            msgs.join(","),
            req.sampling.ollama_format_field(),
            req.sampling.ollama_tools_field(),
            req.sampling.ollama_options()
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
            self.timeout,
        )?;
        let content = Self::parse_response(&response)?;
        let prompt_tokens = crate::routing::estimate_tokens(&req.routing_text()) as u64;
        let completion_tokens = crate::routing::estimate_tokens(&content) as u64;
        Ok(CompletionResponse {
            content,
            model: self.model.clone(),
            prompt_tokens,
            completion_tokens,
            tool_calls: None,
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
            self.timeout,
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
            tool_calls: None,
        })
    }

    fn embeddings(&self, inputs: &[String]) -> Result<EmbeddingsResponse, BackendError> {
        // Ollama `/api/embed`: {"model","input":[..]} -> {"embeddings":[[..]]}.
        let body = embeddings_body(&self.model, inputs);
        let resp = http_post(
            &self.host,
            self.port,
            "/api/embed",
            &body,
            self.timeout,
        )?;
        let v = parse(&resp).map_err(|e| BackendError::Protocol(e.to_string()))?;
        let vectors: Vec<Vec<f64>> = v
            .get("embeddings")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| BackendError::Protocol("missing embeddings".to_string()))?
            .iter()
            .filter_map(json_f64_vec)
            .collect();
        if vectors.is_empty() {
            return Err(BackendError::Protocol("empty embeddings".to_string()));
        }
        let prompt_tokens = inputs
            .iter()
            .map(|s| crate::routing::estimate_tokens(s) as u64)
            .sum();
        Ok(EmbeddingsResponse {
            model: self.model.clone(),
            vectors,
            prompt_tokens,
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
    timeout: Duration,
}

impl OpenAiCompatBackend {
    pub fn new(host: &str, port: u16, path: &str, model: &str, timeout: Duration) -> Self {
        Self {
            host: host.to_string(),
            port,
            path: path.to_string(),
            model: model.to_string(),
            timeout,
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
            self.timeout,
        )?;
        let (content, tool_calls, prompt_tokens, completion_tokens) =
            Provider::OpenAI.parse_response(&resp_body)?;
        Ok(CompletionResponse {
            content,
            model: self.model.clone(),
            prompt_tokens,
            completion_tokens,
            tool_calls,
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
            self.timeout,
        )?;
        let (content, tool_calls, prompt_tokens, completion_tokens) =
            Provider::OpenAI.parse_response(&resp_body)?;
        let confidence = crate::cloud::mean_logprob_from_openai(&resp_body);
        Ok((
            CompletionResponse {
                content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
                tool_calls,
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
        // build_body_stream now includes stream_options.include_usage so
        // backends that support it send a final usage chunk (ADR-173).
        let body = Provider::OpenAI.build_body_stream(&shaped);
        let mut content = String::new();
        let mut stream_usage: Option<(u64, u64)> = None;
        let mut tool_acc = crate::cloud::ToolCallAccumulator::default();
        http_post_streaming(
            &self.host,
            self.port,
            &self.path,
            &body,
            self.timeout,
            &mut |line| {
                match crate::cloud::parse_openai_stream_line(line) {
                    Some(crate::cloud::OpenAiStreamEvent::Delta(d)) => {
                        content.push_str(&d);
                        on_delta(&d);
                    }
                    Some(crate::cloud::OpenAiStreamEvent::Usage(p, c)) => {
                        stream_usage = Some((p, c));
                    }
                    Some(crate::cloud::OpenAiStreamEvent::ToolCallDelta(frag)) => {
                        tool_acc.push(&frag); // accumulate streamed tool_calls (ADR-178)
                    }
                    _ => {}
                }
            },
        )?;
        let tool_calls = tool_acc.finish();
        // A pure tool-call stream has empty content but a tool_calls array (ADR-178).
        if content.is_empty() && tool_calls.is_none() {
            return Err(BackendError::Protocol("empty stream".to_string()));
        }
        // Use actual usage from the stream; fall back to estimate only if the
        // backend did not send a usage chunk (ADR-173).
        let (prompt_tokens, completion_tokens) = stream_usage.unwrap_or_else(|| (
            crate::routing::estimate_tokens(&req.routing_text()) as u64,
            crate::routing::estimate_tokens(&content) as u64,
        ));
        Ok(CompletionResponse {
            content,
            model: self.model.clone(),
            prompt_tokens,
            completion_tokens,
            tool_calls,
        })
    }

    fn embeddings(&self, inputs: &[String]) -> Result<EmbeddingsResponse, BackendError> {
        // OpenAI-compatible `/v1/embeddings`: {"data":[{"embedding":[..]},..]}.
        let path = embeddings_path(&self.path);
        let body = embeddings_body(&self.model, inputs);
        let resp = http_post(
            &self.host,
            self.port,
            &path,
            &body,
            self.timeout,
        )?;
        let v = parse(&resp).map_err(|e| BackendError::Protocol(e.to_string()))?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err.as_str())
                .unwrap_or("unknown error");
            return Err(BackendError::Protocol(format!("server error: {msg}")));
        }
        let vectors: Vec<Vec<f64>> = v
            .get("data")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| BackendError::Protocol("missing data[]".to_string()))?
            .iter()
            .filter_map(|d| d.get("embedding").and_then(json_f64_vec))
            .collect();
        if vectors.is_empty() {
            return Err(BackendError::Protocol("empty embeddings".to_string()));
        }
        let prompt_tokens = inputs
            .iter()
            .map(|s| crate::routing::estimate_tokens(s) as u64)
            .sum();
        Ok(EmbeddingsResponse {
            model: self.model.clone(),
            vectors,
            prompt_tokens,
        })
    }
}

/// Connect with read/write timeouts and send a JSON POST, returning the
/// stream positioned to read the response. Shared by the buffered and
/// streaming HTTP paths so the request wire format has one source of truth.
fn send_json_post(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<TcpStream, BackendError> {
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| {
            // Log the full address for the operator; omit it from the client-
            // facing message to avoid leaking internal network topology (ADR-157).
            eprintln!("pasture: local backend connect {addr}: {e}");
            BackendError::Transport(format!("local backend unreachable ({e})"))
        })?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| BackendError::Transport(e.to_string()))?;
    Ok(stream)
}

fn http_post(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<String, BackendError> {
    let mut stream = send_json_post(host, port, path, body, timeout)?;
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
    let mut stream = send_json_post(host, port, path, body, timeout)?;

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
                // Parse and validate the HTTP status code (ADR-174):
                // without this check a non-2xx response silently becomes
                // Protocol("empty stream") — the wrong error for a 4xx/5xx.
                let header_text = String::from_utf8_lossy(&buf[..pos]);
                let status: u16 = header_text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if !(200..300).contains(&status) {
                    let body_bytes = buf[pos + 4..].to_vec();
                    let mut rest = body_bytes;
                    let _ = stream.read_to_end(&mut rest);
                    let snippet: String =
                        String::from_utf8_lossy(&rest).chars().take(200).collect();
                    return Err(crate::cloud::http_status_error(
                        status,
                        format!("HTTP {status}: {snippet}"),
                    ));
                }
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
            sampling: Default::default(),
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

    #[test]
    fn test_ollama_body_no_options_when_sampling_empty() {
        // Default (empty) sampling must not add an options object.
        assert!(!OllamaBackend::build_body(&req()).contains("\"options\""));
    }

    #[test]
    fn test_ollama_body_threads_sampling_into_options() {
        let mut r = req();
        r.sampling = SamplingParams {
            temperature: Some(0.0),
            max_tokens: Some(128),
            stop: vec!["STOP".to_string()],
            ..Default::default()
        };
        let body = OllamaBackend::build_body(&r);
        assert!(body.contains("\"options\":{"), "{body}");
        assert!(body.contains("\"temperature\":0"), "{body}");
        // Ollama names the length cap num_predict, not max_tokens.
        assert!(body.contains("\"num_predict\":128"), "{body}");
        assert!(body.contains("\"stop\":[\"STOP\"]"), "{body}");
        assert!(!body.contains("\"max_tokens\""), "{body}");
    }

    #[test]
    fn test_ollama_format_json_object() {
        let mut r = req();
        r.sampling.response_format = Some(crate::json::parse(r#"{"type":"json_object"}"#).unwrap());
        let body = OllamaBackend::build_body(&r);
        assert!(body.contains("\"format\":\"json\""), "{body}");
    }

    #[test]
    fn test_ollama_format_json_schema() {
        let mut r = req();
        r.sampling.response_format = Some(
            crate::json::parse(
                r#"{"type":"json_schema","json_schema":{"schema":{"type":"object"}}}"#,
            )
            .unwrap(),
        );
        let body = OllamaBackend::build_body(&r);
        assert!(body.contains("\"format\":{\"type\":\"object\"}"), "{body}");
    }

    #[test]
    fn test_ollama_no_format_when_absent() {
        assert!(!OllamaBackend::build_body(&req()).contains("\"format\""));
    }

    #[test]
    fn test_openai_fields_includes_response_format() {
        let s = SamplingParams {
            response_format: Some(crate::json::parse(r#"{"type":"json_object"}"#).unwrap()),
            ..Default::default()
        };
        let f = s.openai_fields();
        assert!(
            f.contains("\"response_format\":{\"type\":\"json_object\"}"),
            "{f}"
        );
    }

    #[test]
    fn test_sampling_openai_fields_finite_only() {
        let s = SamplingParams {
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(64),
            seed: Some(42),
            ..Default::default()
        };
        let f = s.openai_fields();
        assert!(f.contains("\"temperature\":0.7"));
        assert!(f.contains("\"top_p\":0.9"));
        assert!(f.contains("\"max_tokens\":64"));
        assert!(f.contains("\"seed\":42"));
        // Empty sampling produces no fields.
        assert_eq!(SamplingParams::default().openai_fields(), "");
    }

    #[test]
    fn test_openai_fields_forwards_tools_and_tool_choice() {
        // ADR-177: tools and tool_choice are forwarded verbatim so the backend
        // can make tool calls.
        let s = SamplingParams {
            tools: Some(
                crate::json::parse(r#"[{"type":"function","function":{"name":"f"}}]"#).unwrap(),
            ),
            tool_choice: Some(crate::json::parse(r#""auto""#).unwrap()),
            ..Default::default()
        };
        let f = s.openai_fields();
        // (the JSON serializer sorts object keys, so match on stable substrings)
        assert!(f.contains("\"tools\":["), "{f}");
        assert!(f.contains("\"name\":\"f\""), "{f}");
        assert!(f.contains("\"tool_choice\":\"auto\""), "{f}");
        // Absent → not emitted.
        assert_eq!(SamplingParams::default().openai_fields(), "");
    }

    #[test]
    fn test_ollama_forwards_tools() {
        // ADR-177: Ollama's /api/chat accepts the same tools shape (no tool_choice).
        let mut r = req();
        r.sampling.tools =
            Some(crate::json::parse(r#"[{"type":"function","function":{"name":"f"}}]"#).unwrap());
        r.sampling.tool_choice = Some(crate::json::parse(r#""auto""#).unwrap());
        let body = OllamaBackend::build_body(&r);
        assert!(body.contains("\"tools\":["), "{body}");
        assert!(body.contains("\"name\":\"f\""), "{body}");
        // Ollama has no tool_choice; it must not be injected.
        assert!(!body.contains("tool_choice"), "{body}");
        assert!(crate::json::parse(&body).is_ok(), "valid JSON: {body}");
    }
}

//! OpenAI-compatible routing proxy.
//!
//! `handle_chat` is pure with respect to the network (backends are injected),
//! so it is unit-tested with mock backends. `serve` adds a minimal blocking
//! HTTP/1.1 server loop over `std::net` (plain HTTP, localhost).

use crate::backend::{
    is_retryable, Backend, BackendError, CompletionRequest, CompletionResponse, EmbeddingsResponse,
    Message,
};
use crate::cost::CostRecord;
use crate::json::{escape_string, parse, JsonValue};
use crate::routing::{Decision, Route, RoutingEngine};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;

/// Errors surfaced to the HTTP client.
#[derive(Debug)]
pub enum ProxyError {
    BadRequest(String),
    Routing(String),
    Backend(String),
}

impl ProxyError {
    fn status(&self) -> u16 {
        match self {
            ProxyError::BadRequest(_) => 400,
            ProxyError::Routing(_) => 503,
            ProxyError::Backend(_) => 502,
        }
    }

    fn message(&self) -> &str {
        match self {
            ProxyError::BadRequest(m) | ProxyError::Routing(m) | ProxyError::Backend(m) => m,
        }
    }

    /// OpenAI-style error `type` for the error envelope (SPEC §3.5).
    fn kind(&self) -> &'static str {
        match self {
            ProxyError::BadRequest(_) => "invalid_request_error",
            ProxyError::Routing(_) => "routing_error",
            ProxyError::Backend(_) => "upstream_error",
        }
    }
}

impl std::fmt::Display for ProxyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

/// The proxy: a routing engine plus optional local and cloud backends.
pub struct Proxy {
    engine: RoutingEngine,
    local: Option<Box<dyn Backend>>,
    cloud: Option<Box<dyn Backend>>,
    cost_log_path: String,
    cascade: bool,
    cascade_logprob_threshold: f64,
    cache: Option<std::sync::Mutex<crate::cache::ResponseCache>>,
    /// Model ids advertised on `GET /v1/models` (IMP-8). Clients commonly probe
    /// this endpoint on connect; an empty list still returns a valid response.
    models: Vec<String>,
    /// Number of times to retry a transient cloud failure before falling back
    /// to local (IMP-9). 0 disables retries (a single attempt).
    cloud_retry: u32,
}

/// Base backoff (doubled each attempt) for cloud retries (IMP-9).
const CLOUD_RETRY_BASE_MS: u64 = 200;

impl Proxy {
    pub fn new(
        engine: RoutingEngine,
        local: Option<Box<dyn Backend>>,
        cloud: Option<Box<dyn Backend>>,
        cost_log_path: &str,
    ) -> Self {
        Self {
            engine,
            local,
            cloud,
            cost_log_path: cost_log_path.to_string(),
            cascade: false,
            cascade_logprob_threshold: -1.0,
            cache: None,
            models: Vec::new(),
            cloud_retry: 0,
        }
    }

    /// Retry a transient cloud failure up to `n` times (exponential backoff)
    /// before falling back to local (IMP-9).
    pub fn with_cloud_retry(mut self, n: u32) -> Self {
        self.cloud_retry = n;
        self
    }

    /// Advertise the given model ids on `GET /v1/models` (IMP-8). Duplicates
    /// and empties are dropped so the list is clean.
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        let mut seen = Vec::new();
        for m in models {
            if !m.is_empty() && !seen.contains(&m) {
                seen.push(m);
            }
        }
        self.models = seen;
        self
    }

    /// Enable FrugalGPT-style cascade (try local, escalate on low confidence).
    pub fn with_cascade(mut self, enabled: bool) -> Self {
        self.cascade = enabled;
        self
    }

    /// Mean-logprob threshold below which the cascade escalates to cloud
    /// (used when the local backend reports confidence; arXiv 2605.02241).
    pub fn with_cascade_logprob(mut self, threshold: f64) -> Self {
        self.cascade_logprob_threshold = threshold;
        self
    }

    /// Enable an exact-match response cache holding up to `cap` entries
    /// (cap of 0 leaves the cache disabled).
    pub fn with_cache(mut self, cap: usize) -> Self {
        if cap > 0 {
            self.cache = Some(std::sync::Mutex::new(crate::cache::ResponseCache::new(cap)));
        }
        self
    }

    /// Parse an OpenAI-style chat-completion request body.
    pub fn parse_request(body: &str) -> Result<CompletionRequest, ProxyError> {
        let v = parse(body).map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        let model = v
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("default")
            .to_string();
        let stream = v
            .get("stream")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let messages = v
            .get("messages")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| ProxyError::BadRequest("missing 'messages' array".to_string()))?;
        let mut parsed = Vec::with_capacity(messages.len());
        for m in messages {
            let role = m
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("user")
                .to_string();
            let content = m
                .get("content")
                .and_then(|c| c.as_str())
                .ok_or_else(|| ProxyError::BadRequest("message missing 'content'".to_string()))?
                .to_string();
            parsed.push(Message { role, content });
        }
        if parsed.is_empty() {
            return Err(ProxyError::BadRequest("no messages provided".to_string()));
        }
        // Tool/function-calling presence is a hard routing signal (IMP-10):
        // a non-empty `tools` or `functions` array means the client expects
        // reliable tool use, which the stronger (cloud) model handles best.
        let non_empty_array = |key: &str| {
            v.get(key)
                .and_then(JsonValue::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        };
        let has_tools = non_empty_array("tools") || non_empty_array("functions");
        Ok(CompletionRequest {
            model,
            messages: parsed,
            stream,
            has_tools,
        })
    }

    /// Classify, then decide routing (keeps sensitive content local).
    /// Returns the decision and whether the content was sensitive.
    fn classify_and_decide(&self, req: &CompletionRequest) -> Result<(Decision, bool), ProxyError> {
        let text = req.routing_text();
        let report = crate::privacy::classify(&text);
        let sensitive = report.is_sensitive();
        if sensitive {
            eprintln!(
                "pasture: sensitive content detected -> keeping local ({} categories)",
                report.categories.len()
            );
        }
        let decision = self
            .engine
            .decide_full(&text, None, sensitive, req.has_tools)
            .map_err(|e| ProxyError::Routing(e.to_string()))?;
        Ok((decision, sensitive))
    }

    fn backend_for(&self, route: Route) -> Result<&dyn Backend, ProxyError> {
        match route {
            Route::Local => self.local.as_deref(),
            Route::Cloud => self.cloud.as_deref(),
        }
        .ok_or_else(|| ProxyError::Routing(format!("no backend for route {}", route.as_str())))
    }

    fn log_cost(&self, route_label: &'static str, resp: &CompletionResponse, logprob: Option<f64>) {
        // Record cost (local and cache are free). PII is never written (I5);
        // logprob is the local-answer confidence number, not content.
        let record = CostRecord::new(
            route_label,
            &resp.model,
            resp.prompt_tokens,
            resp.completion_tokens,
            0.0,
        )
        .with_logprob(logprob);
        if let Err(e) = record.append_to(&self.cost_log_path) {
            eprintln!("pasture: cost log write failed: {e}");
        }
    }

    /// Run a completion for an already-parsed request (buffered), applying the
    /// cache and cascade strategies when enabled. Returns (response, label).
    fn run_completion(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, &'static str, Option<f64>), ProxyError> {
        let (decision, sensitive) = self.classify_and_decide(req)?;

        // Exact-match cache (never for sensitive content; I5).
        let cache_key = if !sensitive {
            Some(crate::cache::request_key(req))
        } else {
            None
        };
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            if let Ok(guard) = cache.lock() {
                if let Some(hit) = guard.get(key) {
                    return Ok((hit, "cache", None));
                }
            }
        }

        // Cascade: try local first, escalate to cloud on low confidence.
        // Never for sensitive content (privacy) or when no cloud is available.
        let (resp, route, logprob) = if self.cascade
            && !sensitive
            && decision.route == Route::Local
            && self.cloud.is_some()
            && self.local.is_some()
        {
            let local = self.backend_for(Route::Local)?;
            let (local_resp, confidence) = local
                .complete_scored(req)
                .map_err(|e| ProxyError::Backend(e.to_string()))?;
            if crate::cascade::should_escalate(
                &local_resp.content,
                confidence,
                self.cascade_logprob_threshold,
            ) {
                let cloud = self.backend_for(Route::Cloud)?;
                match complete_with_retry(cloud, req, self.cloud_retry, CLOUD_RETRY_BASE_MS) {
                    Ok(cloud_resp) => (cloud_resp, Route::Cloud, confidence),
                    // Cloud failed: fall back to the local answer rather than error.
                    Err(_) => (local_resp, Route::Local, confidence),
                }
            } else {
                (local_resp, Route::Local, confidence)
            }
        } else if decision.route == Route::Cloud {
            // Cloud route: retry transient failures, then fall back to local if
            // one is available rather than erroring the request (IMP-9).
            let cloud = self.backend_for(Route::Cloud)?;
            match complete_with_retry(cloud, req, self.cloud_retry, CLOUD_RETRY_BASE_MS) {
                Ok(resp) => (resp, Route::Cloud, None),
                Err(e) => match self.local.as_deref() {
                    Some(local) => {
                        eprintln!("pasture: cloud failed ({e}); falling back to local");
                        let resp = local
                            .complete(req)
                            .map_err(|e| ProxyError::Backend(e.to_string()))?;
                        (resp, Route::Local, None)
                    }
                    None => return Err(ProxyError::Backend(e.to_string())),
                },
            }
        } else {
            let backend = self.backend_for(decision.route)?;
            let resp = backend
                .complete(req)
                .map_err(|e| ProxyError::Backend(e.to_string()))?;
            (resp, decision.route, None)
        };

        // Store on miss.
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            if let Ok(mut guard) = cache.lock() {
                guard.put(key, resp.clone());
            }
        }

        Ok((resp, route.as_str(), logprob))
    }

    /// Handle a chat-completion request end to end (buffered), returning the
    /// response body. Used for non-streaming requests and by tests.
    pub fn handle_chat(&self, body: &str) -> Result<String, ProxyError> {
        let req = Self::parse_request(body)?;
        let (resp, label, logprob) = self.run_completion(&req)?;
        self.log_cost(label, &resp, logprob);
        Ok(build_openai_response(&resp, label))
    }

    /// Handle `POST /v1/embeddings` (IMP-8). Inputs are embedded by the **local**
    /// backend (embeddings stay on the machine — privacy/local-first); without a
    /// local backend this is a `503`.
    pub fn handle_embeddings(&self, body: &str) -> Result<String, ProxyError> {
        let inputs = Self::parse_embeddings_request(body)?;
        let local = self
            .local
            .as_deref()
            .ok_or_else(|| ProxyError::Routing("no local backend for embeddings".to_string()))?;
        let resp = local
            .embeddings(&inputs)
            .map_err(|e| ProxyError::Backend(e.to_string()))?;
        Ok(build_embeddings_response(&resp))
    }

    /// Parse an OpenAI embeddings `input`: a string or an array of strings.
    fn parse_embeddings_request(body: &str) -> Result<Vec<String>, ProxyError> {
        let v = parse(body).map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        let input = v
            .get("input")
            .ok_or_else(|| ProxyError::BadRequest("missing 'input'".to_string()))?;
        let inputs = match input {
            JsonValue::Str(s) => vec![s.clone()],
            JsonValue::Array(a) => {
                let mut out = Vec::with_capacity(a.len());
                for item in a {
                    match item.as_str() {
                        Some(s) => out.push(s.to_string()),
                        None => {
                            return Err(ProxyError::BadRequest(
                                "'input' array must contain strings".to_string(),
                            ))
                        }
                    }
                }
                out
            }
            _ => {
                return Err(ProxyError::BadRequest(
                    "'input' must be a string or array of strings".to_string(),
                ))
            }
        };
        if inputs.is_empty() {
            return Err(ProxyError::BadRequest("empty 'input'".to_string()));
        }
        Ok(inputs)
    }

    /// Run the blocking HTTP server until the process is stopped.
    /// Serve requests concurrently with a bounded worker pool. Each connection
    /// is handled on a worker thread; the accept loop feeds a bounded queue so a
    /// connection flood applies backpressure instead of spawning unbounded
    /// threads. Shared state (the cache) is already behind a mutex.
    pub fn serve(self, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        let workers = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 32);
        eprintln!(
            "pasture: listening on http://{addr} ({workers} workers, POST /v1/chat/completions, GET /v1/models)"
        );

        let proxy = Arc::new(self);
        // Bounded queue: backpressure when all workers are busy.
        let (tx, rx) = sync_channel::<TcpStream>(workers * 4);
        let rx = Arc::new(Mutex::new(rx));
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let proxy = Arc::clone(&proxy);
            let rx = Arc::clone(&rx);
            handles.push(thread::spawn(move || loop {
                // Lock only to dequeue; handling happens off-lock (concurrent).
                let next = {
                    let guard = match rx.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    guard.recv()
                };
                let mut stream = match next {
                    Ok(s) => s,
                    Err(_) => break, // sender dropped: shut down
                };
                if let Err(e) = proxy.handle_connection(&mut stream) {
                    eprintln!("pasture: connection error: {e}");
                }
            }));
        }

        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if tx.send(s).is_err() {
                        break; // all workers gone
                    }
                }
                Err(e) => eprintln!("pasture: accept error: {e}"),
            }
        }
        drop(tx);
        for h in handles {
            let _ = h.join();
        }
        Ok(())
    }

    fn handle_connection(&self, stream: &mut std::net::TcpStream) -> std::io::Result<()> {
        let (method, path, body) = match read_request(stream)? {
            ReadOutcome::Request { method, path, body } => (method, path, body),
            ReadOutcome::TooLarge => {
                let payload =
                    build_error_response("request body too large", "invalid_request_error");
                write_response(stream, 413, &payload)?;
                return Ok(());
            }
            ReadOutcome::Closed => {
                let payload = build_error_response("malformed request", "invalid_request_error");
                write_response(stream, 400, &payload)?;
                return Ok(());
            }
        };
        if method == "POST" && path.starts_with("/v1/chat/completions") {
            match Self::parse_request(&body) {
                Ok(req) if req.stream => self.stream_chat_to_socket(stream, &req)?,
                Ok(req) => match self.complete_buffered(&req) {
                    Ok(resp) => write_response(stream, 200, &resp)?,
                    Err(e) => {
                        write_response(
                            stream,
                            e.status(),
                            &build_error_response(e.message(), e.kind()),
                        )?;
                    }
                },
                Err(e) => {
                    write_response(
                        stream,
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                    )?;
                }
            }
        } else if method == "POST" && path.starts_with("/v1/embeddings") {
            match self.handle_embeddings(&body) {
                Ok(resp) => write_response(stream, 200, &resp)?,
                Err(e) => {
                    write_response(
                        stream,
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                    )?;
                }
            }
        } else if method == "GET" && path.starts_with("/v1/models") {
            write_response(stream, 200, &build_models_response(&self.models))?;
        } else if method == "GET" && path.starts_with("/health") {
            write_response(stream, 200, "{\"status\":\"ok\"}")?;
        } else {
            write_response(
                stream,
                404,
                &build_error_response("not found", "invalid_request_error"),
            )?;
        }
        Ok(())
    }

    /// Non-streaming completion from an already-parsed request.
    fn complete_buffered(&self, req: &CompletionRequest) -> Result<String, ProxyError> {
        let (resp, label, logprob) = self.run_completion(req)?;
        self.log_cost(label, &resp, logprob);
        Ok(build_openai_response(&resp, label))
    }

    /// Stream a completion to the socket as Server-Sent Events (IMP-7).
    /// Note: cascade is not applied to streaming requests (the local answer
    /// cannot be un-sent); the routed backend streams directly.
    fn stream_chat_to_socket(
        &self,
        sock: &mut std::net::TcpStream,
        req: &CompletionRequest,
    ) -> std::io::Result<()> {
        let decision = match self.classify_and_decide(req) {
            Ok((d, _)) => d,
            Err(e) => {
                return write_response(
                    sock,
                    e.status(),
                    &build_error_response(e.message(), e.kind()),
                );
            }
        };
        let backend = match self.backend_for(decision.route) {
            Ok(b) => b,
            Err(e) => {
                return write_response(
                    sock,
                    e.status(),
                    &build_error_response(e.message(), e.kind()),
                );
            }
        };

        write_sse_headers(sock)?;
        let route_label = decision.route.as_str();
        let mut io_err: Option<std::io::Error> = None;
        let resp = backend.stream_complete(req, &mut |delta| {
            if io_err.is_some() {
                return;
            }
            let frame = sse_frame(&build_openai_chunk(delta, route_label, None));
            if let Err(e) = sock.write_all(frame.as_bytes()) {
                io_err = Some(e);
            }
        });
        if let Some(e) = io_err {
            return Err(e);
        }
        match resp {
            Ok(r) => {
                self.log_cost(route_label, &r, None);
                let stop = sse_frame(&build_openai_chunk("", route_label, Some("stop")));
                sock.write_all(stop.as_bytes())?;
            }
            Err(e) => {
                let err = sse_frame(&build_error_response(&e.to_string(), "upstream_error"));
                sock.write_all(err.as_bytes())?;
            }
        }
        sock.write_all(b"data: [DONE]\n\n")
    }
}

/// Attempt a completion, retrying transient failures with exponential backoff
/// (IMP-9). Non-retryable errors (protocol/auth) return immediately. A
/// `base_delay_ms` of 0 skips sleeping — used by tests to stay fast.
fn complete_with_retry(
    backend: &dyn Backend,
    req: &CompletionRequest,
    retries: u32,
    base_delay_ms: u64,
) -> Result<CompletionResponse, BackendError> {
    let mut attempt = 0u32;
    loop {
        match backend.complete(req) {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                if attempt >= retries || !is_retryable(&e) {
                    return Err(e);
                }
                if base_delay_ms > 0 {
                    let backoff = base_delay_ms.saturating_mul(1u64 << attempt.min(16));
                    std::thread::sleep(std::time::Duration::from_millis(backoff));
                }
                attempt += 1;
            }
        }
    }
}

/// Current Unix time in seconds (for the OpenAI `created` field).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build an OpenAI-compatible error body: `{"error":{"message":..,"type":..}}`
/// (SPEC §3.5). Both fields are JSON-escaped.
pub fn build_error_response(message: &str, kind: &str) -> String {
    format!(
        "{{\"error\":{{\"message\":\"{}\",\"type\":\"{}\"}}}}",
        escape_string(message),
        escape_string(kind),
    )
}

/// Build an OpenAI-compatible `GET /v1/models` list response (IMP-8).
/// Each model id is emitted as an `object: "model"` entry owned by "pasture".
pub fn build_models_response(models: &[String]) -> String {
    let entries: Vec<String> = models
        .iter()
        .map(|m| {
            format!(
                "{{\"id\":\"{}\",\"object\":\"model\",\"owned_by\":\"pasture\"}}",
                escape_string(m)
            )
        })
        .collect();
    format!("{{\"object\":\"list\",\"data\":[{}]}}", entries.join(","))
}

/// Format a float vector as a JSON array (finite values; non-finite → 0).
fn fmt_float_array(v: &[f64]) -> String {
    let nums: Vec<String> = v
        .iter()
        .map(|x| {
            if x.is_finite() {
                format!("{x}")
            } else {
                "0".to_string()
            }
        })
        .collect();
    format!("[{}]", nums.join(","))
}

/// Build an OpenAI-compatible `POST /v1/embeddings` response (IMP-8).
pub fn build_embeddings_response(resp: &EmbeddingsResponse) -> String {
    let data: Vec<String> = resp
        .vectors
        .iter()
        .enumerate()
        .map(|(i, vec)| {
            format!(
                "{{\"object\":\"embedding\",\"index\":{i},\"embedding\":{}}}",
                fmt_float_array(vec)
            )
        })
        .collect();
    format!(
        "{{\"object\":\"list\",\"data\":[{}],\"model\":\"{}\",\"usage\":{{\"prompt_tokens\":{},\"total_tokens\":{}}}}}",
        data.join(","),
        escape_string(&resp.model),
        resp.prompt_tokens,
        resp.prompt_tokens
    )
}

/// Build an OpenAI-compatible chat-completion response JSON string.
pub fn build_openai_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    format!(
        "{{\"id\":\"pasture\",\"object\":\"chat.completion\",\"created\":{},\"model\":\"{}\",\"x_pasture_route\":\"{}\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
        unix_now(),
        escape_string(&resp.model),
        route_label,
        escape_string(&resp.content),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build an OpenAI-compatible streaming chunk (`chat.completion.chunk`).
pub fn build_openai_chunk(delta: &str, route_label: &str, finish: Option<&str>) -> String {
    let delta_field = if delta.is_empty() {
        "{}".to_string()
    } else {
        format!("{{\"content\":\"{}\"}}", escape_string(delta))
    };
    let finish_field = match finish {
        Some(f) => format!("\"{f}\""),
        None => "null".to_string(),
    };
    format!(
        "{{\"id\":\"pasture\",\"object\":\"chat.completion.chunk\",\"created\":{},\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{delta_field},\"finish_reason\":{finish_field}}}]}}",
        unix_now()
    )
}

/// Wrap a payload in a Server-Sent Events `data:` frame.
pub fn sse_frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

fn write_sse_headers(stream: &mut std::net::TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    stream.write_all(headers.as_bytes())
}
/// Maximum request body the proxy will read (SPEC §7). Far above any real chat
/// payload; a larger `Content-Length`, or a body that grows past it, yields 413
/// instead of an unbounded read (DoS guard, IMP-21).
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Outcome of reading one HTTP request off the socket.
enum ReadOutcome {
    Request {
        method: String,
        path: String,
        body: String,
    },
    /// The declared or actual body exceeded `MAX_BODY_BYTES` → 413.
    TooLarge,
    /// Connection closed early or headers were malformed/oversized → 400.
    Closed,
}

fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<ReadOutcome> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until headers are complete.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(ReadOutcome::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 1_048_576 {
            return Ok(ReadOutcome::Closed); // 1 MiB header guard
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    for line in lines {
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }

    // Reject oversized bodies before reading them (DoS guard, SPEC §7).
    if content_length > MAX_BODY_BYTES {
        return Ok(ReadOutcome::TooLarge);
    }

    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
        if body.len() > MAX_BODY_BYTES {
            return Ok(ReadOutcome::TooLarge);
        }
    }
    body.truncate(content_length);
    Ok(ReadOutcome::Request {
        method,
        path,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
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
    use crate::backend::MockBackend;

    fn proxy_with(local: bool, cloud: bool, threshold: usize, log: &str) -> Proxy {
        let engine = RoutingEngine::new(threshold, local, cloud);
        Proxy::new(
            engine,
            local.then(|| Box::new(MockBackend::new("local", "local-reply")) as Box<dyn Backend>),
            cloud.then(|| Box::new(MockBackend::new("cloud", "cloud-reply")) as Box<dyn Backend>),
            log,
        )
    }

    fn tmp_log() -> String {
        std::env::temp_dir()
            .join(format!(
                "pasture-proxy-{}.jsonl",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn test_parse_request_extracts_messages() {
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let req = Proxy::parse_request(body).unwrap();
        assert_eq!(req.model, "m");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content, "hi");
    }

    #[test]
    fn test_parse_request_detects_tools() {
        // IMP-10: a non-empty tools/functions array sets has_tools.
        let with_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        assert!(Proxy::parse_request(with_tools).unwrap().has_tools);
        let with_functions = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"functions":[{"name":"f"}]}"#;
        assert!(Proxy::parse_request(with_functions).unwrap().has_tools);
        let empty_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[]}"#;
        assert!(!Proxy::parse_request(empty_tools).unwrap().has_tools);
        let no_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        assert!(!Proxy::parse_request(no_tools).unwrap().has_tools);
    }

    #[test]
    fn test_handle_chat_with_tools_goes_cloud() {
        // IMP-10: even a short prompt routes to cloud when tools are present.
        let log = tmp_log();
        let p = proxy_with(true, true, 100, &log);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let resp = p.handle_chat(body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"cloud\""));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_build_models_response_shape() {
        // IMP-8: OpenAI-compatible list shape, de-duplicated and non-empty.
        let p = proxy_with(true, false, 100, "unused").with_models(vec![
            "llama3".into(),
            "llama3".into(),
            "".into(),
            "gpt-4o-mini".into(),
        ]);
        let body = build_models_response(&p.models);
        assert!(body.starts_with("{\"object\":\"list\",\"data\":["));
        assert!(body.contains("\"id\":\"llama3\""));
        assert!(body.contains("\"id\":\"gpt-4o-mini\""));
        assert!(body.contains("\"object\":\"model\""));
        // de-duplicated llama3, dropped empty -> exactly two entries.
        assert_eq!(body.matches("\"object\":\"model\"").count(), 2);
    }

    #[test]
    fn test_build_models_response_empty_is_valid() {
        let body = build_models_response(&[]);
        assert_eq!(body, "{\"object\":\"list\",\"data\":[]}");
    }

    #[test]
    fn test_build_error_response_envelope() {
        // SPEC §3.5: nested {"error":{"message","type"}}, escaped.
        let body = build_error_response("bad \"thing\"", "invalid_request_error");
        assert_eq!(
            body,
            "{\"error\":{\"message\":\"bad \\\"thing\\\"\",\"type\":\"invalid_request_error\"}}"
        );
    }

    #[test]
    fn test_response_includes_created() {
        let resp = CompletionResponse {
            content: "hi".into(),
            model: "m".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
        };
        assert!(build_openai_response(&resp, "local").contains("\"created\":"));
        assert!(build_openai_chunk("hi", "local", None).contains("\"created\":"));
    }

    /// Send `raw_request` to a one-shot server backed by `proxy`; return
    /// (status_code, body) of the HTTP response.
    fn roundtrip(proxy: Proxy, raw_request: String) -> (u16, String) {
        use std::net::{Shutdown, TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(raw_request.as_bytes()).unwrap();
            c.shutdown(Shutdown::Write).ok();
            let mut resp = String::new();
            c.read_to_string(&mut resp).unwrap();
            resp
        });
        let (mut server, _) = listener.accept().unwrap();
        proxy.handle_connection(&mut server).unwrap();
        drop(server);
        let resp = client.join().unwrap();
        let status = resp
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    fn http_post(path: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn test_roundtrip_chat_ok_has_created_and_route() {
        let log = tmp_log();
        let p = proxy_with(true, true, 100, &log);
        let req = http_post(
            "/v1/chat/completions",
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
        );
        let (status, body) = roundtrip(p, req);
        assert_eq!(status, 200);
        assert!(body.contains("\"created\":"), "{body}");
        assert!(body.contains("\"x_pasture_route\":\"local\""), "{body}");
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_roundtrip_bad_json_is_400_envelope() {
        let p = proxy_with(true, true, 100, "unused");
        let (status, body) = roundtrip(p, http_post("/v1/chat/completions", "{not json"));
        assert_eq!(status, 400);
        assert!(body.contains("\"error\":{"), "{body}");
        assert!(
            body.contains("\"type\":\"invalid_request_error\""),
            "{body}"
        );
    }

    #[test]
    fn test_roundtrip_oversized_body_is_413() {
        let p = proxy_with(true, true, 100, "unused");
        // Declare a Content-Length far beyond MAX_BODY_BYTES; no body sent.
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let (status, body) = roundtrip(p, req);
        assert_eq!(status, 413);
        assert!(
            body.contains("\"type\":\"invalid_request_error\""),
            "{body}"
        );
    }

    #[test]
    fn test_roundtrip_unknown_path_is_404_envelope() {
        let p = proxy_with(true, true, 100, "unused");
        let (status, body) = roundtrip(p, "GET /nope HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
        assert_eq!(status, 404);
        assert!(body.contains("\"error\":{"), "{body}");
    }

    #[test]
    fn test_parse_embeddings_string_and_array() {
        assert_eq!(
            Proxy::parse_embeddings_request(r#"{"input":"hello"}"#).unwrap(),
            vec!["hello".to_string()]
        );
        assert_eq!(
            Proxy::parse_embeddings_request(r#"{"input":["a","bb"]}"#).unwrap(),
            vec!["a".to_string(), "bb".to_string()]
        );
        assert!(Proxy::parse_embeddings_request(r#"{"model":"m"}"#).is_err());
        assert!(Proxy::parse_embeddings_request(r#"{"input":[1,2]}"#).is_err());
        assert!(Proxy::parse_embeddings_request(r#"{"input":[]}"#).is_err());
    }

    #[test]
    fn test_build_embeddings_response_shape() {
        let resp = EmbeddingsResponse {
            model: "m".into(),
            vectors: vec![vec![0.5, 1.0], vec![2.0, 3.0]],
            prompt_tokens: 4,
        };
        let body = build_embeddings_response(&resp);
        assert!(body.starts_with("{\"object\":\"list\",\"data\":["));
        assert!(body.contains("\"embedding\":[0.5,1]"), "{body}");
        assert!(body.contains("\"index\":1"), "{body}");
        assert!(body.contains("\"model\":\"m\""), "{body}");
        assert!(body.contains("\"total_tokens\":4"), "{body}");
    }

    #[test]
    fn test_handle_embeddings_local() {
        // Mock embeddings return [char_count, 0.0]; "hello"=5, "hi"=2.
        let p = proxy_with(true, false, 100, "unused");
        let body = p.handle_embeddings(r#"{"input":["hello","hi"]}"#).unwrap();
        assert!(body.contains("\"embedding\":[5,0]"), "{body}");
        assert!(body.contains("\"embedding\":[2,0]"), "{body}");
    }

    #[test]
    fn test_handle_embeddings_no_local_is_503() {
        let p = proxy_with(false, true, 100, "unused");
        assert_eq!(
            p.handle_embeddings(r#"{"input":"x"}"#)
                .unwrap_err()
                .status(),
            503
        );
    }

    #[test]
    fn test_roundtrip_embeddings_ok() {
        let p = proxy_with(true, true, 100, "unused");
        let (status, body) = roundtrip(p, http_post("/v1/embeddings", r#"{"input":"hello"}"#));
        assert_eq!(status, 200);
        assert!(body.contains("\"object\":\"embedding\""), "{body}");
    }

    /// Backend that fails with a transient error `fail_n` times, then succeeds.
    struct FlakyBackend {
        remaining: std::sync::atomic::AtomicU32,
        attempts: std::sync::atomic::AtomicU32,
        retryable: bool,
    }
    impl FlakyBackend {
        fn new(fail_n: u32, retryable: bool) -> Self {
            Self {
                remaining: std::sync::atomic::AtomicU32::new(fail_n),
                attempts: std::sync::atomic::AtomicU32::new(0),
                retryable,
            }
        }
    }
    impl Backend for FlakyBackend {
        fn name(&self) -> &str {
            "flaky"
        }
        fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
            use std::sync::atomic::Ordering;
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.remaining.load(Ordering::SeqCst) > 0 {
                self.remaining.fetch_sub(1, Ordering::SeqCst);
                return Err(if self.retryable {
                    BackendError::Transport("temporary".into())
                } else {
                    BackendError::Protocol("permanent".into())
                });
            }
            Ok(CompletionResponse {
                content: "cloud-ok".into(),
                model: "flaky".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
            })
        }
    }

    #[test]
    fn test_retry_succeeds_after_transient() {
        let b = FlakyBackend::new(2, true);
        let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
        let resp = complete_with_retry(&b, &req, 3, 0).unwrap();
        assert_eq!(resp.content, "cloud-ok");
        assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn test_retry_gives_up_after_limit() {
        let b = FlakyBackend::new(5, true);
        let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
        assert!(complete_with_retry(&b, &req, 2, 0).is_err());
        // 1 initial + 2 retries = 3 attempts.
        assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn test_retry_skips_non_retryable() {
        let b = FlakyBackend::new(1, false); // Protocol error -> not retryable
        let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
        assert!(complete_with_retry(&b, &req, 5, 0).is_err());
        assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn test_cloud_failure_falls_back_to_local() {
        // IMP-9: a persistently failing cloud falls back to the local answer.
        let log = tmp_log();
        let engine = RoutingEngine::new(100, true, true);
        let proxy = Proxy::new(
            engine,
            Some(Box::new(MockBackend::new("local", "local-reply"))),
            Some(Box::new(FlakyBackend::new(99, true))), // always fails (transient)
            &log,
        )
        .with_cloud_retry(0); // no backoff sleeps in the test
                              // tools force the cloud route (IMP-10); cloud fails -> fall back to local.
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let resp = proxy.handle_chat(body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"local\""));
        assert!(resp.contains("local-reply"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_parse_request_missing_messages_errors() {
        assert!(Proxy::parse_request(r#"{"model":"m"}"#).is_err());
    }

    #[test]
    fn test_parse_request_empty_messages_errors() {
        assert!(Proxy::parse_request(r#"{"messages":[]}"#).is_err());
    }

    #[test]
    fn test_handle_chat_short_goes_local() {
        let log = tmp_log();
        let p = proxy_with(true, true, 100, &log);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let resp = p.handle_chat(body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"local\""));
        assert!(resp.contains("local-reply"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_handle_chat_long_goes_cloud() {
        let log = tmp_log();
        let p = proxy_with(true, true, 5, &log);
        let long = "word ".repeat(50);
        let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{long}"}}]}}"#);
        let resp = p.handle_chat(&body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"cloud\""));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_handle_chat_sensitive_kept_local_despite_long() {
        let log = tmp_log();
        let p = proxy_with(true, true, 5, &log); // low threshold -> would be cloud
        let long = format!("contact me at user@example.com {}", "word ".repeat(50));
        let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{long}"}}]}}"#);
        let resp = p.handle_chat(&body).unwrap();
        assert!(
            resp.contains("\"x_pasture_route\":\"local\""),
            "sensitive content must stay local: {resp}"
        );
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_handle_chat_writes_cost_log() {
        let log = tmp_log();
        let p = proxy_with(true, true, 100, &log);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        p.handle_chat(body).unwrap();
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("\"route\":\"local\""));
        let _ = std::fs::remove_file(&log);
    }

    fn cascade_proxy(local_reply: &str, cloud_reply: &str, log: &str) -> Proxy {
        let engine = RoutingEngine::new(10_000, true, true); // high threshold -> local first
        Proxy::new(
            engine,
            Some(Box::new(MockBackend::new("local", local_reply))),
            Some(Box::new(MockBackend::new("cloud", cloud_reply))),
            log,
        )
        .with_cascade(true)
    }

    #[test]
    fn test_cascade_escalates_low_confidence() {
        let log = tmp_log();
        let p = cascade_proxy("I don't know", "the answer is 42", &log);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hard"}]}"#;
        let resp = p.handle_chat(body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"cloud\""), "{resp}");
        assert!(resp.contains("the answer is 42"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_cascade_keeps_confident_local() {
        let log = tmp_log();
        let p = cascade_proxy(
            "The capital of France is Paris.",
            "should not be used",
            &log,
        );
        let body = r#"{"model":"m","messages":[{"role":"user","content":"q"}]}"#;
        let resp = p.handle_chat(body).unwrap();
        assert!(resp.contains("\"x_pasture_route\":\"local\""), "{resp}");
        assert!(resp.contains("Paris"));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_cache_hit_returns_cache_route() {
        let log = tmp_log();
        let p = proxy_with(true, false, 100, &log).with_cache(8);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let first = p.handle_chat(body).unwrap();
        assert!(first.contains("\"x_pasture_route\":\"local\""), "{first}");
        let second = p.handle_chat(body).unwrap();
        assert!(second.contains("\"x_pasture_route\":\"cache\""), "{second}");
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_sensitive_not_cached() {
        let log = tmp_log();
        let p = proxy_with(true, false, 100, &log).with_cache(8);
        let body = r#"{"model":"m","messages":[{"role":"user","content":"my email a@b.com"}]}"#;
        let first = p.handle_chat(body).unwrap();
        let second = p.handle_chat(body).unwrap();
        assert!(first.contains("\"x_pasture_route\":\"local\""));
        assert!(second.contains("\"x_pasture_route\":\"local\""), "{second}");
        assert!(!second.contains("\"cache\""));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_build_openai_response_shape_is_valid_json() {
        let resp = CompletionResponse {
            content: "answer with \"quotes\"".to_string(),
            model: "m".to_string(),
            prompt_tokens: 3,
            completion_tokens: 2,
        };
        let json = build_openai_response(&resp, "cloud");
        let parsed = parse(&json).unwrap();
        assert_eq!(
            parsed.get("x_pasture_route").and_then(|r| r.as_str()),
            Some("cloud")
        );
        assert_eq!(
            parsed
                .get("usage")
                .and_then(|u| u.get("total_tokens"))
                .map(|t| matches!(t, JsonValue::Number(_))),
            Some(true)
        );
    }

    #[test]
    fn test_find_subslice_locates_header_break() {
        assert_eq!(find_subslice(b"ab\r\n\r\ncd", b"\r\n\r\n"), Some(2));
        assert_eq!(find_subslice(b"abc", b"\r\n\r\n"), None);
    }

    #[test]
    fn test_build_openai_chunk_delta_is_valid_json() {
        let json = build_openai_chunk("hel\"lo", "local", None);
        let v = parse(&json).unwrap();
        assert_eq!(
            v.get("object").and_then(|o| o.as_str()),
            Some("chat.completion.chunk")
        );
        assert_eq!(
            v.get("x_pasture_route").and_then(|r| r.as_str()),
            Some("local")
        );
        let delta = v
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|c| c.as_str());
        assert_eq!(delta, Some("hel\"lo"));
    }

    #[test]
    fn test_build_openai_chunk_finish_stop() {
        let json = build_openai_chunk("", "cloud", Some("stop"));
        assert!(json.contains("\"finish_reason\":\"stop\""));
        assert!(json.contains("\"delta\":{}"));
    }

    #[test]
    fn test_sse_frame_format() {
        assert_eq!(sse_frame("X"), "data: X\n\n");
    }
}

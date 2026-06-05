//! OpenAI-compatible routing proxy.
//!
//! `handle_chat` is pure with respect to the network (backends are injected),
//! so it is unit-tested with mock backends. `serve` adds a minimal blocking
//! HTTP/1.1 server loop over `std::net` (plain HTTP, localhost).

use crate::backend::{Backend, CompletionRequest, CompletionResponse, Message};
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
}

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
        }
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
        Ok(CompletionRequest {
            model,
            messages: parsed,
            stream,
        })
    }

    /// Classify, then decide routing (keeps sensitive content local).
    /// Returns the decision and whether the content was sensitive.
    fn classify_and_decide(&self, text: &str) -> Result<(Decision, bool), ProxyError> {
        let report = crate::privacy::classify(text);
        let sensitive = report.is_sensitive();
        if sensitive {
            eprintln!(
                "pasture: sensitive content detected -> keeping local ({} categories)",
                report.categories.len()
            );
        }
        let decision = self
            .engine
            .decide_with_sensitivity(text, None, sensitive)
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
        let (decision, sensitive) = self.classify_and_decide(&req.routing_text())?;

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
                match cloud.complete(req) {
                    Ok(cloud_resp) => (cloud_resp, Route::Cloud, confidence),
                    // Cloud failed: fall back to the local answer rather than error.
                    Err(_) => (local_resp, Route::Local, confidence),
                }
            } else {
                (local_resp, Route::Local, confidence)
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
            "pasture: listening on http://{addr} ({workers} workers, POST /v1/chat/completions)"
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
        let Some((method, path, body)) = read_request(stream)? else {
            write_response(stream, 400, "{\"error\":\"malformed request\"}")?;
            return Ok(());
        };
        if method == "POST" && path.starts_with("/v1/chat/completions") {
            match Self::parse_request(&body) {
                Ok(req) if req.stream => self.stream_chat_to_socket(stream, &req)?,
                Ok(req) => match self.complete_buffered(&req) {
                    Ok(resp) => write_response(stream, 200, &resp)?,
                    Err(e) => {
                        let payload = format!("{{\"error\":\"{}\"}}", escape_string(e.message()));
                        write_response(stream, e.status(), &payload)?;
                    }
                },
                Err(e) => {
                    let payload = format!("{{\"error\":\"{}\"}}", escape_string(e.message()));
                    write_response(stream, e.status(), &payload)?;
                }
            }
        } else if method == "GET" && path.starts_with("/health") {
            write_response(stream, 200, "{\"status\":\"ok\"}")?;
        } else {
            write_response(stream, 404, "{\"error\":\"not found\"}")?;
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
        let decision = match self.classify_and_decide(&req.routing_text()) {
            Ok((d, _)) => d,
            Err(e) => {
                let payload = format!("{{\"error\":\"{}\"}}", escape_string(e.message()));
                return write_response(sock, e.status(), &payload);
            }
        };
        let backend = match self.backend_for(decision.route) {
            Ok(b) => b,
            Err(e) => {
                let payload = format!("{{\"error\":\"{}\"}}", escape_string(e.message()));
                return write_response(sock, e.status(), &payload);
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
                let err = sse_frame(&format!(
                    "{{\"error\":\"{}\"}}",
                    escape_string(&e.to_string())
                ));
                sock.write_all(err.as_bytes())?;
            }
        }
        sock.write_all(b"data: [DONE]\n\n")
    }
}

/// Build an OpenAI-compatible chat-completion response JSON string.
pub fn build_openai_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    format!(
        "{{\"id\":\"pasture\",\"object\":\"chat.completion\",\"model\":\"{}\",\"x_pasture_route\":\"{}\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
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
        "{{\"id\":\"pasture\",\"object\":\"chat.completion.chunk\",\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{delta_field},\"finish_reason\":{finish_field}}}]}}"
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
fn read_request(
    stream: &mut std::net::TcpStream,
) -> std::io::Result<Option<(String, String, String)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until headers are complete.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 1_048_576 {
            return Ok(None); // 1 MiB header guard
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

    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);
    Ok(Some((
        method,
        path,
        String::from_utf8_lossy(&body).into_owned(),
    )))
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

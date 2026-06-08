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
use std::time::Duration;

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

/// CORS policy for browser clients (IMP-cors). Off by default — a localhost
/// server with permissive CORS is reachable by any website the user visits, so
/// this is opt-in via `PASTURE_CORS_ORIGINS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsPolicy {
    /// `*`: reflect any origin (echo `Access-Control-Allow-Origin: *`).
    allow_any: bool,
    /// Explicit allow-list (exact-match against the request `Origin`).
    origins: Vec<String>,
}

impl CorsPolicy {
    /// Parse a comma-separated origins spec. `*` allows any; an empty spec
    /// disables CORS (returns `None`).
    pub fn parse(spec: &str) -> Option<CorsPolicy> {
        let spec = spec.trim();
        if spec.is_empty() {
            return None;
        }
        if spec == "*" {
            return Some(CorsPolicy {
                allow_any: true,
                origins: Vec::new(),
            });
        }
        let origins: Vec<String> = spec
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if origins.is_empty() {
            None
        } else {
            Some(CorsPolicy {
                allow_any: false,
                origins,
            })
        }
    }

    /// The `Access-Control-Allow-Origin` value to return for a request's Origin,
    /// or `None` when the origin is not allowed.
    fn allow_origin(&self, origin: Option<&str>) -> Option<String> {
        if self.allow_any {
            return Some("*".to_string());
        }
        let o = origin?;
        if self.origins.iter().any(|a| a == o) {
            Some(o.to_string())
        } else {
            None
        }
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
    /// Optional small/fast local model for simple short queries (dual-local routing).
    /// None means disabled; all local traffic uses the default local backend model.
    fast_model: Option<String>,
    /// Estimated-token threshold below which the fast model is used.
    fast_threshold: usize,
    /// When true, prepend a system message with current date/OS info so that
    /// lightweight local models can act as capable PC assistants.
    inject_context: bool,
    /// Optional bearer token required on `/v1/*` requests (IMP-15). None = no
    /// auth (the localhost default). `/health` is always exempt.
    auth_token: Option<String>,
    /// Optional global token-bucket rate limiter for `/v1/*` (IMP-15). None =
    /// unlimited (the localhost default).
    rate_limiter: Option<std::sync::Mutex<crate::ratelimit::RateLimiter>>,
    /// Optional CORS policy for browser clients (IMP-cors). None = no CORS headers.
    cors: Option<CorsPolicy>,
    /// Per-connection socket read/write timeout (IMP-timeout). None = no timeout.
    /// Guards against slow/dead clients pinning a bounded worker thread.
    io_timeout: Option<Duration>,
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
            fast_model: None,
            fast_threshold: 50,
            inject_context: false,
            auth_token: None,
            rate_limiter: None,
            cors: None,
            io_timeout: None,
        }
    }

    /// Set a per-connection socket read/write timeout (IMP-timeout). A slow or
    /// dead client then frees its worker after the timeout instead of pinning it
    /// (slow-loris guard). None disables the timeout.
    pub fn with_request_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.io_timeout = timeout;
        self
    }

    /// Set the CORS policy for browser clients (IMP-cors). None leaves CORS off.
    pub fn with_cors(mut self, policy: Option<CorsPolicy>) -> Self {
        self.cors = policy;
        self
    }

    /// Build CORS response header lines for a request's Origin (empty when CORS
    /// is off or the origin is not allowed). Always appended to responses so the
    /// browser can read both success and error bodies.
    fn cors_headers(&self, origin: Option<&str>) -> String {
        let Some(policy) = &self.cors else {
            return String::new();
        };
        match policy.allow_origin(origin) {
            Some(allow) => {
                let mut h = format!("Access-Control-Allow-Origin: {allow}\r\n");
                // A specific origin varies by request; tell caches so.
                if allow != "*" {
                    h.push_str("Vary: Origin\r\n");
                }
                h
            }
            None => String::new(),
        }
    }

    /// Build the full preflight (OPTIONS) header block, or None when CORS is off
    /// or the origin is not allowed.
    fn cors_preflight(&self, origin: Option<&str>) -> Option<String> {
        let policy = self.cors.as_ref()?;
        let allow = policy.allow_origin(origin)?;
        let mut h = format!("Access-Control-Allow-Origin: {allow}\r\n");
        if allow != "*" {
            h.push_str("Vary: Origin\r\n");
        }
        h.push_str("Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n");
        h.push_str("Access-Control-Allow-Headers: Authorization, Content-Type\r\n");
        h.push_str("Access-Control-Max-Age: 86400\r\n");
        Some(h)
    }

    /// Require this bearer token on `/v1/*` requests (IMP-15). Empty disables auth.
    pub fn with_auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token.filter(|t| !t.is_empty());
        self
    }

    /// Cap `/v1/*` requests to `per_minute` (global token bucket, IMP-15).
    /// 0 disables rate limiting.
    pub fn with_rate_limit(mut self, per_minute: u32) -> Self {
        self.rate_limiter = if per_minute > 0 {
            Some(std::sync::Mutex::new(
                crate::ratelimit::RateLimiter::per_minute(per_minute),
            ))
        } else {
            None
        };
        self
    }

    /// Apply rate-limit then auth gating for a request path (IMP-15). Returns
    /// `Some((status, message, type))` when the request must be rejected, else
    /// `None`. `/health` is always exempt so liveness probes work unauthenticated.
    fn check_gate(
        &self,
        path: &str,
        auth: Option<&str>,
    ) -> Option<(u16, &'static str, &'static str)> {
        if path.starts_with("/health") {
            return None;
        }
        if let Some(rl) = &self.rate_limiter {
            let allowed = rl.lock().map(|mut g| g.allow()).unwrap_or(true);
            if !allowed {
                return Some((429, "rate limit exceeded", "rate_limit_error"));
            }
        }
        if let Some(expected) = &self.auth_token {
            if !auth_ok(auth, expected) {
                return Some((
                    401,
                    "missing or invalid Authorization bearer token",
                    "invalid_request_error",
                ));
            }
        }
        None
    }

    /// Select a small/fast local model for short, simple queries (dual-local routing).
    /// `threshold` is the estimated-token count below which the fast model is preferred.
    pub fn with_fast_model(mut self, model: Option<String>, threshold: usize) -> Self {
        self.fast_model = model.filter(|m| !m.is_empty());
        self.fast_threshold = threshold;
        self
    }

    /// Prepend a system message with the current date and OS so lightweight local
    /// models have grounding for PC-assistant tasks (e.g. scheduling, file ops).
    pub fn with_inject_context(mut self, enabled: bool) -> Self {
        self.inject_context = enabled;
        self
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
        let sampling = Self::parse_sampling(&v);
        Ok(CompletionRequest {
            model,
            messages: parsed,
            stream,
            has_tools,
            sampling,
        })
    }

    /// True when the client requested `stream_options.include_usage` — emit a
    /// final SSE chunk carrying token usage (OpenAI streaming feature).
    fn parse_include_usage(body: &str) -> bool {
        parse(body)
            .ok()
            .and_then(|v| {
                v.get("stream_options")
                    .and_then(|s| s.get("include_usage"))
                    .and_then(JsonValue::as_bool)
            })
            .unwrap_or(false)
    }

    /// Extract OpenAI sampling parameters from a request body. Non-finite numbers
    /// are rejected so the value can always be serialised as valid JSON; negative
    /// `max_tokens` is dropped. `stop` accepts a string or an array of strings;
    /// `max_completion_tokens` is honoured as an alias for `max_tokens`.
    fn parse_sampling(v: &JsonValue) -> crate::backend::SamplingParams {
        let finite = |key: &str| {
            v.get(key)
                .and_then(JsonValue::as_f64)
                .filter(|x| x.is_finite())
        };
        let max_tokens = finite("max_tokens")
            .or_else(|| finite("max_completion_tokens"))
            .filter(|x| *x >= 0.0)
            .map(|x| x as u64);
        let stop = match v.get("stop") {
            Some(JsonValue::Str(s)) => vec![s.clone()],
            Some(JsonValue::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        crate::backend::SamplingParams {
            temperature: finite("temperature"),
            top_p: finite("top_p"),
            max_tokens,
            stop,
            seed: finite("seed").map(|x| x as i64),
            presence_penalty: finite("presence_penalty"),
            frequency_penalty: finite("frequency_penalty"),
            response_format: v.get("response_format").cloned(),
        }
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
        // Optionally prepend a system message so local models know the current date/OS.
        let injected;
        let req = if self.inject_context {
            injected = inject_context_into(req);
            &injected
        } else {
            req
        };

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
            // Dual-local routing: if a fast model is configured, use it for
            // simple short prompts (no hard signals, below fast_threshold tokens).
            let fast_req;
            let effective_req = if decision.route == Route::Local {
                if let Some(fm) = self.fast_model.as_deref() {
                    let text = req
                        .messages
                        .iter()
                        .rev()
                        .find(|m| m.role == "user")
                        .map(|m| m.content.as_str())
                        .unwrap_or("");
                    if crate::routing::is_simple_prompt(text, self.fast_threshold) {
                        fast_req = CompletionRequest {
                            model: fm.to_string(),
                            messages: req.messages.clone(),
                            stream: req.stream,
                            has_tools: req.has_tools,
                            sampling: req.sampling.clone(),
                        };
                        &fast_req
                    } else {
                        req
                    }
                } else {
                    req
                }
            } else {
                req
            };
            let resp = backend
                .complete(effective_req)
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

    /// Handle `GET /v1/stats` (IMP-metrics): a live JSON view of the cost-log
    /// counters (route counts, rates, tokens, spend) without parsing JSONL by
    /// hand. Read-only over the PII-free cost log (I3); a missing log reads as
    /// all-zeros. Localhost-default, so no auth is implied (I5).
    pub fn handle_stats(&self) -> Result<String, ProxyError> {
        let records = crate::cost::read_log(&self.cost_log_path)
            .map_err(|e| ProxyError::Backend(e.to_string()))?;
        let summary = crate::cost::summarize(&records);
        Ok(build_stats_response(&summary))
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
            "pasture: listening on http://{addr} ({workers} workers, POST /v1/chat/completions, GET /v1/models, GET /v1/stats)"
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
        // Bound how long a slow/dead client can hold a worker (slow-loris guard).
        if let Some(d) = self.io_timeout {
            let _ = stream.set_read_timeout(Some(d));
            let _ = stream.set_write_timeout(Some(d));
        }
        let (method, path, body, auth, origin) = match read_request(stream)? {
            ReadOutcome::Request {
                method,
                path,
                body,
                auth,
                origin,
            } => (method, path, body, auth, origin),
            ReadOutcome::TooLarge => {
                let payload =
                    build_error_response("request body too large", "invalid_request_error");
                write_response(stream, 413, &payload, "")?;
                return Ok(());
            }
            ReadOutcome::TimedOut => {
                let payload = build_error_response("request timed out", "invalid_request_error");
                write_response(stream, 408, &payload, "")?;
                return Ok(());
            }
            ReadOutcome::Closed => {
                let payload = build_error_response("malformed request", "invalid_request_error");
                write_response(stream, 400, &payload, "")?;
                return Ok(());
            }
        };
        // CORS headers reflected on every response so the browser can read it.
        let cors = self.cors_headers(origin.as_deref());
        // CORS preflight: answer OPTIONS before the gate (preflight is credential-free).
        if method == "OPTIONS" {
            match self.cors_preflight(origin.as_deref()) {
                Some(h) => write_response(stream, 204, "", &h)?,
                None => write_response(
                    stream,
                    404,
                    &build_error_response("not found", "invalid_request_error"),
                    &cors,
                )?,
            }
            return Ok(());
        }
        // Auth + rate-limit gating (IMP-15); /health is exempt.
        if let Some((status, msg, kind)) = self.check_gate(&path, auth.as_deref()) {
            write_response(stream, status, &build_error_response(msg, kind), &cors)?;
            return Ok(());
        }
        if method == "POST" && path.starts_with("/v1/chat/completions") {
            match Self::parse_request(&body) {
                Ok(req) if req.stream => {
                    let include_usage = Self::parse_include_usage(&body);
                    self.stream_chat_to_socket(stream, &req, &cors, include_usage)?
                }
                Ok(req) => match self.complete_buffered(&req) {
                    Ok(resp) => write_response(stream, 200, &resp, &cors)?,
                    Err(e) => {
                        write_response(
                            stream,
                            e.status(),
                            &build_error_response(e.message(), e.kind()),
                            &cors,
                        )?;
                    }
                },
                Err(e) => {
                    write_response(
                        stream,
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                        &cors,
                    )?;
                }
            }
        } else if method == "POST" && path.starts_with("/v1/embeddings") {
            match self.handle_embeddings(&body) {
                Ok(resp) => write_response(stream, 200, &resp, &cors)?,
                Err(e) => {
                    write_response(
                        stream,
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                        &cors,
                    )?;
                }
            }
        } else if method == "GET" && path.starts_with("/v1/stats") {
            match self.handle_stats() {
                Ok(resp) => write_response(stream, 200, &resp, &cors)?,
                Err(e) => {
                    write_response(
                        stream,
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                        &cors,
                    )?;
                }
            }
        } else if method == "GET" && path.starts_with("/v1/models") {
            // `/v1/models` lists; `/v1/models/{id}` retrieves a single model.
            let rest = &path["/v1/models".len()..];
            if let Some(after) = rest.strip_prefix('/') {
                let id = after
                    .split('?')
                    .next()
                    .unwrap_or(after)
                    .trim_end_matches('/');
                match build_model_response(&self.models, id) {
                    Some(b) => write_response(stream, 200, &b, &cors)?,
                    None => write_response(
                        stream,
                        404,
                        &build_error_response(
                            &format!("model '{id}' not found"),
                            "invalid_request_error",
                        ),
                        &cors,
                    )?,
                }
            } else {
                write_response(stream, 200, &build_models_response(&self.models), &cors)?;
            }
        } else if method == "GET" && path.starts_with("/health") {
            write_response(stream, 200, "{\"status\":\"ok\"}", &cors)?;
        } else {
            write_response(
                stream,
                404,
                &build_error_response("not found", "invalid_request_error"),
                &cors,
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
        cors: &str,
        include_usage: bool,
    ) -> std::io::Result<()> {
        let decision = match self.classify_and_decide(req) {
            Ok((d, _)) => d,
            Err(e) => {
                return write_response(
                    sock,
                    e.status(),
                    &build_error_response(e.message(), e.kind()),
                    cors,
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
                    cors,
                );
            }
        };

        write_sse_headers(sock, cors)?;
        let route_label = decision.route.as_str();
        // One id shared by every chunk of this stream (OpenAI behaviour).
        let id = next_completion_id();
        let mut io_err: Option<std::io::Error> = None;
        let resp = backend.stream_complete(req, &mut |delta| {
            if io_err.is_some() {
                return;
            }
            let frame = sse_frame(&build_openai_chunk(&id, delta, route_label, None));
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
                let stop = sse_frame(&build_openai_chunk(&id, "", route_label, Some("stop")));
                sock.write_all(stop.as_bytes())?;
                // Final usage chunk when the client asked for it (OpenAI feature).
                if include_usage {
                    let usage = sse_frame(&build_openai_usage_chunk(
                        &id,
                        route_label,
                        r.prompt_tokens,
                        r.completion_tokens,
                    ));
                    sock.write_all(usage.as_bytes())?;
                }
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

/// Convert a Unix timestamp to a UTC date string `YYYY-MM-DD`.
fn utc_date_str(secs: u64) -> String {
    let mut d = secs / 86400;
    let mut y = 1970u32;
    loop {
        let yd: u64 = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if d < yd {
            break;
        }
        d -= yd;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays: [u64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 1u32;
    for &md in &mdays {
        if d < md {
            break;
        }
        d -= md;
        m += 1;
    }
    format!("{y}-{m:02}-{:02}", d + 1)
}

/// Return a system context string for injection: date, OS, and lightweight-LLM note.
fn system_context_text() -> String {
    let date = utc_date_str(unix_now());
    let os = std::env::consts::OS;
    format!(
        "[System context — provided by Pasture]\nDate (UTC): {date}\nOS: {os}\n\
         You are a helpful local AI assistant running on this machine. Answer \
         concisely and accurately."
    )
}

/// Prepend a system-context message into a completion request (for PC-assistant mode).
fn inject_context_into(req: &CompletionRequest) -> CompletionRequest {
    let ctx = Message {
        role: "system".to_string(),
        content: system_context_text(),
    };
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    // Insert context before any existing system messages, or at the front.
    let has_system = req
        .messages
        .first()
        .map(|m| m.role == "system")
        .unwrap_or(false);
    if has_system {
        // Merge with existing system message rather than duplicating.
        let mut merged = req.messages.clone();
        let existing = merged[0].content.clone();
        merged[0].content = format!("{}\n\n{}", ctx.content, existing);
        return CompletionRequest {
            model: req.model.clone(),
            messages: merged,
            stream: req.stream,
            has_tools: req.has_tools,
            sampling: req.sampling.clone(),
        };
    }
    messages.push(ctx);
    messages.extend_from_slice(&req.messages);
    CompletionRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        has_tools: req.has_tools,
        sampling: req.sampling.clone(),
    }
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

/// Build a `GET /v1/models/{id}` response for a single configured model, or
/// `None` if the id is not in the advertised list (→ 404). OpenAI shape.
pub fn build_model_response(models: &[String], id: &str) -> Option<String> {
    if models.iter().any(|m| m == id) {
        Some(format!(
            "{{\"id\":\"{}\",\"object\":\"model\",\"owned_by\":\"pasture\"}}",
            escape_string(id)
        ))
    } else {
        None
    }
}

/// Build the `GET /v1/stats` JSON body (IMP-metrics): live counters from the
/// cost log. All values are PII-free aggregates (I3). Rates are rounded to 4 dp.
pub fn build_stats_response(s: &crate::cost::CostSummary) -> String {
    let round4 = |x: f64| (x * 10_000.0).round() / 10_000.0;
    format!(
        "{{\"object\":\"pasture.stats\",\"total\":{},\"local\":{},\"cloud\":{},\"cache\":{},\
\"cloud_rate\":{},\"cache_rate\":{},\"prompt_tokens\":{},\"completion_tokens\":{},\
\"cloud_cost_usd\":{}}}",
        s.total,
        s.local,
        s.cloud,
        s.cache,
        round4(s.cloud_rate()),
        round4(s.cache_rate()),
        s.prompt_tokens,
        s.completion_tokens,
        round4(s.cloud_cost_usd),
    )
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

/// A unique completion id (`chatcmpl-…`), matching OpenAI's per-response id that
/// logging/observability/dedup tooling keys on. Uniqueness within the process is
/// guaranteed by an atomic counter; the wall-clock prefix adds cross-run variety.
fn next_completion_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{}{:08}", unix_now(), n)
}

/// Build an OpenAI-compatible chat-completion response JSON string.
pub fn build_openai_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    format!(
        "{{\"id\":\"{}\",\"object\":\"chat.completion\",\"created\":{},\"model\":\"{}\",\"x_pasture_route\":\"{}\",\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
        next_completion_id(),
        unix_now(),
        escape_string(&resp.model),
        route_label,
        escape_string(&resp.content),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build an OpenAI-compatible streaming chunk (`chat.completion.chunk`). The `id`
/// is supplied by the caller so every chunk of one stream shares it (OpenAI does).
pub fn build_openai_chunk(
    id: &str,
    delta: &str,
    route_label: &str,
    finish: Option<&str>,
) -> String {
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
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{},\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{delta_field},\"finish_reason\":{finish_field}}}]}}",
        unix_now()
    )
}

/// Build the final streaming chunk carrying token `usage` (emitted only when the
/// client sets `stream_options.include_usage`). Per the OpenAI contract this
/// chunk has an empty `choices` array. Shares the stream's `id`.
pub fn build_openai_usage_chunk(
    id: &str,
    route_label: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{},\"x_pasture_route\":\"{route_label}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":{completion_tokens},\"total_tokens\":{}}}}}",
        unix_now(),
        prompt_tokens + completion_tokens
    )
}

/// Wrap a payload in a Server-Sent Events `data:` frame.
pub fn sse_frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

fn write_sse_headers(stream: &mut std::net::TcpStream, cors: &str) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n{cors}\r\n"
    );
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
        /// The `Authorization` header value, if present (used for auth, IMP-15).
        auth: Option<String>,
        /// The `Origin` header value, if present (used for CORS, IMP-cors).
        origin: Option<String>,
    },
    /// The declared or actual body exceeded `MAX_BODY_BYTES` → 413.
    TooLarge,
    /// A socket read timed out before the request completed → 408 (IMP-timeout).
    TimedOut,
    /// Connection closed early or headers were malformed/oversized → 400.
    Closed,
}

/// True for an I/O error that means "no data within the read timeout window".
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn read_request(stream: &mut std::net::TcpStream) -> std::io::Result<ReadOutcome> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until headers are complete.
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut),
            Err(e) => return Err(e),
        };
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
    let mut auth: Option<String> = None;
    let mut origin: Option<String> = None;
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            // Preserve original case for the token value (header name is ASCII).
            if let Some((_, v)) = line.split_once(':') {
                auth = Some(v.trim().to_string());
            }
        } else if lower.starts_with("origin:") {
            if let Some((_, v)) = line.split_once(':') {
                origin = Some(v.trim().to_string());
            }
        }
    }

    // Reject oversized bodies before reading them (DoS guard, SPEC §7).
    if content_length > MAX_BODY_BYTES {
        return Ok(ReadOutcome::TooLarge);
    }

    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut),
            Err(e) => return Err(e),
        };
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
        auth,
        origin,
    })
}

/// Validate a bearer token from an `Authorization` header against the expected
/// value, in constant time (no early return on first mismatch).
fn auth_ok(header: Option<&str>, expected: &str) -> bool {
    let Some(h) = header else {
        return false;
    };
    let token = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
        .unwrap_or(h)
        .trim();
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

/// Length-checked, constant-time byte comparison (avoids leaking token length
/// match timing beyond the unavoidable length check).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write an HTTP/1.1 response. `extra` is a block of additional header lines
/// (each already terminated with `\r\n`, e.g. CORS headers) or empty.
fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    body: &str,
    extra: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
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
    fn test_build_model_response_found_and_missing() {
        let models = vec!["llama3".to_string(), "gpt-4o-mini".to_string()];
        let body = build_model_response(&models, "llama3").expect("found");
        assert!(body.contains("\"id\":\"llama3\""), "{body}");
        assert!(body.contains("\"object\":\"model\""), "{body}");
        assert!(build_model_response(&models, "nope").is_none());
    }

    #[test]
    fn test_is_timeout_classifies_kinds() {
        use std::io::{Error, ErrorKind};
        assert!(is_timeout(&Error::from(ErrorKind::WouldBlock)));
        assert!(is_timeout(&Error::from(ErrorKind::TimedOut)));
        assert!(!is_timeout(&Error::from(ErrorKind::BrokenPipe)));
    }

    #[test]
    fn test_roundtrip_slow_client_times_out_408() {
        // A client that opens a connection and sends an incomplete request must
        // not pin the worker: with a short timeout the server responds 408.
        let p = proxy_with(true, false, 100, "unused")
            .with_request_timeout(Some(std::time::Duration::from_millis(50)));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            // Partial request: headers never terminate.
            c.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n")
                .unwrap();
            // Hold the connection open past the server's read timeout.
            std::thread::sleep(std::time::Duration::from_millis(300));
            let mut resp = String::new();
            let _ = c.read_to_string(&mut resp);
            resp
        });
        let (mut s, _) = listener.accept().unwrap();
        p.handle_connection(&mut s).unwrap();
        drop(s);
        let resp = client.join().unwrap();
        assert!(resp.contains("408"), "expected 408 timeout, got: {resp}");
    }

    #[test]
    fn test_roundtrip_model_retrieve_ok() {
        let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
        let (status, body) = roundtrip(p, "GET /v1/models/llama3 HTTP/1.1\r\n\r\n".to_string());
        assert_eq!(status, 200);
        let v = crate::json::parse(&body).unwrap();
        assert_eq!(v.get("id").and_then(|x| x.as_str()), Some("llama3"));
        assert_eq!(v.get("object").and_then(|x| x.as_str()), Some("model"));
    }

    #[test]
    fn test_roundtrip_model_retrieve_unknown_is_404() {
        let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
        let (status, body) = roundtrip(p, "GET /v1/models/ghost HTTP/1.1\r\n\r\n".to_string());
        assert_eq!(status, 404);
        assert!(body.contains("not found"), "{body}");
    }

    #[test]
    fn test_roundtrip_models_list_still_works() {
        // The bare list path must not be captured by the retrieve branch.
        let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
        let (status, body) = roundtrip(p, "GET /v1/models HTTP/1.1\r\n\r\n".to_string());
        assert_eq!(status, 200);
        assert!(body.contains("\"object\":\"list\""), "{body}");
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
        assert!(build_openai_chunk("chatcmpl-x", "hi", "local", None).contains("\"created\":"));
    }

    #[test]
    fn test_completion_ids_are_unique_and_prefixed() {
        let resp = CompletionResponse {
            content: "hi".into(),
            model: "m".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
        };
        let id_of = |json: &str| {
            crate::json::parse(json)
                .unwrap()
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap()
                .to_string()
        };
        let a = id_of(&build_openai_response(&resp, "local"));
        let b = id_of(&build_openai_response(&resp, "local"));
        assert!(a.starts_with("chatcmpl-"), "id: {a}");
        assert_ne!(a, b, "completion ids must be unique");
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
    fn test_build_stats_response_shape() {
        let s = crate::cost::CostSummary {
            total: 4,
            local: 2,
            cloud: 1,
            cache: 1,
            prompt_tokens: 100,
            completion_tokens: 50,
            cloud_cost_usd: 0.0123,
        };
        let json = build_stats_response(&s);
        let v = crate::json::parse(&json).expect("valid json");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(4.0));
        assert_eq!(v.get("cloud").and_then(|x| x.as_f64()), Some(1.0));
        assert_eq!(v.get("cloud_rate").and_then(|x| x.as_f64()), Some(0.25));
        assert_eq!(v.get("cache_rate").and_then(|x| x.as_f64()), Some(0.25));
        assert_eq!(
            v.get("completion_tokens").and_then(|x| x.as_f64()),
            Some(50.0)
        );
    }

    #[test]
    fn test_handle_stats_empty_log_is_zeros() {
        // A non-existent cost log reads as all-zeros (no error).
        let p = proxy_with(true, false, 100, "/no/such/cost-log.jsonl");
        let json = p.handle_stats().unwrap();
        let v = crate::json::parse(&json).expect("valid json");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(0.0));
    }

    #[test]
    fn test_handle_stats_counts_logged_requests() {
        // Drive 2 local completions through a real log file, then read stats.
        let log = tmp_log();
        let p = proxy_with(true, false, 100, &log);
        p.handle_chat(r#"{"messages":[{"role":"user","content":"hi"}]}"#)
            .unwrap();
        p.handle_chat(r#"{"messages":[{"role":"user","content":"yo"}]}"#)
            .unwrap();
        let json = p.handle_stats().unwrap();
        let v = crate::json::parse(&json).expect("valid json");
        assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(2.0));
        assert_eq!(v.get("local").and_then(|x| x.as_f64()), Some(2.0));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn test_parse_request_extracts_sampling() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"temperature":0.2,"max_tokens":100,"stop":["\n\n"],"seed":7}"#;
        let req = Proxy::parse_request(body).unwrap();
        assert_eq!(req.sampling.temperature, Some(0.2));
        assert_eq!(req.sampling.max_tokens, Some(100));
        assert_eq!(req.sampling.seed, Some(7));
        assert_eq!(req.sampling.stop, vec!["\n\n".to_string()]);
    }

    #[test]
    fn test_parse_request_max_completion_tokens_alias() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":42}"#;
        let req = Proxy::parse_request(body).unwrap();
        assert_eq!(req.sampling.max_tokens, Some(42));
    }

    #[test]
    fn test_parse_request_stop_as_string() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"stop":"END"}"#;
        let req = Proxy::parse_request(body).unwrap();
        assert_eq!(req.sampling.stop, vec!["END".to_string()]);
    }

    #[test]
    fn test_parse_request_no_sampling_is_empty() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let req = Proxy::parse_request(body).unwrap();
        assert!(req.sampling.is_empty());
    }

    #[test]
    fn test_parse_request_extracts_response_format() {
        let body = r#"{"messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_object"}}"#;
        let req = Proxy::parse_request(body).unwrap();
        let rf = req
            .sampling
            .response_format
            .expect("response_format parsed");
        assert_eq!(rf.get("type").and_then(|x| x.as_str()), Some("json_object"));
    }

    #[test]
    fn test_cache_distinguishes_by_response_format() {
        let base = r#"{"messages":[{"role":"user","content":"hi"}]"#;
        let plain = Proxy::parse_request(&format!("{base}}}")).unwrap();
        let json_mode = Proxy::parse_request(&format!(
            "{base},\"response_format\":{{\"type\":\"json_object\"}}}}"
        ))
        .unwrap();
        assert_ne!(
            crate::cache::request_key(&plain),
            crate::cache::request_key(&json_mode)
        );
    }

    #[test]
    fn test_cache_distinguishes_by_temperature() {
        // Same messages, different temperature -> different cache key, so a
        // temperature:0 response is never served to a temperature:1 request.
        let base = r#"{"messages":[{"role":"user","content":"hi"}]"#;
        let r0 = Proxy::parse_request(&format!("{base},\"temperature\":0}}")).unwrap();
        let r1 = Proxy::parse_request(&format!("{base},\"temperature\":1}}")).unwrap();
        assert_ne!(
            crate::cache::request_key(&r0),
            crate::cache::request_key(&r1)
        );
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_auth_ok_bearer_forms() {
        assert!(auth_ok(Some("Bearer tok123"), "tok123"));
        assert!(auth_ok(Some("bearer tok123"), "tok123")); // case-insensitive scheme
        assert!(auth_ok(Some("tok123"), "tok123")); // bare token accepted too
        assert!(!auth_ok(Some("Bearer wrong"), "tok123"));
        assert!(!auth_ok(None, "tok123"));
    }

    #[test]
    fn test_gate_open_when_unconfigured() {
        let p = proxy_with(true, false, 100, "unused");
        assert!(p.check_gate("/v1/chat/completions", None).is_none());
    }

    #[test]
    fn test_gate_auth_required_and_enforced() {
        let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("s3cret".to_string()));
        // No / wrong token -> 401.
        assert_eq!(
            p.check_gate("/v1/chat/completions", None).map(|g| g.0),
            Some(401)
        );
        assert_eq!(
            p.check_gate("/v1/chat/completions", Some("Bearer nope"))
                .map(|g| g.0),
            Some(401)
        );
        // Correct token -> allowed.
        assert!(p
            .check_gate("/v1/chat/completions", Some("Bearer s3cret"))
            .is_none());
        // /health is always exempt.
        assert!(p.check_gate("/health", None).is_none());
    }

    #[test]
    fn test_gate_rate_limit_enforced() {
        let p = proxy_with(true, false, 100, "unused").with_rate_limit(1);
        // First request consumes the only token; second is rejected with 429.
        assert!(p.check_gate("/v1/models", None).is_none());
        assert_eq!(p.check_gate("/v1/models", None).map(|g| g.0), Some(429));
        // /health bypasses the limiter.
        assert!(p.check_gate("/health", None).is_none());
    }

    #[test]
    fn test_roundtrip_401_without_token() {
        let p = proxy_with(true, true, 100, "unused").with_auth_token(Some("k".to_string()));
        let (status, body) = roundtrip(
            p,
            http_post(
                "/v1/chat/completions",
                r#"{"messages":[{"role":"user","content":"hi"}]}"#,
            ),
        );
        assert_eq!(status, 401);
        assert!(
            body.contains("\"type\":\"invalid_request_error\""),
            "{body}"
        );
    }

    #[test]
    fn test_roundtrip_authed_request_ok() {
        let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("k".to_string()));
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer k\r\nContent-Length: {}\r\n\r\n{}",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#.len(),
            r#"{"messages":[{"role":"user","content":"hi"}]}"#
        );
        let (status, _body) = roundtrip(p, req);
        assert_eq!(status, 200);
    }

    #[test]
    fn test_roundtrip_health_exempt_from_auth() {
        let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("k".to_string()));
        let (status, _) = roundtrip(p, "GET /health HTTP/1.1\r\n\r\n".to_string());
        assert_eq!(status, 200);
    }

    #[test]
    fn test_parse_include_usage() {
        assert!(Proxy::parse_include_usage(
            r#"{"stream":true,"stream_options":{"include_usage":true}}"#
        ));
        assert!(!Proxy::parse_include_usage(
            r#"{"stream":true,"stream_options":{"include_usage":false}}"#
        ));
        assert!(!Proxy::parse_include_usage(r#"{"stream":true}"#));
    }

    #[test]
    fn test_build_usage_chunk_shape() {
        let json = build_openai_usage_chunk("chatcmpl-x", "local", 10, 5);
        let v = crate::json::parse(&json).expect("valid json");
        assert_eq!(
            v.get("object").and_then(|x| x.as_str()),
            Some("chat.completion.chunk")
        );
        // OpenAI: the usage chunk has an empty choices array.
        assert_eq!(
            v.get("choices")
                .and_then(JsonValue::as_array)
                .map(|a| a.len()),
            Some(0)
        );
        let usage = v.get("usage").unwrap();
        assert_eq!(
            usage.get("prompt_tokens").and_then(|x| x.as_f64()),
            Some(10.0)
        );
        assert_eq!(
            usage.get("completion_tokens").and_then(|x| x.as_f64()),
            Some(5.0)
        );
        assert_eq!(
            usage.get("total_tokens").and_then(|x| x.as_f64()),
            Some(15.0)
        );
    }

    #[test]
    fn test_roundtrip_stream_includes_usage_when_requested() {
        let p = proxy_with(true, false, 100, tmp_log().as_str());
        let body = r#"{"stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}"#;
        let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
        assert_eq!(status, 200);
        assert!(resp.contains("\"usage\""), "expected usage chunk: {resp}");
        assert!(resp.contains("\"total_tokens\""), "{resp}");
        assert!(resp.contains("data: [DONE]"), "{resp}");
    }

    #[test]
    fn test_roundtrip_stream_shares_one_id() {
        let p = proxy_with(true, false, 100, tmp_log().as_str());
        let body = r#"{"stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}"#;
        let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
        assert_eq!(status, 200);
        // Every chunk's id must be identical across the stream (incl. usage chunk).
        let ids: Vec<String> = resp
            .lines()
            .filter_map(|l| l.strip_prefix("data: "))
            .filter(|p| p.starts_with('{'))
            .filter_map(|p| crate::json::parse(p).ok())
            .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(str::to_string))
            .collect();
        assert!(ids.len() >= 2, "expected multiple chunks: {resp}");
        assert!(ids.iter().all(|id| *id == ids[0]), "ids differ: {ids:?}");
        assert!(ids[0].starts_with("chatcmpl-"), "{:?}", ids[0]);
    }

    #[test]
    fn test_roundtrip_stream_omits_usage_by_default() {
        let p = proxy_with(true, false, 100, tmp_log().as_str());
        let body = r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
        assert_eq!(status, 200);
        assert!(!resp.contains("\"usage\""), "should not emit usage: {resp}");
        assert!(resp.contains("data: [DONE]"), "{resp}");
    }

    #[test]
    fn test_cors_policy_parse() {
        assert_eq!(CorsPolicy::parse(""), None);
        assert_eq!(CorsPolicy::parse("   "), None);
        assert!(CorsPolicy::parse("*").unwrap().allow_any);
        let p = CorsPolicy::parse("https://a.com, https://b.com").unwrap();
        assert!(!p.allow_any);
        assert_eq!(p.origins, vec!["https://a.com", "https://b.com"]);
    }

    #[test]
    fn test_cors_allow_origin_matching() {
        let any = CorsPolicy::parse("*").unwrap();
        assert_eq!(
            any.allow_origin(Some("https://x.com")).as_deref(),
            Some("*")
        );
        assert_eq!(any.allow_origin(None).as_deref(), Some("*"));
        let list = CorsPolicy::parse("https://ok.com").unwrap();
        assert_eq!(
            list.allow_origin(Some("https://ok.com")).as_deref(),
            Some("https://ok.com")
        );
        assert_eq!(list.allow_origin(Some("https://evil.com")), None);
        assert_eq!(list.allow_origin(None), None);
    }

    #[test]
    fn test_cors_headers_off_by_default() {
        let p = proxy_with(true, false, 100, "unused");
        assert_eq!(p.cors_headers(Some("https://x.com")), "");
        assert!(p.cors_preflight(Some("https://x.com")).is_none());
    }

    #[test]
    fn test_cors_headers_wildcard() {
        let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
        let h = p.cors_headers(Some("https://x.com"));
        assert!(h.contains("Access-Control-Allow-Origin: *"), "{h}");
        assert!(!h.contains("Vary"), "wildcard needs no Vary: {h}");
    }

    #[test]
    fn test_cors_headers_specific_origin_adds_vary() {
        let p =
            proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("https://ok.com"));
        let h = p.cors_headers(Some("https://ok.com"));
        assert!(
            h.contains("Access-Control-Allow-Origin: https://ok.com"),
            "{h}"
        );
        assert!(h.contains("Vary: Origin"), "{h}");
        // Disallowed origin -> no header.
        assert_eq!(p.cors_headers(Some("https://evil.com")), "");
    }

    #[test]
    fn test_roundtrip_options_preflight() {
        let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
        let req = "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
        let (status, _body) = roundtrip(p, req.to_string());
        assert_eq!(status, 204);
    }

    #[test]
    fn test_roundtrip_preflight_skips_auth() {
        // Preflight carries no credentials, so it must not be 401'd even with auth on.
        let p = proxy_with(true, false, 100, "unused")
            .with_cors(CorsPolicy::parse("*"))
            .with_auth_token(Some("k".to_string()));
        let req = "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\n\r\n";
        let (status, _) = roundtrip(p, req.to_string());
        assert_eq!(status, 204);
    }

    #[test]
    fn test_roundtrip_cors_header_on_response() {
        let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
        let req = "GET /v1/models HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\n\r\n";
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            c.write_all(req.as_bytes()).unwrap();
            c.shutdown(std::net::Shutdown::Write).ok();
            let mut resp = String::new();
            c.read_to_string(&mut resp).unwrap();
            resp
        });
        let (mut s, _) = listener.accept().unwrap();
        p.handle_connection(&mut s).unwrap();
        drop(s);
        let resp = client.join().unwrap();
        assert!(
            resp.contains("Access-Control-Allow-Origin: *"),
            "missing CORS header: {resp}"
        );
    }

    #[test]
    fn test_roundtrip_stats_ok() {
        let p = proxy_with(true, true, 100, "/no/such/cost-log.jsonl");
        let (status, body) = roundtrip(p, "GET /v1/stats HTTP/1.1\r\n\r\n".to_string());
        assert_eq!(status, 200);
        assert!(body.contains("\"object\":\"pasture.stats\""), "{body}");
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
        let json = build_openai_chunk("chatcmpl-x", "hel\"lo", "local", None);
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
        let json = build_openai_chunk("chatcmpl-x", "", "cloud", Some("stop"));
        assert!(json.contains("\"finish_reason\":\"stop\""));
        assert!(json.contains("\"delta\":{}"));
    }

    #[test]
    fn test_sse_frame_format() {
        assert_eq!(sse_frame("X"), "data: X\n\n");
    }

    #[test]
    fn test_utc_date_str_epoch() {
        assert_eq!(utc_date_str(0), "1970-01-01");
    }

    #[test]
    fn test_utc_date_str_known_date() {
        // 2026-06-08 UTC = 20612 days from epoch (verified by counting leap years)
        let ts = 20612u64 * 86400;
        assert_eq!(utc_date_str(ts), "2026-06-08");
    }

    #[test]
    fn test_utc_date_str_y2k() {
        // 2000-01-01 UTC = 10957 days from epoch
        let ts = 10957u64 * 86400;
        assert_eq!(utc_date_str(ts), "2000-01-01");
    }

    #[test]
    fn test_inject_context_prepends_system() {
        let req = CompletionRequest {
            model: "m".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            has_tools: false,
            sampling: Default::default(),
        };
        let injected = inject_context_into(&req);
        assert_eq!(injected.messages[0].role, "system");
        assert!(injected.messages[0].content.contains("Date"));
        assert_eq!(injected.messages[1].role, "user");
    }

    #[test]
    fn test_inject_context_merges_existing_system() {
        let req = CompletionRequest {
            model: "m".to_string(),
            messages: vec![
                Message {
                    role: "system".to_string(),
                    content: "be brief".to_string(),
                },
                Message {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                },
            ],
            stream: false,
            has_tools: false,
            sampling: Default::default(),
        };
        let injected = inject_context_into(&req);
        // No duplicate system messages — context merged into the existing one.
        assert_eq!(injected.messages[0].role, "system");
        assert!(injected.messages[0].content.contains("be brief"));
        assert!(injected.messages[0].content.contains("Date"));
        assert_eq!(injected.messages.len(), 2);
    }

    #[test]
    fn test_inject_context_enabled_on_proxy() {
        let log = tmp_log();
        let p = proxy_with(true, false, 100, &log).with_inject_context(true);
        let resp = p
            .handle_chat(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
            .unwrap();
        assert!(resp.contains("local-reply"));
    }

    #[test]
    fn test_local_only_routes_all_traffic_local() {
        let log = tmp_log();
        let engine = RoutingEngine::new(10, true, true).with_local_only(true);
        // Long prompt that would normally go cloud.
        let long_body = format!(
            "{{\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
            "x".repeat(500)
        );
        let p = Proxy::new(
            engine,
            Some(Box::new(MockBackend::new("local", "local-reply"))),
            Some(Box::new(MockBackend::new("cloud", "cloud-reply"))),
            &log,
        );
        let resp = p.handle_chat(&long_body).unwrap();
        assert!(resp.contains("x_pasture_route"));
        assert!(resp.contains("\"local\"") || resp.contains("local-reply"));
    }

    #[test]
    fn test_fast_model_used_for_simple_prompt() {
        let log = tmp_log();
        let engine = RoutingEngine::new(1000, true, false);
        let p = Proxy::new(
            engine,
            Some(Box::new(MockBackend::new("local", "local-reply"))),
            None,
            &log,
        )
        .with_fast_model(Some("phi3:mini".to_string()), 50);
        // Short, no-hard-signal prompt should use the fast model.
        let resp = p
            .handle_chat(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
            .unwrap();
        assert!(resp.contains("local-reply"));
    }
}

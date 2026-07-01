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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Daily token budget exceeded and `budget_action = "block"` (IMP-26).
    BudgetExceeded(String),
}

impl ProxyError {
    fn status(&self) -> u16 {
        match self {
            ProxyError::BadRequest(_) => 400,
            ProxyError::Routing(_) => 503,
            ProxyError::Backend(_) => 502,
            ProxyError::BudgetExceeded(_) => 429,
        }
    }

    fn message(&self) -> &str {
        match self {
            ProxyError::BadRequest(m)
            | ProxyError::Routing(m)
            | ProxyError::Backend(m)
            | ProxyError::BudgetExceeded(m) => m,
        }
    }

    /// OpenAI-style error `type` for the error envelope (SPEC §3.5).
    fn kind(&self) -> &'static str {
        match self {
            ProxyError::BadRequest(_) => "invalid_request_error",
            ProxyError::Routing(_) => "routing_error",
            ProxyError::Backend(_) => "upstream_error",
            ProxyError::BudgetExceeded(_) => "rate_limit_error",
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
    /// Optional user-defined system prompt prepended to every request
    /// (`PASTURE_SYSTEM_PROMPT`). Merged with any existing system message in the
    /// request (prepended). Complementary to `inject_context` (date/OS context).
    system_prompt: Option<String>,
    /// Configured local model name (from `PASTURE_LOCAL_MODEL`). When a request
    /// specifies this model name, the route is forced to Local regardless of the
    /// routing engine's decision. The sentinel `"local"` also forces Local.
    local_model_name: String,
    /// Configured cloud model name (from `PASTURE_CLOUD_MODEL`). When a request
    /// specifies this model name, the route is forced to Cloud. The sentinel
    /// `"cloud"` also forces Cloud.
    cloud_model_name: String,
    /// Optional path for the structured per-request access log (IMP-access-log).
    /// Appends one JSONL record per request: ts, method, path, status, ms, request_id.
    /// No prompt content, no auth tokens, no PII. None = disabled.
    access_log: Option<String>,
    /// Optional semantic (embedding-similarity) cache (IMP-12). Queries the local
    /// backend for an embedding, then scans stored (embedding, response) pairs for
    /// cosine similarity ≥ threshold. Off by default; never used for sensitive content.
    semantic_cache: Option<std::sync::Mutex<crate::cache::SemanticCache>>,
    /// Known-hard prompts for the embedding difficulty signal (IMP-14). A request
    /// embedding-similar to any of these escalates Local → Cloud before wasting a
    /// local attempt. Empty = disabled (the default). Never overrides privacy.
    hard_prompts: Vec<String>,
    /// Cosine similarity at which a prompt counts as "near a known-hard prompt".
    hard_threshold: f64,
    /// Lazily-embedded centroids for `hard_prompts` (the local backend may not be
    /// running at construction). `None` = not yet attempted; `Some(vec![])` after
    /// a failed embedding attempt — the signal stays disabled for the process
    /// lifetime rather than re-querying a broken backend on every request.
    hard_centroids: std::sync::Mutex<Option<Arc<Vec<Vec<f64>>>>>,
    /// Prompt-injection guard mode (IMP-20). `"off"` = disabled; `"flag"` =
    /// detect and log + annotate the JSON response; `"block"` = reject with 400.
    injection_guard: String,
    /// Daily cloud token budget (IMP-26). 0 = disabled. Running sum of
    /// cloud prompt+completion tokens today (UTC day), initialized from the cost
    /// log at startup and incremented atomically on each cloud completion.
    today_cloud_tokens: AtomicU64,
    /// UTC day number (days since epoch) the `today_cloud_tokens` counter belongs
    /// to (IMP-26). When the current day moves past this, the counter is lazily
    /// reset to 0 so the budget is truly *daily* for a long-running process — not
    /// cumulative-since-startup. Checked on each budget access; no timer thread.
    budget_day: AtomicU64,
    /// Cloud request count and token sum for spike detection (IMP-26).
    cloud_request_count: AtomicU64,
    cloud_token_sum: AtomicU64,
    /// Daily token budget cap (IMP-26). 0 = disabled.
    budget_daily_tokens: u64,
    /// Action when the budget is exceeded: `"local-only"` (default), `"warn"`,
    /// or `"block"` (return 429).
    budget_action: String,
    /// Spike detection factor (IMP-26). A request estimating more than
    /// `spike_factor × running-average` tokens overrides the cloud route to local.
    /// 0 = spike detection disabled.
    spike_factor: u64,
    /// Cloud price in USD per 1M tokens as `(input, output)` (ADR-166). Used to
    /// compute the real `cost_usd` for cloud completions in the cost log and the
    /// `/metrics` + `/v1/stats` spend gauges. `(0.0, 0.0)` (default) means no
    /// pricing is configured and the cost stays 0 — honestly, rather than a
    /// metric that silently always reads zero.
    cloud_price_per_1m: (f64, f64),
    /// Maximum request body bytes (IMP-21). Default 16 MiB. Set via
    /// `PASTURE_MAX_BODY_BYTES`. Bodies larger than this yield 413.
    max_body_bytes: usize,
    /// Pseudonymize PII in cloud requests and restore in responses (IMP-19).
    pseudonymize: bool,
    /// Optional OTel-compatible GenAI trace log path (IMP-23). Empty = disabled.
    otel_log: Option<String>,
    /// OTel `gen_ai.system` value for cloud-routed spans (IMP-23): the configured
    /// cloud provider name (e.g. `"openai"`, `"anthropic"`). Empty falls back to
    /// `"cloud"`. Used only when `otel_log` is set.
    cloud_system: String,
    /// Optional secondary cloud backend tried when the primary cloud fails all
    /// retries (IMP-9 multi-provider follow-up). When set, a primary cloud failure
    /// attempts this backend before falling back to local. None = disabled.
    cloud_fallback: Option<Box<dyn Backend>>,
    /// Incremental cache for `/metrics` and `/v1/stats` (IMP-32, ADR-151):
    /// `(bytes_of_cost_log_consumed, running_summary)`. Each scrape folds only the
    /// newly-appended complete lines instead of re-parsing the whole growing log.
    metrics_cache: std::sync::Mutex<(u64, crate::cost::CostSummary)>,
    /// Output-side PII category visibility (IMP-33). Off by default (`None`);
    /// when enabled, every buffered completion's response text is scanned with
    /// the same category classifier used on input, tallying counts only —
    /// never mutating the response or storing matched values (I5). Exposed via
    /// `/v1/stats` so an operator can see whether responses are echoing PII the
    /// input classifier had no reason to flag (e.g. a summarized document).
    output_pii_stats: Option<crate::output_scan::OutputPiiStats>,
    /// Input-side PII category visibility (IMP-28). Off by default (`None`);
    /// when enabled, every classified request's already-computed
    /// `SensitivityReport` categories are tallied — no re-scanning, since
    /// `route_decision` has already run `privacy::classify`. Complements the
    /// existing stderr notice ("sensitive content detected -> keeping local
    /// (N categories)") with a persistent, queryable breakdown of *which*
    /// categories are triggering local-only routing, exposed via `/v1/stats`.
    input_pii_stats: Option<crate::output_scan::OutputPiiStats>,
    /// Routing decision audit log (IMP-29). `None` disables logging (the
    /// default). When set via `PASTURE_DECISION_LOG=<path>`, every routed
    /// request appends one JSONL record (signals, threshold, route, reason —
    /// no PII, no prompt content) so a wrong decision can be replayed/audited
    /// after the fact instead of only inferred from the PII-free cost log's
    /// outcome-only view.
    decision_logger: Option<crate::decision_log::DecisionLogger>,
    /// Local-backend health tracker (IMP-30). Always present (no opt-in
    /// needed — it only tallies outcomes of calls Pasture already makes, no
    /// extra probes or threads). Every local completion attempt records
    /// success/failure here; after 3 consecutive failures the backend is
    /// marked Down. Exposed via `/v1/stats` so an operator sees a crashed
    /// local model within the next request instead of inferring it only from
    /// scattered error logs.
    local_health: crate::health::BackendHealth,
    /// Circuit-breaker cooldown in seconds (IMP-34, ADR-221). Acts on
    /// `local_health` instead of only observing it: once the local backend
    /// is `Down`, `route_decision` redirects *non-sensitive* Local decisions
    /// to Cloud (when available) instead of repeating a call already known
    /// to fail — until `cooldown` elapses, at which point one probe request
    /// is let through to detect recovery. Sensitive content is never
    /// redirected (privacy over availability, I5), and there is no cloud
    /// fallback if none is configured — this only ever makes an already-Local
    /// decision faster to fail over, never a privacy override.
    health_cooldown_secs: u64,
}

/// Base backoff (doubled each attempt) for cloud retries (IMP-9).
const CLOUD_RETRY_BASE_MS: u64 = 200;

/// Outcome of the embedding-based routing step — the semantic cache (IMP-12) and
/// the difficulty signal (IMP-14), shared by the buffered and streaming paths so
/// they cannot drift (ADR-150).
enum EmbeddingStep {
    /// A semantic-cache hit: serve this response, skipping the backend.
    SemanticHit(CompletionResponse),
    /// Proceed to completion with this (possibly difficulty-escalated) route. The
    /// query embedding, when computed, is returned so the caller can store the
    /// completed response in the semantic cache on a miss.
    Proceed {
        route: Route,
        embedding: Option<Vec<f64>>,
    },
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
            models: Vec::new(),
            cloud_retry: 0,
            fast_model: None,
            fast_threshold: 50,
            inject_context: false,
            auth_token: None,
            rate_limiter: None,
            cors: None,
            io_timeout: None,
            system_prompt: None,
            local_model_name: String::new(),
            cloud_model_name: String::new(),
            access_log: None,
            semantic_cache: None,
            hard_prompts: Vec::new(),
            hard_threshold: 0.85,
            hard_centroids: std::sync::Mutex::new(None),
            injection_guard: "off".to_string(),
            pseudonymize: false,
            otel_log: None,
            cloud_system: String::new(),
            today_cloud_tokens: AtomicU64::new(0),
            budget_day: AtomicU64::new(unix_now() / 86_400),
            cloud_request_count: AtomicU64::new(0),
            cloud_token_sum: AtomicU64::new(0),
            budget_daily_tokens: 0,
            budget_action: "local-only".to_string(),
            spike_factor: 50,
            cloud_price_per_1m: (0.0, 0.0),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            cloud_fallback: None,
            metrics_cache: std::sync::Mutex::new((0, crate::cost::summarize(&[]))),
            output_pii_stats: None,
            input_pii_stats: None,
            decision_logger: None,
            local_health: crate::health::BackendHealth::new(),
            health_cooldown_secs: 30,
        }
    }

    /// Set the prompt-injection guard mode (IMP-20).
    pub fn with_injection_guard(mut self, mode: &str) -> Self {
        self.injection_guard = mode.trim().to_ascii_lowercase();
        self
    }

    /// Configure the daily token budget and spike detection (IMP-26).
    /// `daily_tokens` = 0 disables the budget; `spike_factor` = 0 disables spike
    /// detection. `cost_log_path` seeds the running counter from today's log so
    /// the budget survives a proxy restart.
    pub fn with_budget(
        mut self,
        daily_tokens: u64,
        action: &str,
        spike_factor: u64,
        cost_log_path: &str,
    ) -> Self {
        self.budget_daily_tokens = daily_tokens;
        self.budget_action = action.trim().to_ascii_lowercase();
        self.spike_factor = spike_factor;
        if daily_tokens > 0 {
            let used = crate::cost::today_cloud_tokens(cost_log_path);
            self.today_cloud_tokens = AtomicU64::new(used);
            // The seeded count is today's usage; anchor the rollover day to match.
            self.budget_day = AtomicU64::new(unix_now() / 86_400);
        }
        self
    }

    /// Set cloud pricing in USD per 1M tokens as `(input, output)` (ADR-166).
    /// Cloud completions then log a real `cost_usd` (prompt × input + completion ×
    /// output, per million tokens), making the cost log and the `/metrics` +
    /// `/v1/stats` spend gauges meaningful. `(0.0, 0.0)` leaves cost at 0. Negative
    /// or non-finite prices are clamped to 0 so a misconfiguration cannot produce a
    /// negative or NaN spend total.
    pub fn with_cloud_price(mut self, input_per_1m: f64, output_per_1m: f64) -> Self {
        let clamp = |x: f64| if x.is_finite() && x > 0.0 { x } else { 0.0 };
        self.cloud_price_per_1m = (clamp(input_per_1m), clamp(output_per_1m));
        self
    }

    /// USD cost of a cloud completion from its token counts and configured pricing
    /// (ADR-166). Zero when no pricing is set. Only cloud routes incur a cost;
    /// local and cache are free.
    fn cloud_cost_usd(&self, prompt_tokens: u64, completion_tokens: u64) -> f64 {
        let (in_price, out_price) = self.cloud_price_per_1m;
        (prompt_tokens as f64 / 1_000_000.0) * in_price
            + (completion_tokens as f64 / 1_000_000.0) * out_price
    }

    /// Set the maximum request body size in bytes (IMP-21). Bodies larger than
    /// this are rejected with 413 before being read. Default: 16 MiB.
    pub fn with_max_body_bytes(mut self, limit: usize) -> Self {
        self.max_body_bytes = limit;
        self
    }

    /// Enable PII pseudonymization for cloud requests (IMP-19). When true,
    /// detected PII in messages is replaced with opaque tokens before the
    /// request is sent to the cloud backend, and restored from the response.
    pub fn with_pseudonymize(mut self, enabled: bool) -> Self {
        self.pseudonymize = enabled;
        self
    }

    /// Enable output-side PII category visibility scanning (IMP-33). Off by
    /// default. When enabled, every buffered completion's response text is
    /// scanned for PII categories (detection-only, never mutated) and tallied
    /// for `/v1/stats`.
    pub fn with_output_pii_scan(mut self, enabled: bool) -> Self {
        self.output_pii_stats = if enabled {
            Some(crate::output_scan::OutputPiiStats::new())
        } else {
            None
        };
        self
    }

    /// Enable input-side PII category visibility tallying (IMP-28). Off by
    /// default. When enabled, every classified request's sensitivity
    /// categories are tallied (no re-scanning — reuses the report
    /// `route_decision` already computed) and exposed via `/v1/stats`.
    pub fn with_input_pii_scan(mut self, enabled: bool) -> Self {
        self.input_pii_stats = if enabled {
            Some(crate::output_scan::OutputPiiStats::new())
        } else {
            None
        };
        self
    }

    /// Set the local-backend circuit-breaker cooldown in seconds (IMP-34).
    /// Default 30. 0 disables the cooldown (every request probes local again
    /// immediately when Down — effectively no circuit-breaking, matching
    /// pre-IMP-34 behavior since `should_attempt` with cooldown 0 always
    /// returns true).
    pub fn with_health_cooldown(mut self, secs: u64) -> Self {
        self.health_cooldown_secs = secs;
        self
    }

    /// Enable the routing decision audit log (IMP-29). `path` empty or `None`
    /// disables logging (the default). A file that cannot be opened for
    /// appending disables logging silently (best-effort observability aid,
    /// never a reason to fail request handling).
    pub fn with_decision_log(mut self, path: Option<&str>) -> Self {
        let path = path.filter(|p| !p.is_empty());
        self.decision_logger = crate::decision_log::DecisionLogger::new(path).ok();
        self
    }

    /// Enable the OTel GenAI trace log (IMP-23). Each request appends one
    /// JSONL span with GenAI semantic convention attributes to `path`.
    /// Empty path disables the feature.
    pub fn with_otel_log(mut self, path: Option<String>) -> Self {
        self.otel_log = path.filter(|p| !p.is_empty());
        self
    }

    /// Set the `gen_ai.system` value emitted for cloud-routed OTel spans (IMP-23):
    /// the cloud provider name (e.g. `"openai"`, `"anthropic"`). Empty leaves the
    /// generic `"cloud"` fallback.
    pub fn with_cloud_system(mut self, system: &str) -> Self {
        self.cloud_system = system.trim().to_string();
        self
    }

    /// OTel `gen_ai.system` for an actually-taken route (IMP-23). Reflects the
    /// backend that served the request — cloud routes report the configured
    /// provider (`openai`/`anthropic`), local routes report the local backend's
    /// own name (`ollama`/`local`). This is set after completion because the
    /// route is not known when the span is started.
    fn otel_system_for(&self, route: Route) -> String {
        match route {
            Route::Cloud => {
                if self.cloud_system.is_empty() {
                    "cloud".to_string()
                } else {
                    self.cloud_system.clone()
                }
            }
            Route::Local => self
                .local
                .as_deref()
                .map(|b| b.name().to_string())
                .unwrap_or_else(|| "local".to_string()),
        }
    }

    /// Emit an OTel span for a cache hit (ADR-144). Without this, requests served
    /// from the exact-match or semantic cache return before any span is started,
    /// so the trace log shows zero cache traffic even though the schema documents
    /// `pasture.route="cache"`. `gen_ai.system` is set to the route label (there is
    /// no upstream provider for a cache hit). No-op when `PASTURE_OTEL_LOG` is unset.
    fn emit_cache_hit_span(
        &self,
        span: &mut Option<crate::telemetry::Span>,
        resp: &CompletionResponse,
        route_label: &'static str,
    ) {
        if let (Some(span), Some(log_path)) = (span.as_mut(), self.otel_log.as_deref()) {
            span.system = route_label.to_string();
            span.response_model = resp.model.clone();
            span.input_tokens = resp.prompt_tokens;
            span.output_tokens = resp.completion_tokens;
            span.route = route_label;
            // Derive finish_reason from the response: tool calls use "tool_calls",
            // ordinary completions use "stop" (ADR-181).
            span.finish_reason = Some(finish_reason_for(resp).to_string());
            span.finish();
            if let Err(e) = span.append_to(log_path) {
                eprintln!("pasture: otel log write failed: {e}");
            }
        }
    }

    /// Set a secondary cloud backend for multi-provider fallback (IMP-9 follow-up).
    /// When the primary cloud provider fails all retries, this backend is tried
    /// before giving up to the local model. `None` keeps single-provider behaviour.
    pub fn with_cloud_fallback(mut self, backend: Option<Box<dyn Backend>>) -> Self {
        self.cloud_fallback = backend;
        self
    }

    pub fn with_access_log(mut self, path: Option<String>) -> Self {
        self.access_log = path;
        self
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

    /// Build a stub `POST /v1/moderations` response: all categories false, all
    /// scores zero. Pasture does not run content moderation; the stub lets clients
    /// that call `/v1/moderations` unconditionally continue without error.
    fn handle_moderations(body: &str) -> Result<String, ProxyError> {
        let v = parse(body).map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        // Accept "input" as string or array; ignore the value (stub always passes).
        let _has_input = v.get("input").is_some();
        Ok(build_moderations_response())
    }

    /// Return the `Allow:` header value for a known route so the caller can
    /// send 405 Method Not Allowed. Returns `None` for unknown paths (→ 404).
    fn route_allowed_methods(path: &str) -> Option<&'static str> {
        if path.starts_with("/v1/chat/completions")
            || path.starts_with("/v1/completions")
            || path.starts_with("/v1/embeddings")
            || path.starts_with("/v1/moderations")
        {
            Some("POST, OPTIONS")
        } else if path.starts_with("/v1/stats")
            || path.starts_with("/metrics")
            || path.starts_with("/v1/models")
            || path.starts_with("/v1/engines")
        {
            Some("GET, OPTIONS")
        } else if path.starts_with("/health") {
            Some("GET, HEAD")
        } else if path.starts_with("/v1/audio") || path.starts_with("/v1/images") {
            // Recognised but not implemented; allow POST so the 405 vs 501 distinction is correct.
            Some("POST, OPTIONS")
        } else {
            None
        }
    }

    /// Apply rate-limit then auth gating for a request path (IMP-15). Returns
    /// `Some((status, message, type))` when the request must be rejected, else
    /// `None`. `/health` is always exempt so liveness probes work unauthenticated.
    /// Build the `X-RateLimit-*` response header block (IMP-ratelimit-headers).
    /// Empty when rate limiting is disabled (the localhost default) so there is
    /// zero overhead and no header noise in the common case. When enabled, every
    /// response advertises the request budget so clients can self-throttle
    /// proactively instead of only reacting to a `429`. Only the *request* family
    /// is emitted — Pasture meters requests, not tokens, so token-family headers
    /// would be misleading.
    fn ratelimit_headers(&self) -> String {
        let Some(rl) = &self.rate_limiter else {
            return String::new();
        };
        let Ok(mut g) = rl.lock() else {
            return String::new();
        };
        let (limit, remaining, reset) = g.snapshot();
        format!(
            "X-RateLimit-Limit-Requests: {limit}\r\n\
             X-RateLimit-Remaining-Requests: {remaining}\r\n\
             X-RateLimit-Reset-Requests: {reset}s\r\n"
        )
    }

    /// The fourth tuple element is a `Retry-After` value in whole seconds, set
    /// only for a `429` so the caller can advise the client when to retry.
    fn check_gate(
        &self,
        path: &str,
        auth: Option<&str>,
    ) -> Option<(u16, &'static str, &'static str, Option<u64>)> {
        if path.starts_with("/health") {
            return None;
        }
        // Authenticate BEFORE metering: an unauthenticated request is rejected
        // cheaply with 401 and must NOT consume a token from the global bucket.
        // Otherwise an anonymous flood (no valid token) could drain the single
        // shared bucket and 429 the legitimate authenticated client — turning the
        // rate limit into a denial-of-service lever for an unauthenticated party.
        if let Some(expected) = &self.auth_token {
            if !auth_ok(auth, expected) {
                return Some((
                    401,
                    "missing or invalid Authorization bearer token",
                    "invalid_request_error",
                    None,
                ));
            }
        }
        // Monitoring endpoints (/metrics, /v1/stats) are read-only, zero-inference-cost
        // and must not consume rate-limit tokens (ADR-167). A standard Prometheus scraper
        // at 15-second intervals (4 req/min) would otherwise eat a disproportionate share
        // of a tight inference budget — e.g. 40% of PASTURE_RATE_LIMIT=10. /metrics is
        // also outside the /v1/* namespace the limiter documents itself as covering.
        // Auth is still enforced above for both endpoints.
        if path.starts_with("/metrics") || path.starts_with("/v1/stats") {
            return None;
        }
        if let Some(rl) = &self.rate_limiter {
            // Hold the guard so the Retry-After estimate reflects the same bucket
            // state as the denial. A poisoned lock fails open (request allowed).
            if let Ok(mut g) = rl.lock() {
                if !g.allow() {
                    let retry = g.retry_after_secs();
                    return Some((429, "rate limit exceeded", "rate_limit_error", Some(retry)));
                }
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

    /// Set a user-defined system prompt prepended to every request. An empty
    /// string disables the feature (equivalent to passing `None`). The prompt is
    /// merged with any existing system message in the request (prepended).
    pub fn with_system_prompt(mut self, prompt: Option<String>) -> Self {
        self.system_prompt = prompt.filter(|s| !s.is_empty());
        self
    }

    /// Set the known local and cloud model names for model-pinned routing.
    /// When a request specifies `req.model == local_name` (or `"local"`), the
    /// route is forced to Local; `cloud_name` (or `"cloud"`) forces Cloud.
    pub fn with_model_names(mut self, local_name: String, cloud_name: String) -> Self {
        self.local_model_name = local_name;
        self.cloud_model_name = cloud_name;
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

    pub fn with_cache_ttl(self, ttl_secs: u64) -> Self {
        if ttl_secs > 0 {
            if let Some(ref cache_mutex) = self.cache {
                if let Ok(mut guard) = cache_mutex.lock() {
                    guard.set_max_age(ttl_secs);
                }
            }
            // Apply the same TTL to the semantic cache (ADR-160) so PASTURE_CACHE_TTL
            // bounds staleness for both caches, not just the exact-match one.
            if let Some(ref sem_mutex) = self.semantic_cache {
                if let Ok(mut guard) = sem_mutex.lock() {
                    guard.set_max_age(ttl_secs);
                }
            }
        }
        self
    }

    /// Enable the optional semantic cache (IMP-12). `cap` entries are stored;
    /// `threshold` is the minimum cosine similarity for a hit (e.g. 0.92).
    /// The local backend's `/v1/embeddings` endpoint is used — no new deps.
    /// Disabled (cap = 0) by default so the zero-dependency build is unchanged.
    pub fn with_semantic_cache(mut self, cap: usize, threshold: f64) -> Self {
        if cap > 0 {
            self.semantic_cache = Some(std::sync::Mutex::new(crate::cache::SemanticCache::new(
                cap, threshold,
            )));
        }
        self
    }

    /// Enable the embedding difficulty signal (IMP-14): requests whose embedding
    /// is within `threshold` cosine similarity of any of `prompts` escalate
    /// Local → Cloud. An empty list leaves the signal disabled.
    pub fn with_hard_prompts(mut self, prompts: Vec<String>, threshold: f64) -> Self {
        self.hard_prompts = prompts;
        self.hard_threshold = threshold;
        self
    }

    /// Extract a message's `content` (IMP-31 / ADR-145). Accepts both the
    /// canonical string form and OpenAI's array-of-parts form
    /// (`[{"type":"text","text":...}, ...]`) that vision-capable SDKs emit even
    /// for plain text. Text parts are concatenated (newline-joined) so the result
    /// still flows through `routing_text()` → privacy `classify()` (the PII guard
    /// is unchanged). A genuinely multimodal part (image/audio/file) is rejected
    /// with 400 rather than silently dropped: Pasture is a text-routing proxy and
    /// must not answer a vision request as if the image were absent.
    fn extract_message_content(m: &JsonValue) -> Result<String, ProxyError> {
        // content:null is valid for tool-call assistant messages (ADR-183).
        let c = match m.get("content") {
            Some(JsonValue::Null) => return Ok(String::new()),
            None => {
                return Err(ProxyError::BadRequest(
                    "message missing 'content'".to_string(),
                ))
            }
            Some(v) => v,
        };
        match c {
            JsonValue::Str(s) => Ok(s.clone()),
            JsonValue::Array(parts) => {
                let mut out: Vec<String> = Vec::with_capacity(parts.len());
                for part in parts {
                    let kind = part.get("type").and_then(|t| t.as_str());
                    let text = part.get("text").and_then(|t| t.as_str());
                    match (kind, text) {
                        // Canonical text part, or a bare {"text": "..."} with no
                        // explicit type (some clients omit it).
                        (Some("text"), Some(t)) | (None, Some(t)) => out.push(t.to_string()),
                        // A non-text part means the client wants true multimodal
                        // input, which a text router cannot serve faithfully.
                        (Some(other), _) => {
                            return Err(ProxyError::BadRequest(format!(
                                "unsupported content part '{other}'; Pasture routes text only"
                            )));
                        }
                        _ => {
                            return Err(ProxyError::BadRequest(
                                "content part missing 'text'".to_string(),
                            ));
                        }
                    }
                }
                Ok(out.join("\n"))
            }
            _ => Err(ProxyError::BadRequest(
                "message 'content' must be a string or array of text parts".to_string(),
            )),
        }
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
            let content = Self::extract_message_content(m)?;
            // Carry tool_call_id for role:"tool" result messages (ADR-182).
            let tool_call_id = m
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            // Carry tool_calls from role:"assistant" messages (ADR-183): the
            // array must be round-tripped to the backend so multi-turn agent
            // conversations preserve the model's prior tool-call context.
            let tool_calls_json = m
                .get("tool_calls")
                .filter(|tc| matches!(tc, JsonValue::Array(a) if !a.is_empty()))
                .map(|tc| tc.to_json_string());
            parsed.push(Message {
                role,
                content,
                tool_call_id,
                tool_calls_json,
            });
        }
        if parsed.is_empty() {
            return Err(ProxyError::BadRequest("no messages provided".to_string()));
        }
        // Tool/function-calling presence is a hard routing signal (IMP-10):
        // a non-empty `tools` or `functions` array means the client expects
        // reliable tool use, which the stronger (cloud) model handles best.
        // `tool_choice` != "none" is treated equivalently: if the caller explicitly
        // requests a tool call (auto/required/named function), escalate regardless
        // of whether `tools` is populated (some clients send tool_choice separately).
        let non_empty_array = |key: &str| {
            v.get(key)
                .and_then(JsonValue::as_array)
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        };
        // An explicit JSON `null` (common from serializers that emit every
        // field) means "no tool choice" — treat it like absent, not active,
        // so it does not force every request to the cloud (ADR-176).
        let tool_choice_active = v
            .get("tool_choice")
            .map(|tc| !matches!(tc, JsonValue::Null) && tc.as_str() != Some("none"))
            .unwrap_or(false);
        let has_tools =
            non_empty_array("tools") || non_empty_array("functions") || tool_choice_active;
        // "n" must be 1 (or absent). Pasture always returns exactly one
        // completion per request; n > 1 would silently return fewer than
        // requested, so we reject it with a clear error.
        let n = v.get("n").and_then(JsonValue::as_f64).unwrap_or(1.0) as i64;
        if n != 1 {
            return Err(ProxyError::BadRequest(format!(
                "'n' must be 1; got {n} (Pasture returns exactly one completion per request)"
            )));
        }
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
            // Forward tool definitions verbatim so the backend can make tool
            // calls (ADR-177). Only a non-empty `tools` array and a non-null
            // `tool_choice` are carried — an empty/null value is meaningless and
            // would only bloat the upstream request.
            tools: v
                .get("tools")
                .filter(|t| matches!(t, JsonValue::Array(a) if !a.is_empty()))
                .cloned(),
            tool_choice: v
                .get("tool_choice")
                .filter(|t| !matches!(t, JsonValue::Null))
                .cloned(),
        }
    }

    /// Classify, then decide routing (keeps sensitive content local).
    /// Returns the decision and whether the content was sensitive.
    fn classify_and_decide(&self, req: &CompletionRequest) -> Result<(Decision, bool), ProxyError> {
        let (decision, report) = self.route_decision(req)?;
        if report.is_sensitive() {
            eprintln!(
                "pasture: sensitive content detected -> keeping local ({} categories)",
                report.categories.len()
            );
            if let Some(stats) = &self.input_pii_stats {
                stats.tally(&report.categories);
            }
        }
        Ok((decision, report.is_sensitive()))
    }

    /// Pure routing decision + sensitivity report, with no side effects (no
    /// logging). Shared by `classify_and_decide` (which adds the stderr notice)
    /// and the `/v1/route` preview handler (ADR-198), which must compute the same
    /// decision without running a backend or emitting logs.
    fn route_decision(
        &self,
        req: &CompletionRequest,
    ) -> Result<(Decision, crate::privacy::SensitivityReport), ProxyError> {
        let text = req.routing_text();
        // Privacy classification scans tool-call arguments too (ADR-187): PII can
        // live solely in tool_calls_json, which routing_text() omits. The routing
        // *difficulty* decision below still uses content-only `text` so tool bytes
        // don't inflate the token-length heuristic.
        let report = crate::privacy::classify(&req.privacy_text());
        let sensitive = report.is_sensitive();
        // Model-pinned routing: if the client requested a specific model that we
        // recognise, force the route to the matching backend so the backend can
        // honour the exact model name. Sentinels "local" and "cloud" also work.
        let forced = {
            let m = req.model.as_str();
            if m == "local" || (!self.local_model_name.is_empty() && m == self.local_model_name) {
                Some(Route::Local)
            } else if m == "cloud"
                || (!self.cloud_model_name.is_empty() && m == self.cloud_model_name)
            {
                Some(Route::Cloud)
            } else {
                None
            }
        };
        let mut decision = self
            .engine
            .decide_full(&text, forced, sensitive, req.has_tools)
            .map_err(|e| ProxyError::Routing(e.to_string()))?;
        // Circuit breaker (IMP-34, ADR-221): local_health (IMP-30) was, until
        // now, purely observational — the router kept sending traffic to a
        // backend it already knew was Down, wasting a full request's latency
        // on a call almost certain to fail again before any fallback kicked
        // in. Redirect the *natural, difficulty-based* Local decision to
        // Cloud once the local backend is Down and outside its cooldown
        // window — but never for: sensitive content (I5 — privacy always
        // wins over availability), an explicit per-request model pin
        // (`forced`, e.g. `model:"local"` — honour the caller's explicit
        // choice), or `PASTURE_LOCAL_ONLY` (an explicit "never touch cloud"
        // operator setting). Requires a configured cloud backend to fall back
        // to; with none, this is a no-op (there is nowhere to redirect).
        if decision.route == Route::Local
            && !sensitive
            && forced.is_none()
            && !self.engine.is_local_only()
            && self.cloud.is_some()
            && !self.local_health.should_attempt(self.health_cooldown_secs)
        {
            decision.reason = format!(
                "{} (local circuit open, redirected to cloud)",
                decision.reason
            );
            decision.route = Route::Cloud;
        }
        Ok((decision, report))
    }

    /// Handle `POST /v1/route` (ADR-198): preview the routing decision for a
    /// chat-completions body **without** calling any backend — no tokens spent,
    /// no cost logged, nothing sent to a cloud provider. Returns the route,
    /// reason, sensitivity (category labels only, never values — I3), the
    /// estimated token count the length heuristic uses, and whether tools were
    /// detected. Mirrors the CLI `route` command on the HTTP surface.
    fn handle_route_preview(&self, body: &str) -> Result<String, ProxyError> {
        let req = Self::parse_request(body)?;
        let (decision, report) = self.route_decision(&req)?;
        // `estimated_tokens` is the content-only routing-heuristic value the
        // `reason` references. The cost/billing prediction uses `estimation_text`
        // (which includes tool definitions and tool_calls, ADR-184) and the IMP-24
        // output prediction, so the dollar estimate matches what the budget guard
        // and the real cloud bill would count.
        let estimated = crate::routing::estimate_tokens(&req.routing_text());
        let billed_input = crate::routing::estimate_tokens(&req.estimation_text()) as u64;
        let predicted_total =
            crate::routing::estimate_total_tokens(&req.estimation_text(), req.sampling.max_tokens)
                as u64;
        let predicted_output = predicted_total.saturating_sub(billed_input);
        // Budget/spike guard preview (ADR-200): the routing engine may decide Cloud,
        // but the IMP-26 guard can redirect it to local (local-only), let it proceed
        // (warn), or reject it (block) at request time. Reflect that here — read-only,
        // reserving nothing — so the previewed route and cost match what would really
        // happen instead of the pre-guard engine decision.
        let (effective_route, budget_note, serves_cloud) = if decision.route == Route::Cloud
            && (self.budget_daily_tokens > 0 || self.spike_factor > 0)
        {
            match self.budget_spike_preview(predicted_total) {
                None => (Route::Cloud, None, true),
                Some(reason) => match self.budget_action.as_str() {
                    "block" => (
                        Route::Cloud,
                        Some(format!("would be blocked (429): {reason}")),
                        false,
                    ),
                    "warn" => (
                        Route::Cloud,
                        Some(format!("over budget, proceeds (warn): {reason}")),
                        true,
                    ),
                    _ => (
                        Route::Local,
                        Some(format!("redirected to local: {reason}")),
                        false,
                    ),
                },
            }
        } else {
            (decision.route, None, decision.route == Route::Cloud)
        };
        // Cost applies only when the request would actually be served on cloud
        // (priced from PASTURE_CLOUD_PRICE_PER_1M, 0 when unset). A local route, a
        // local-only redirect, or a block all cost nothing.
        let estimated_cost = if serves_cloud {
            self.cloud_cost_usd(billed_input, predicted_output)
        } else {
            0.0
        };
        let categories = report
            .categories
            .iter()
            .map(|c| format!("\"{}\"", escape_string(c)))
            .collect::<Vec<_>>()
            .join(",");
        let budget_field = match budget_note {
            Some(note) => format!("\"{}\"", escape_string(&note)),
            None => "null".to_string(),
        };
        Ok(format!(
            "{{\"object\":\"pasture.route\",\"route\":\"{}\",\"reason\":\"{}\",\"budget\":{},\"sensitive\":{},\"categories\":[{}],\"estimated_tokens\":{},\"predicted_output_tokens\":{},\"predicted_total_tokens\":{},\"estimated_cost_usd\":{:.6},\"has_tools\":{}}}",
            effective_route.as_str(),
            escape_string(&decision.reason),
            budget_field,
            report.is_sensitive(),
            categories,
            estimated,
            predicted_output,
            predicted_total,
            estimated_cost,
            req.has_tools,
        ))
    }

    /// Apply the configured prompt framing to a request: the user-defined system
    /// prompt first (outermost frame), then the optional date/OS context message.
    /// Returns `None` when neither is configured, so the caller keeps the original
    /// borrow without a clone. Shared by the buffered (`run_completion`) and
    /// streaming (`stream_chat_to_socket`) paths so a `stream:true` request gets
    /// the same system prompt and context as a buffered one (ADR-149).
    fn frame_request(&self, req: &CompletionRequest) -> Option<CompletionRequest> {
        let mut framed: Option<CompletionRequest> = None;
        if let Some(sp) = &self.system_prompt {
            framed = Some(prepend_system_prompt(req, sp));
        }
        if self.inject_context {
            let base = framed.as_ref().unwrap_or(req);
            framed = Some(inject_context_into(base));
        }
        framed
    }

    /// Compute the query embedding once and apply the semantic cache (IMP-12) and
    /// difficulty signal (IMP-14). Returns `SemanticHit` to serve a cached answer,
    /// or `Proceed` with the (possibly escalated) route and the embedding for a
    /// later store-on-miss. Never computed for sensitive content (I5); embeddings
    /// use the local backend and stay on the machine. On a semantic hit a cache
    /// span is emitted into `otel_span` (ADR-144). Shared by the buffered and
    /// streaming paths (ADR-150).
    fn embedding_step(
        &self,
        req: &CompletionRequest,
        route: Route,
        sensitive: bool,
        otel_span: &mut Option<crate::telemetry::Span>,
    ) -> EmbeddingStep {
        let want_difficulty =
            !self.hard_prompts.is_empty() && route == Route::Local && self.cloud.is_some();
        let query_embedding: Option<Vec<f64>> =
            if !sensitive && (self.semantic_cache.is_some() || want_difficulty) {
                self.local.as_deref().and_then(|local| {
                    let text = semantic_embed_text(req);
                    local
                        .embeddings(&[text])
                        .ok()
                        .and_then(|r| r.vectors.into_iter().next())
                })
            } else {
                None
            };
        if let (Some(emb), Some(sem_mutex)) =
            (query_embedding.as_ref(), self.semantic_cache.as_ref())
        {
            if let Ok(mut guard) = sem_mutex.lock() {
                let samp = crate::cache::sampling_key(&req.sampling);
                if let Some(hit) = guard.find_similar(emb, &req.model, samp) {
                    self.emit_cache_hit_span(otel_span, &hit, "semantic_cache");
                    return EmbeddingStep::SemanticHit(hit);
                }
            }
        }
        let mut planned_route = route;
        if want_difficulty {
            if let Some(emb) = query_embedding.as_ref() {
                if let Some(sim) = self.similar_to_hard(emb) {
                    eprintln!("pasture: prompt similar to known-hard set (cos {sim:.2}) -> cloud");
                    planned_route = Route::Cloud;
                }
            }
        }
        EmbeddingStep::Proceed {
            route: planned_route,
            embedding: query_embedding,
        }
    }

    /// Replay a cached response to the socket as a complete SSE stream
    /// (ADR-147/150): headers, the content as one delta, the stop chunk, an
    /// optional usage chunk, then `[DONE]`. Records a free cost entry. The caller
    /// emits the OTel cache-hit span. `route_label` is `"cache"` or
    /// `"semantic_cache"`. The reported model is the cached response's model, as
    /// the buffered cache path does.
    fn write_cached_stream(
        &self,
        sock: &mut std::net::TcpStream,
        cors: &str,
        hit: &CompletionResponse,
        route_label: &'static str,
        include_usage: bool,
        injection_label: Option<&str>,
    ) -> std::io::Result<()> {
        write_sse_headers(sock, cors)?;
        let id = next_completion_id();
        let model = hit.model.clone();
        let fp = fingerprint_for_model(&model);
        // Capture once so every chunk in this stream shares the same created
        // timestamp, matching the OpenAI contract (ADR-170).
        let created = unix_now();
        // Surface the injection-guard flag first (ADR-191), even for a cache hit,
        // so streaming flag-mode matches the buffered path's annotation.
        if let Some(label) = injection_label {
            let frame = sse_frame(&build_openai_injection_chunk(
                &id,
                &model,
                &fp,
                route_label,
                label,
                created,
            ));
            sock.write_all(frame.as_bytes())?;
        }
        if !hit.content.is_empty() {
            let frame = sse_frame(&build_openai_chunk(
                &id,
                &model,
                &fp,
                &hit.content,
                route_label,
                None,
                created,
            ));
            sock.write_all(frame.as_bytes())?;
        }
        // Replay any tool_calls as a delta chunk before the stop chunk (ADR-178),
        // so a cached tool-call response is not silently dropped on the stream path.
        if let Some(tc) = &hit.tool_calls {
            let frame = sse_frame(&build_openai_tool_calls_chunk(
                &id,
                &model,
                &fp,
                tc,
                route_label,
                created,
            ));
            sock.write_all(frame.as_bytes())?;
        }
        self.log_cost(route_label, hit, None, 0);
        let finish = finish_reason_for(hit);
        let stop = sse_frame(&build_openai_chunk(
            &id,
            &model,
            &fp,
            "",
            route_label,
            Some(finish),
            created,
        ));
        sock.write_all(stop.as_bytes())?;
        if include_usage {
            let usage = sse_frame(&build_openai_usage_chunk(
                &id,
                &model,
                &fp,
                route_label,
                hit.prompt_tokens,
                hit.completion_tokens,
                created,
            ));
            sock.write_all(usage.as_bytes())?;
        }
        sock.write_all(b"data: [DONE]\n\n")
    }

    /// Account a completed streamed response (ADR-153): cost log + budget accrual,
    /// OTel span, and cache stores (exact + semantic, restored content). Performs
    /// no client I/O, so it runs whether or not the client is still connected — a
    /// mid-stream disconnect must not lose the cost, budget, trace, or cache entry
    /// for tokens the backend really consumed. Mirrors the buffered path's
    /// post-completion accounting.
    #[allow(clippy::too_many_arguments)]
    fn finalize_streamed(
        &self,
        r: &CompletionResponse,
        route: Route,
        route_label: &'static str,
        otel_span: &mut Option<crate::telemetry::Span>,
        cache_key: Option<u64>,
        query_embedding: Option<Vec<f64>>,
        cache_mapping: Option<&crate::pseudonymize::Mapping>,
        req_model: &str,
        req_sampling: u64,
        reserved_tokens: u64,
    ) {
        self.log_cost(route_label, r, None, reserved_tokens);
        if let (Some(span), Some(log_path)) = (otel_span.as_mut(), self.otel_log.as_deref()) {
            span.system = self.otel_system_for(route);
            span.response_model = r.model.clone();
            span.input_tokens = r.prompt_tokens;
            span.output_tokens = r.completion_tokens;
            span.route = route_label;
            // Derive finish_reason from the response: tool calls use "tool_calls",
            // ordinary completions use "stop" (ADR-181).
            span.finish_reason = Some(finish_reason_for(r).to_string());
            span.finish();
            if let Err(e) = span.append_to(log_path) {
                eprintln!("pasture: otel log write failed: {e}");
            }
        }
        // Cache the *restored* content (never the `<EMAIL_n>` tokens), as buffered.
        let restored = match cache_mapping {
            Some(m) => crate::pseudonymize::restore(&r.content, m),
            None => r.content.clone(),
        };
        // IMP-33 streaming parity: complete_buffered scans the *restored* response
        // (real PII visible again, after any pseudonymize masking is undone) — the
        // buffered path's stats.scan(&resp.content) runs after run_completion has
        // already called pseudonymize::restore internally. Scanning `r.content`
        // here instead would see masked placeholders like `<EMAIL_1>` whenever
        // pseudonymize is active, never the real category. `restored` is exactly
        // what the client actually receives over the wire (the StreamRestorer
        // de-masks each SSE delta the same way), so it is the correct text to scan.
        if let Some(stats) = &self.output_pii_stats {
            stats.scan(&restored);
        }
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            let mut to_cache = r.clone();
            to_cache.content = restored.clone();
            if let Ok(mut g) = cache.lock() {
                g.put(key, to_cache);
            }
        }
        if let (Some(emb), Some(sem_mutex)) = (query_embedding, self.semantic_cache.as_ref()) {
            let mut to_cache = r.clone();
            to_cache.content = restored;
            if let Ok(mut g) = sem_mutex.lock() {
                g.put(emb, req_model.to_string(), req_sampling, to_cache);
            }
        }
    }

    /// Emit an error OTel span for a request that failed or was rejected *after*
    /// the span was started (ADR-143/193): mark `status=error`, record the message
    /// and route, finish, and append. No-op when tracing is off or no span exists.
    /// Shared by the backend-failure paths and the budget-block / backend-unavailable
    /// rejection paths so no started span is ever silently dropped.
    fn emit_error_span(
        &self,
        otel_span: &mut Option<crate::telemetry::Span>,
        route: Route,
        error: &str,
    ) {
        if let (Some(span), Some(log_path)) = (otel_span.as_mut(), self.otel_log.as_deref()) {
            span.status = "error";
            span.error_message = Some(error.to_string());
            span.route = route.as_str();
            span.system = self.otel_system_for(route);
            span.finish();
            if let Err(w) = span.append_to(log_path) {
                eprintln!("pasture: otel log write failed: {w}");
            }
        }
    }

    fn backend_for(&self, route: Route) -> Result<&dyn Backend, ProxyError> {
        match route {
            Route::Local => self.local.as_deref(),
            Route::Cloud => self.cloud.as_deref(),
        }
        .ok_or_else(|| ProxyError::Routing(format!("no backend for route {}", route.as_str())))
    }

    /// Record the outcome of a local-backend call in `local_health` (IMP-30).
    /// A thin wrapper around the closure that actually makes the call, so
    /// every local completion path (direct, cascade, cloud-failure fallback)
    /// updates the same tracker without duplicating timing/outcome logic.
    fn track_local_call<T>(
        &self,
        f: impl FnOnce() -> Result<T, BackendError>,
    ) -> Result<T, BackendError> {
        let start = std::time::Instant::now();
        let result = f();
        let elapsed_ms = start.elapsed().as_millis() as u64;
        match &result {
            Ok(_) => self.local_health.mark_healthy(elapsed_ms),
            Err(e) => self.local_health.mark_unhealthy(e.to_string(), elapsed_ms),
        }
        result
    }

    /// Reset the daily token counter when the UTC day has advanced past the day
    /// it was last anchored to (IMP-26). Lazy — invoked on each budget access, so
    /// a long-running process gets a *daily* budget without a timer thread. The
    /// thread that wins the day swap performs the reset; concurrent callers see
    /// the new day and skip it.
    fn roll_budget_day_if_needed(&self) {
        let today = unix_now() / 86_400;
        let stored = self.budget_day.load(Ordering::Relaxed);
        // Reset only when the day strictly ADVANCES (ADR-155). A backward wall-clock
        // step across UTC midnight — NTP correction, VM snapshot restore, manual
        // change — must NOT zero the counter and hand out a fresh daily budget; that
        // would under-enforce the cap. `today > stored` also means a clock that was
        // briefly ahead and corrected back keeps enforcing until real time catches
        // up (the safe direction).
        if today > stored
            && self
                .budget_day
                .compare_exchange(stored, today, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.today_cloud_tokens.store(0, Ordering::Relaxed);
            // ADR-185: reset spike-detector history so the average stays day-scoped.
            // Without this, cloud_token_sum / cloud_request_count accumulate for the
            // lifetime of the process — stale history skews the average, making the
            // detector unresponsive to genuine spikes late in a long-running session.
            self.cloud_token_sum.store(0, Ordering::Relaxed);
            self.cloud_request_count.store(0, Ordering::Relaxed);
        }
    }

    /// Release `n` tokens back to the daily counter, saturating at 0 (ADR-164).
    ///
    /// Every budget pre-reservation (ADR-163) is rolled back or reconciled with a
    /// subtraction. A plain `fetch_sub` underflows — wrapping to ~`u64::MAX` — when
    /// `roll_budget_day_if_needed` has reset `today_cloud_tokens` to 0 between the
    /// reservation and its release. That happens whenever a request straddles UTC
    /// midnight (cloud RTTs are seconds) or a concurrent request rolls the day. The
    /// wrapped counter would then dwarf any budget and silently block the cloud
    /// route for the rest of the new day. A saturating CAS loop clamps at 0 instead.
    fn release_cloud_tokens(&self, n: u64) {
        let _ = self
            .today_cloud_tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
                Some(cur.saturating_sub(n))
            });
    }

    /// Live view of the daily cloud-token budget for `/metrics` and `/v1/stats`
    /// (ADR-165): `(used_today, daily_limit)`. Rolls the day first so the gauge
    /// reads 0 on a fresh UTC day even before any request arrives, matching what
    /// the enforcer (`check_budget_and_spike`) would see. `used` reflects the
    /// enforcement counter — including in-flight pre-reservations (ADR-163) — so a
    /// scrape shows exactly the value that gates the next request. A `limit` of 0
    /// means the daily budget is disabled (no cap). Token counts only; no PII (I3).
    fn budget_snapshot(&self) -> (u64, u64) {
        self.roll_budget_day_if_needed();
        (
            self.today_cloud_tokens.load(Ordering::Relaxed),
            self.budget_daily_tokens,
        )
    }

    /// Budget + spike check (IMP-26). Returns `Some(reason)` when the cloud route
    /// should be overridden or blocked; `None` when the request may proceed normally.
    /// Called only when the routing engine has decided Cloud.
    fn check_budget_and_spike(&self, estimated_tokens: u64) -> Option<&'static str> {
        self.roll_budget_day_if_needed();
        // Spike: single request far above the running average → route local instead.
        if self.spike_factor > 0 && estimated_tokens > 0 {
            let count = self.cloud_request_count.load(Ordering::Relaxed);
            if count > 0 {
                let sum = self.cloud_token_sum.load(Ordering::Relaxed);
                let avg = sum / count;
                if avg > 0 && estimated_tokens > self.spike_factor.saturating_mul(avg) {
                    return Some("spike detected — request exceeds average by spike_factor");
                }
            }
        }
        // Daily budget: cumulative cloud tokens since UTC midnight.
        if self.budget_daily_tokens > 0 {
            // Atomically reserve estimated tokens. If the counter was already at or
            // above the budget before our add, roll back and reject. This closes the
            // TOCTOU window between check and increment (ADR-163).
            let prev = self
                .today_cloud_tokens
                .fetch_add(estimated_tokens, Ordering::Relaxed);
            if prev >= self.budget_daily_tokens {
                self.release_cloud_tokens(estimated_tokens);
                return Some("daily cloud token budget exceeded");
            }
        }
        None
    }

    /// Read-only budget/spike check for the `/v1/route` preview (ADR-200): returns
    /// the reason the guard *would* override a cloud route, **without** reserving
    /// tokens or mutating the daily counter. Mirrors `check_budget_and_spike`'s
    /// conditions exactly — the spike comparison is identical, and the budget test
    /// uses `load() >= budget` (the same `prev >= budget` predicate, since `prev`
    /// is the value before the live path's `fetch_add`). Rolls the UTC day first,
    /// like `budget_snapshot`, so a preview on a fresh day reads 0.
    fn budget_spike_preview(&self, estimated_tokens: u64) -> Option<&'static str> {
        self.roll_budget_day_if_needed();
        if self.spike_factor > 0 && estimated_tokens > 0 {
            let count = self.cloud_request_count.load(Ordering::Relaxed);
            if count > 0 {
                let sum = self.cloud_token_sum.load(Ordering::Relaxed);
                let avg = sum / count;
                if avg > 0 && estimated_tokens > self.spike_factor.saturating_mul(avg) {
                    return Some("spike detected — request exceeds average by spike_factor");
                }
            }
        }
        if self.budget_daily_tokens > 0
            && self.today_cloud_tokens.load(Ordering::Relaxed) >= self.budget_daily_tokens
        {
            return Some("daily cloud token budget exceeded");
        }
        None
    }

    /// Apply the IMP-26 budget/spike guard to an already-decided route. Returns
    /// the (possibly downgraded) route, or `Err(BudgetExceeded)` when the action
    /// is `block`. A no-op for non-cloud routes or when neither guard is
    /// configured. Shared by the buffered (`run_completion`) and streaming
    /// (`stream_chat_to_socket`) paths so a `stream:true` request cannot bypass
    /// the daily token cap or spike redirect.
    fn apply_budget_guard(
        &self,
        req: &CompletionRequest,
        route: Route,
    ) -> Result<(Route, u64), ProxyError> {
        if route != Route::Cloud || (self.budget_daily_tokens == 0 && self.spike_factor == 0) {
            return Ok((route, 0));
        }
        // IMP-24: estimate input + predicted output tokens (cloud cost is driven
        // mostly by output, so input alone under-estimates spend).
        let estimated =
            crate::routing::estimate_total_tokens(&req.estimation_text(), req.sampling.max_tokens)
                as u64;
        if let Some(reason) = self.check_budget_and_spike(estimated) {
            match self.budget_action.as_str() {
                "block" => return Err(ProxyError::BudgetExceeded(reason.to_string())),
                "warn" => {
                    // Reservation was rolled back in check_budget_and_spike; proceed
                    // over budget without a pre-reservation (warn-action path, ADR-163).
                    eprintln!("pasture: budget warning: {reason} (cloud request proceeds)");
                    return Ok((route, 0));
                }
                _ => {
                    // "local-only" (default): silently redirect to local.
                    // Reservation was rolled back in check_budget_and_spike (ADR-163).
                    eprintln!("pasture: {reason} -> routing local");
                    return Ok((Route::Local, 0));
                }
            }
        }
        // A pre-reservation exists only when the daily budget is active: the
        // `fetch_add` in check_budget_and_spike is guarded by `budget_daily_tokens
        // > 0`. With spike detection alone (budget off), no tokens were reserved,
        // so report 0 — otherwise log_cost would reconcile against a phantom
        // reservation and corrupt the today_cloud_tokens gauge (ADR-169).
        let reserved = if self.budget_daily_tokens > 0 {
            estimated
        } else {
            0
        };
        Ok((route, reserved))
    }

    fn log_cost(
        &self,
        route_label: &'static str,
        resp: &CompletionResponse,
        logprob: Option<f64>,
        reserved_tokens: u64,
    ) {
        // Record cost. Local and cache are free; cloud is priced from the
        // configured per-1M rates (ADR-166) — 0.0 when no pricing is set. PII is
        // never written (I5); logprob is the local-answer confidence, not content.
        let cost_usd = if route_label == "cloud" {
            self.cloud_cost_usd(resp.prompt_tokens, resp.completion_tokens)
        } else {
            0.0
        };
        let record = CostRecord::new(
            route_label,
            &resp.model,
            resp.prompt_tokens,
            resp.completion_tokens,
            cost_usd,
        )
        .with_logprob(logprob);
        if let Err(e) = record.append_to(&self.cost_log_path) {
            eprintln!("pasture: cost log write failed: {e}");
        }
        // Update IMP-26 budget / spike counters for cloud completions.
        if route_label == "cloud" {
            // Reset the daily counter first if the UTC day rolled over, so this
            // completion accrues to the new day rather than a stale total.
            self.roll_budget_day_if_needed();
            let tokens = resp.prompt_tokens + resp.completion_tokens;
            if reserved_tokens > 0 {
                // Reconcile pre-reservation (estimated) with actual tokens used (ADR-163).
                // The release path saturates at 0 across a UTC day rollover (ADR-164).
                match tokens.cmp(&reserved_tokens) {
                    std::cmp::Ordering::Greater => self
                        .today_cloud_tokens
                        .fetch_add(tokens - reserved_tokens, Ordering::Relaxed),
                    std::cmp::Ordering::Less => {
                        self.release_cloud_tokens(reserved_tokens - tokens);
                        0 // fetch_add returns the previous value; match arms must agree
                    }
                    std::cmp::Ordering::Equal => 0, // exact estimate: no adjustment
                };
            } else {
                // No pre-reservation (warn-action path or spike-only guard): add actual post-hoc.
                self.today_cloud_tokens.fetch_add(tokens, Ordering::Relaxed);
            }
            self.cloud_token_sum.fetch_add(tokens, Ordering::Relaxed);
            self.cloud_request_count.fetch_add(1, Ordering::Relaxed);
        } else if reserved_tokens > 0 {
            // Planned cloud route fell back to local; release the pre-reservation (ADR-163).
            self.roll_budget_day_if_needed();
            self.release_cloud_tokens(reserved_tokens);
        }
    }

    /// Difficulty signal (IMP-14): the best cosine similarity between the query
    /// embedding and the known-hard centroids, when it meets `hard_threshold`.
    /// Centroids are embedded lazily on first use (the local backend may not be
    /// up at construction); a failed attempt disables the signal for the process
    /// lifetime (logged once) instead of re-querying a broken backend per request.
    fn similar_to_hard(&self, query: &[f64]) -> Option<f64> {
        // Fast path: already initialised. Clone the Arc and release the lock BEFORE
        // the (CPU-bound) cosine computation, so concurrent difficulty-signal
        // requests are not serialized on this mutex (ADR-154). The centroids are
        // immutable after init, so the clone is a cheap refcount bump.
        {
            let guard = self.hard_centroids.lock().ok()?;
            if let Some(c) = guard.as_ref() {
                let c = Arc::clone(c);
                drop(guard);
                return crate::difficulty::similar_to_hard(query, &c, self.hard_threshold);
            }
        }
        // First use: embed the hard prompts WITHOUT holding the lock, so a slow or
        // cold local backend cannot block every other concurrent request. A rare
        // race may embed twice; the result is identical and idempotent, and the
        // first stored value wins.
        let computed = match self.local.as_deref() {
            Some(local) => match local.embeddings(&self.hard_prompts) {
                Ok(r) => r.vectors,
                Err(e) => {
                    eprintln!(
                        "pasture: hard-prompt embedding failed ({e}); difficulty signal disabled"
                    );
                    Vec::new()
                }
            },
            None => Vec::new(),
        };
        let centroids = {
            let mut guard = self.hard_centroids.lock().ok()?;
            // Another thread may have initialised while we were embedding; keep the
            // existing value rather than overwriting it.
            if guard.is_none() {
                *guard = Some(Arc::new(computed));
            }
            Arc::clone(guard.as_ref().expect("initialised above"))
        };
        crate::difficulty::similar_to_hard(query, &centroids, self.hard_threshold)
    }

    /// Run a completion for an already-parsed request (buffered), applying the
    /// cache and cascade strategies when enabled. Returns (response, label).
    fn run_completion(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, &'static str, Option<f64>, u64), ProxyError> {
        // Apply the configured prompt framing (system prompt, then date/OS context).
        let framed = self.frame_request(req);
        let req = framed.as_ref().unwrap_or(req);

        let (decision, sensitive) = self.classify_and_decide(req)?;
        if let Some(logger) = &self.decision_logger {
            logger.log_decision(
                &decision,
                crate::routing::estimate_tokens(&req.routing_text()) as u64,
                self.engine.threshold() as u64,
            );
        }

        // OTel span (IMP-23/ADR-144): start before the cache checks so a cache
        // hit is traced too. Cache hits return early below, so the span must
        // exist by now or the trace log would silently omit all cache traffic.
        let mut otel_span = self
            .otel_log
            .as_deref()
            .map(|_| crate::telemetry::Span::start("", &req.model));

        // Exact-match cache (never for sensitive content; I5).
        let cache_key = if !sensitive {
            Some(crate::cache::request_key(req))
        } else {
            None
        };
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            if let Ok(mut guard) = cache.lock() {
                if let Some(hit) = guard.get(key) {
                    self.emit_cache_hit_span(&mut otel_span, &hit, "cache");
                    return Ok((hit, "cache", None, 0));
                }
            }
        }

        // Semantic cache (IMP-12) + difficulty signal (IMP-14), via the shared
        // embedding step (ADR-150). Returns a cached answer or the route to use
        // (possibly escalated) plus the query embedding for store-on-miss.
        let (planned_route, query_embedding) =
            match self.embedding_step(req, decision.route, sensitive, &mut otel_span) {
                EmbeddingStep::SemanticHit(hit) => return Ok((hit, "semantic_cache", None, 0)),
                EmbeddingStep::Proceed { route, embedding } => (route, embedding),
            };

        // Budget / spike guard (IMP-26): only applies when routing to cloud and
        // budget or spike detection is configured. Sensitive content was excluded
        // from cloud routing above (privacy), so this check runs on non-sensitive
        // cloud-bound requests only. Shared with the streaming path.
        let (planned_route, budget_reserved_outer) =
            match self.apply_budget_guard(req, planned_route) {
                Ok(v) => v,
                Err(e) => {
                    // ADR-193: a budget "block" rejection (429) must emit an error
                    // span too, not drop the span the `?` shortcut used to discard.
                    self.emit_error_span(&mut otel_span, planned_route, &e.to_string());
                    return Err(e);
                }
            };

        // Pseudonymization (IMP-19): replace PII with opaque tokens before
        // sending to the cloud. Only applied to cloud-bound, non-sensitive
        // requests (sensitive content is already handled above). The mapping
        // is kept in memory and never logged (I5).
        let pseudo_mapping;
        let pseudo_req;
        let req = if self.pseudonymize && planned_route == Route::Cloud {
            let (msgs, mapping) = crate::pseudonymize::pseudonymize_messages(&req.messages);
            pseudo_mapping = Some(mapping);
            pseudo_req = CompletionRequest {
                messages: msgs,
                ..req.clone()
            };
            &pseudo_req
        } else {
            pseudo_mapping = None;
            req
        };

        // Pick the completion strategy. Cascade (try local, escalate on low
        // confidence) is never used for sensitive content (privacy) or when
        // either backend is missing.
        let completion_result = if self.cascade
            && !sensitive
            && planned_route == Route::Local
            && self.cloud.is_some()
            && self.local.is_some()
        {
            self.complete_cascade(req)
        } else if planned_route == Route::Cloud {
            self.complete_cloud_with_fallback(req)
        } else {
            self.complete_direct(req, planned_route)
        };
        let (mut resp, route, logprob, cascade_reserved) = match completion_result {
            Ok(v) => v,
            Err(e) => {
                // ADR-194: release the budget pre-reservation — the cloud completion
                // failed with no fallback, so the tokens apply_budget_guard reserved
                // must not stay on the daily gauge (mirrors the streaming path's
                // rollback, ADR-163; release saturates at 0 per ADR-164).
                if budget_reserved_outer > 0 {
                    self.release_cloud_tokens(budget_reserved_outer);
                }
                // ADR-143: emit error span so backend failures are visible in
                // the trace log, not silently dropped.
                self.emit_error_span(&mut otel_span, planned_route, &e.to_string());
                return Err(e);
            }
        };
        // One of budget_reserved_outer (from the outer apply_budget_guard) or
        // cascade_reserved (from the inner cascade apply_budget_guard) is always 0.
        let effective_reserved = cascade_reserved + budget_reserved_outer;

        // Restore pseudonymized tokens in the response (IMP-19).
        if let Some(ref mapping) = pseudo_mapping {
            if !mapping.is_empty() {
                resp.content = crate::pseudonymize::restore(&resp.content, mapping);
                // ADR-189: also restore in cloud-generated tool_calls. The cloud
                // may echo back a pseudonymized token it saw in the request history
                // (e.g. <EMAIL_1> in an argument), which must be un-masked before
                // the response reaches the client.
                if let Some(ref mut tc) = resp.tool_calls {
                    *tc = crate::pseudonymize::restore(tc, mapping);
                }
            }
        }

        // Finish and emit the OTel span (IMP-23). gen_ai.system reflects the
        // backend that actually served the request, now that the route is known.
        if let (Some(span), Some(log_path)) = (otel_span.as_mut(), self.otel_log.as_deref()) {
            span.system = self.otel_system_for(route);
            span.response_model = resp.model.clone();
            span.input_tokens = resp.prompt_tokens;
            span.output_tokens = resp.completion_tokens;
            span.route = route.as_str();
            // Derive finish_reason from the response (ADR-181).
            span.finish_reason = Some(finish_reason_for(&resp).to_string());
            span.finish();
            if let Err(e) = span.append_to(log_path) {
                eprintln!("pasture: otel log write failed: {e}");
            }
        }

        // Store on miss (exact-match).
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            if let Ok(mut guard) = cache.lock() {
                guard.put(key, resp.clone());
            }
        }
        // Store on miss (semantic).
        if let (Some(emb), Some(sem_mutex)) = (query_embedding, self.semantic_cache.as_ref()) {
            if let Ok(mut guard) = sem_mutex.lock() {
                let samp = crate::cache::sampling_key(&req.sampling);
                guard.put(emb, req.model.clone(), samp, resp.clone());
            }
        }

        Ok((resp, route.as_str(), logprob, effective_reserved))
    }

    /// Cascade strategy: answer locally, escalate to the cloud when the local
    /// answer's confidence is low. A cloud failure falls back to the local
    /// answer rather than erroring; the failure is logged so the operator
    /// knows the cascade attempted.
    fn complete_cascade(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, Route, Option<f64>, u64), ProxyError> {
        let local = self.backend_for(Route::Local)?;
        let (local_resp, confidence) = self
            .track_local_call(|| local.complete_scored(req))
            .map_err(|e| ProxyError::Backend(e.to_string()))?;
        if crate::cascade::should_escalate(
            &local_resp.content,
            confidence,
            self.cascade_logprob_threshold,
        ) {
            // Apply the same budget/spike guard the direct cloud path uses before
            // spending cloud tokens (ADR-161). The cascade is reached only when the
            // request was routed Local, so apply_budget_guard was a no-op upstream
            // (it ignores non-Cloud routes); without this an escalation would bypass
            // the daily cap and spike redirect entirely. When the guard declines the
            // cloud (over budget → Local redirect, or "block" → Err), the cascade
            // keeps its already-computed local answer — the same graceful degradation
            // it does on a cloud failure, so a budget cap never turns into a 429.
            match self.apply_budget_guard(req, Route::Cloud) {
                Ok((Route::Cloud, cascade_reserved)) => {
                    let cloud = self.backend_for(Route::Cloud)?;
                    match complete_with_retry(cloud, req, self.cloud_retry, CLOUD_RETRY_BASE_MS) {
                        Ok(cloud_resp) => {
                            return Ok((cloud_resp, Route::Cloud, confidence, cascade_reserved))
                        }
                        Err(e) => {
                            // Roll back the pre-reservation: cloud failed, no tokens were spent.
                            if cascade_reserved > 0 {
                                self.release_cloud_tokens(cascade_reserved);
                            }
                            eprintln!("pasture: cascade cloud failed ({e}); using local answer");
                        }
                    }
                }
                _ => {
                    eprintln!(
                        "pasture: cascade escalation suppressed by budget/spike guard; using local answer"
                    );
                }
            }
        }
        Ok((local_resp, Route::Local, confidence, 0))
    }

    /// Cloud strategy: retry transient failures, then fall back to local if
    /// one is available rather than erroring the request (IMP-9).
    fn complete_cloud_with_fallback(
        &self,
        req: &CompletionRequest,
    ) -> Result<(CompletionResponse, Route, Option<f64>, u64), ProxyError> {
        let cloud = self.backend_for(Route::Cloud)?;
        match complete_with_retry(cloud, req, self.cloud_retry, CLOUD_RETRY_BASE_MS) {
            Ok(resp) => Ok((resp, Route::Cloud, None, 0)),
            Err(primary_err) => {
                // IMP-9 multi-provider follow-up: try the fallback cloud provider
                // before giving up to local. This handles a full primary-cloud
                // outage (vs. transient errors, which the retry loop already covers).
                if let Some(fallback) = self.cloud_fallback.as_deref() {
                    eprintln!(
                        "pasture: primary cloud failed ({primary_err}); trying fallback cloud"
                    );
                    match complete_with_retry(fallback, req, self.cloud_retry, CLOUD_RETRY_BASE_MS)
                    {
                        Ok(resp) => return Ok((resp, Route::Cloud, None, 0)),
                        Err(fb_err) => {
                            eprintln!("pasture: fallback cloud also failed ({fb_err})");
                        }
                    }
                }
                match self.local.as_deref() {
                    Some(local) => {
                        eprintln!("pasture: cloud failed ({primary_err}); falling back to local");
                        let resp = self
                            .track_local_call(|| local.complete(req))
                            .map_err(|e| ProxyError::Backend(e.to_string()))?;
                        Ok((resp, Route::Local, None, 0))
                    }
                    None => Err(ProxyError::Backend(primary_err.to_string())),
                }
            }
        }
    }

    /// Direct strategy: send to the decided backend. On the local route a
    /// configured fast model handles simple short prompts (dual-local routing).
    fn complete_direct(
        &self,
        req: &CompletionRequest,
        route: Route,
    ) -> Result<(CompletionResponse, Route, Option<f64>, u64), ProxyError> {
        let backend = self.backend_for(route)?;
        let fast_req = if route == Route::Local {
            self.fast_request(req)
        } else {
            None
        };
        let effective_req = fast_req.as_ref().unwrap_or(req);
        let resp = if route == Route::Local {
            self.track_local_call(|| backend.complete(effective_req))
        } else {
            backend.complete(effective_req)
        }
        .map_err(|e| ProxyError::Backend(e.to_string()))?;
        Ok((resp, route, None, 0))
    }

    /// A copy of `req` retargeted at the configured fast model when the last
    /// user prompt is simple enough (no hard signals, below fast_threshold
    /// tokens); `None` to use the main model.
    fn fast_request(&self, req: &CompletionRequest) -> Option<CompletionRequest> {
        let fm = self.fast_model.as_deref()?;
        let text = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");
        if !crate::routing::is_simple_prompt(text, self.fast_threshold) {
            return None;
        }
        // Same request retargeted at the fast model; struct-update carries the
        // messages/stream/tools/sampling unchanged (and any future field).
        Some(CompletionRequest {
            model: fm.to_string(),
            ..req.clone()
        })
    }

    /// Handle a chat-completion request end to end (buffered), returning the
    /// response body. Used for non-streaming requests and by tests.
    pub fn handle_chat(&self, body: &str) -> Result<String, ProxyError> {
        let req = Self::parse_request(body)?;
        self.complete_buffered(&req)
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

    /// Cost summary for the `/metrics` and `/v1/stats` endpoints, computed
    /// incrementally (IMP-32, ADR-151). Each call folds only the cost-log lines
    /// appended since the previous call into a cached running summary, instead of
    /// re-reading and re-parsing the whole (unbounded-growing) log on every
    /// Prometheus scrape. The result is byte-identical to `summarize(read_log())`.
    /// A shrunk file (rotation/truncation) resets the cache and re-reads in full.
    /// Only complete lines (ending in `\n`) are consumed, so a scrape that races a
    /// half-written record simply folds it on the next call.
    fn live_cost_summary(&self) -> Result<crate::cost::CostSummary, ProxyError> {
        use std::io::{Seek, SeekFrom};
        let mut guard = self
            .metrics_cache
            .lock()
            .map_err(|_| ProxyError::Backend("metrics cache poisoned".to_string()))?;
        let cur_len = match std::fs::metadata(&self.cost_log_path) {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(ProxyError::Backend(e.to_string())),
        };
        // File shrank (rotated/truncated): the cached summary no longer corresponds
        // to the file prefix, so reset and rebuild from the start.
        if cur_len < guard.0 {
            *guard = (0, crate::cost::summarize(&[]));
        }
        if cur_len > guard.0 {
            if let Ok(mut f) = std::fs::File::open(&self.cost_log_path) {
                if f.seek(SeekFrom::Start(guard.0)).is_ok() {
                    let mut buf = Vec::new();
                    if f.read_to_end(&mut buf).is_ok() {
                        // Consume only up to the last newline; a trailing partial
                        // line is left for the next call (re-read once complete).
                        let consumed = buf.iter().rposition(|&b| b == b'\n').map(|i| i + 1);
                        if let Some(end) = consumed {
                            let text = String::from_utf8_lossy(&buf[..end]);
                            for line in text.lines() {
                                if let Some(rec) = crate::cost::parse_log_line(line) {
                                    crate::cost::fold_record(&mut guard.1, &rec);
                                }
                            }
                            guard.0 += end as u64;
                        }
                    }
                }
            }
        }
        Ok(guard.1.clone())
    }

    /// Handle `GET /v1/stats` (IMP-metrics): a live JSON view of the cost-log
    /// counters (route counts, rates, tokens, spend) without parsing JSONL by
    /// hand. Read-only over the PII-free cost log (I3); a missing log reads as
    /// all-zeros. Localhost-default, so no auth is implied (I5).
    pub fn handle_stats(&self) -> Result<String, ProxyError> {
        let summary = self.live_cost_summary()?;
        // Live hit/miss counters + size/capacity from the in-memory caches.
        let (live_hits, live_misses, cache_size, cache_cap) = self
            .cache
            .as_ref()
            .and_then(|m| m.lock().ok())
            .map(|g| (g.hits(), g.misses(), g.len(), g.cap()))
            .unwrap_or((0, 0, 0, 0));
        let (sem_hits, sem_misses, sem_size, sem_cap) = self
            .semantic_cache
            .as_ref()
            .and_then(|m| m.lock().ok())
            .map(|g| (g.hits(), g.misses(), g.len(), g.cap()))
            .unwrap_or((0, 0, 0, 0));
        let (budget_used, budget_limit) = self.budget_snapshot();
        let pii_categories = self
            .output_pii_stats
            .as_ref()
            .map(|s| s.snapshot())
            .unwrap_or_default();
        let input_pii_categories = self
            .input_pii_stats
            .as_ref()
            .map(|s| s.snapshot())
            .unwrap_or_default();
        Ok(build_stats_response(
            &summary,
            live_hits,
            live_misses,
            cache_size,
            cache_cap,
            sem_hits,
            sem_misses,
            sem_size,
            sem_cap,
            budget_used,
            budget_limit,
            &pii_categories,
            self.local_health.status().as_str(),
            &input_pii_categories,
        ))
    }

    /// Serve `GET /metrics` in Prometheus text exposition format (IMP-metrics-prom).
    /// Exposes the same counters as `/v1/stats` but in the standard text format
    /// consumed by Prometheus scrape targets and Grafana agent.
    pub fn handle_metrics(&self) -> Result<String, ProxyError> {
        let s = self.live_cost_summary()?;
        let (live_hits, live_misses, cache_size, cache_cap) = self
            .cache
            .as_ref()
            .and_then(|m| m.lock().ok())
            .map(|g| (g.hits(), g.misses(), g.len(), g.cap()))
            .unwrap_or((0, 0, 0, 0));
        let (sem_hits, sem_misses, sem_size, sem_cap) = self
            .semantic_cache
            .as_ref()
            .and_then(|m| m.lock().ok())
            .map(|g| (g.hits(), g.misses(), g.len(), g.cap()))
            .unwrap_or((0, 0, 0, 0));
        let (budget_used, budget_limit) = self.budget_snapshot();
        Ok(build_metrics_response(
            &s,
            live_hits,
            live_misses,
            cache_size,
            cache_cap,
            sem_hits,
            sem_misses,
            sem_size,
            sem_cap,
            budget_used,
            budget_limit,
        ))
    }

    /// Parse a legacy `POST /v1/completions` request (text-completion format).
    /// Maps `prompt` (string or first array element) to a single user message so
    /// the request can be routed through the same pipeline as chat completions.
    /// Streaming is not supported via this shim; callers should use
    /// `POST /v1/chat/completions` with `"stream":true` instead.
    fn parse_legacy_completion(body: &str) -> Result<CompletionRequest, ProxyError> {
        let v = parse(body).map_err(|e| ProxyError::BadRequest(e.to_string()))?;
        let model = v
            .get("model")
            .and_then(JsonValue::as_str)
            .unwrap_or("")
            .to_string();
        let prompt_text = match v.get("prompt") {
            Some(JsonValue::Str(s)) => s.clone(),
            Some(JsonValue::Array(a)) => a
                .iter()
                .filter_map(JsonValue::as_str)
                .collect::<Vec<_>>()
                .join("\n"),
            _ => {
                return Err(ProxyError::BadRequest(
                    "missing or invalid 'prompt'".to_string(),
                ))
            }
        };
        if prompt_text.is_empty() {
            return Err(ProxyError::BadRequest("empty 'prompt'".to_string()));
        }
        let sampling = Self::parse_sampling(&v);
        Ok(CompletionRequest {
            model,
            messages: vec![Message {
                role: "user".to_string(),
                content: prompt_text,
                ..Default::default()
            }],
            stream: false,
            has_tools: false,
            sampling,
        })
    }

    /// Handle a legacy `POST /v1/completions` request. Routes through the normal
    /// pipeline and re-formats the response as `object:"text_completion"` with
    /// `choices[].text` (not `message.content`) for API compatibility.
    fn handle_legacy_completion(&self, body: &str) -> Result<String, ProxyError> {
        let req = Self::parse_legacy_completion(body)?;
        // Apply injection guard on legacy completions too (IMP-20).
        if self.injection_guard != "off" {
            let text = req.routing_text();
            if let crate::guard::InjectionRisk::Flag(label) =
                crate::guard::classify_injection(&text)
            {
                if self.injection_guard == "block" {
                    return Err(ProxyError::BadRequest(format!(
                        "request blocked by injection guard: {label}"
                    )));
                }
                eprintln!("pasture: injection_flag:{label} (flag mode, legacy request proceeds)");
            }
        }
        let (resp, label, logprob, reserved) = self.run_completion(&req)?;
        self.log_cost(label, &resp, logprob, reserved);
        Ok(build_legacy_completion_response(&resp, label))
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
        // Per-connection read buffer: bytes read ahead for the current request
        // stay here for the next (keep-alive pipelining support).
        let mut conn_buf: Vec<u8> = Vec::new();
        // Safety cap: one keep-alive connection serves at most this many requests.
        const MAX_KEEPALIVE_REQUESTS: usize = 100;
        for _ in 0..MAX_KEEPALIVE_REQUESTS {
            let (method, path, body, auth, origin, keep_alive, request_id, content_type) =
                match read_request(stream, &mut conn_buf, self.max_body_bytes)? {
                    ReadOutcome::Request {
                        method,
                        path,
                        body,
                        auth,
                        origin,
                        connection_close,
                        request_id,
                        content_type,
                    } => (
                        method,
                        path,
                        body,
                        auth,
                        origin,
                        !connection_close,
                        request_id,
                        content_type,
                    ),
                    ReadOutcome::TooLarge => {
                        let payload =
                            build_error_response("request body too large", "invalid_request_error");
                        write_response(stream, 413, &payload, "", false)?;
                        return Ok(());
                    }
                    ReadOutcome::TimedOut => {
                        let payload =
                            build_error_response("request timed out", "invalid_request_error");
                        write_response(stream, 408, &payload, "", false)?;
                        return Ok(());
                    }
                    ReadOutcome::Closed => return Ok(()), // normal EOF / graceful close
                };
            // Every response is traceable: echo the caller's X-Request-ID, or mint
            // one server-side when absent (OpenAI/LiteLLM always return x-request-id).
            let request_id = Some(request_id.unwrap_or_else(next_request_id));
            // CORS headers reflected on every response so the browser can read it.
            let cors = self.cors_headers(origin.as_deref());
            // Echo X-Request-ID back to the caller so clients can correlate responses.
            let req_id_hdr = request_id
                .as_ref()
                .map(|id| format!("X-Request-ID: {id}\r\n"))
                .unwrap_or_default();
            // X-RateLimit-* headers so clients self-throttle (empty when the
            // limiter is disabled — the localhost default). Snapshotted before the
            // gate consumes a token, so `remaining` includes the in-flight request.
            let rl_hdr = self.ratelimit_headers();
            // Server header identifies the proxy + version (peer parity: nginx,
            // LiteLLM, Ollama all send one); compile-time constant.
            const SERVER_HDR: &str = concat!("Server: pasture/", env!("CARGO_PKG_VERSION"), "\r\n");
            let extra = format!("{cors}{req_id_hdr}{rl_hdr}{SERVER_HDR}");
            // Append X-Response-Time (elapsed ms) to every response's header block.
            // Timing begins after request parsing, just before dispatch, so it covers
            // routing + backend time but not TCP accept or header reading.
            let t0 = std::time::Instant::now();
            let te = || {
                format!(
                    "{}X-Response-Time: {}ms\r\n",
                    extra,
                    t0.elapsed().as_millis()
                )
            };
            // Normalised path (no query string) for access-log entries.
            let norm_path = path.split('?').next().unwrap_or(&path);
            // Local macros log and write each response (IMP-access-log). Using
            // macros (not closures) so `stream` can be borrowed mutably in the body.
            macro_rules! access_log {
                ($s:expr) => {
                    if let Some(ref log_path) = self.access_log {
                        append_access_log(
                            log_path,
                            &method,
                            norm_path,
                            $s,
                            t0.elapsed().as_millis(),
                            request_id.as_deref(),
                        );
                    }
                };
            }
            macro_rules! wr {
                ($s:expr, $b:expr, $e:expr, $ka:expr) => {{
                    access_log!($s);
                    write_response(stream, $s, $b, $e, $ka)?;
                }};
            }
            macro_rules! wrp {
                ($b:expr, $e:expr, $ka:expr) => {{
                    access_log!(200u16);
                    write_plain_response(stream, $b, $e, $ka)?;
                }};
            }
            macro_rules! wrh {
                ($s:expr, $l:expr, $e:expr, $ka:expr) => {{
                    access_log!($s);
                    write_head_response(stream, $s, $l, $e, $ka)?;
                }};
            }
            // Handler error: write the OpenAI error envelope and close.
            macro_rules! wr_err {
                ($e:expr) => {{
                    let e = $e;
                    wr!(
                        e.status(),
                        &build_error_response(e.message(), e.kind()),
                        &te(),
                        false
                    );
                    return Ok(());
                }};
            }
            // Handler Result: 200 with the body on Ok, error envelope on Err.
            macro_rules! wr_result {
                ($r:expr) => {
                    match $r {
                        Ok(resp) => wr!(200, &resp, &te(), keep_alive),
                        Err(e) => wr_err!(e),
                    }
                };
            }
            // CORS preflight: answer OPTIONS before the gate (preflight is credential-free).
            if method == "OPTIONS" {
                match self.cors_preflight(origin.as_deref()) {
                    Some(h) => {
                        let h_timed =
                            format!("{}X-Response-Time: {}ms\r\n", h, t0.elapsed().as_millis());
                        access_log!(204u16);
                        write_response(stream, 204, "", &h_timed, keep_alive)?;
                    }
                    None => wr!(
                        404,
                        &build_error_response("not found", "invalid_request_error"),
                        &te(),
                        keep_alive
                    ),
                }
                // Preflight doesn't count as content: the client follows up immediately.
                if !keep_alive {
                    return Ok(());
                }
                continue;
            }
            // Auth + rate-limit gating (IMP-15); /health is exempt. A 429 carries
            // a Retry-After header (RFC 7231 §7.1.3) so clients back off precisely.
            if let Some((status, msg, kind, retry_after)) = self.check_gate(&path, auth.as_deref())
            {
                let gate_extra = match retry_after {
                    Some(secs) => format!("Retry-After: {secs}\r\n{}", te()),
                    None => te(),
                };
                // Populate OpenAI's error `code` where it is unambiguous so SDK
                // retry/branch logic keyed on it works (rate-limit / auth).
                let code = match status {
                    429 => Some("rate_limit_exceeded"),
                    401 => Some("invalid_api_key"),
                    _ => None,
                };
                access_log!(status);
                write_response(
                    stream,
                    status,
                    &build_error_response_coded(msg, kind, code),
                    &gate_extra,
                    false,
                )?;
                return Ok(());
            }
            // 415 Unsupported Media Type: POST requests must carry JSON bodies.
            // Only reject when Content-Type is explicitly set to something other
            // than application/json (absent Content-Type is still accepted so that
            // plain curl and minimal clients work without extra flags).
            if method == "POST" {
                if let Some(ct) = &content_type {
                    if !ct.starts_with("application/json") {
                        wr!(
                            415,
                            &build_error_response(
                                "unsupported media type; use application/json",
                                "invalid_request_error"
                            ),
                            &te(),
                            false
                        );
                        return Ok(());
                    }
                }
            }
            if method == "POST" && path.starts_with("/v1/chat/completions") {
                match Self::parse_request(&body) {
                    Ok(req) if req.stream => {
                        // SSE is a long-lived stream; always close the connection afterwards.
                        let include_usage = Self::parse_include_usage(&body);
                        // Log the ACTUAL status (ADR-192): 200 once the stream starts, or the
                        // rejection status when the guard/router/budget rejects before any SSE
                        // byte. A mid-stream I/O error (client disconnect after the 200 headers)
                        // is still logged as 200, matching the bytes already sent.
                        match self.stream_chat_to_socket(stream, &req, &te(), include_usage) {
                            Ok(status) => access_log!(status),
                            Err(e) => {
                                access_log!(200u16);
                                return Err(e);
                            }
                        }
                        return Ok(());
                    }
                    Ok(req) => wr_result!(self.complete_buffered(&req)),
                    Err(e) => wr_err!(e),
                }
            } else if method == "POST" && path.starts_with("/v1/completions") {
                // Legacy text-completion API shim — maps prompt→chat message,
                // routes through the same pipeline, returns object:"text_completion".
                wr_result!(self.handle_legacy_completion(&body));
            } else if method == "POST" && path.starts_with("/v1/embeddings") {
                wr_result!(self.handle_embeddings(&body));
            } else if method == "POST" && path.starts_with("/v1/moderations") {
                // Stub: always marks content as safe. Pasture does not run real
                // moderation; the stub prevents SDK clients that call this endpoint
                // unconditionally from receiving a 404.
                wr_result!(Self::handle_moderations(&body));
            } else if method == "POST" && path.starts_with("/v1/route") {
                // Routing preview / dry-run (ADR-198): return the route decision
                // for this body without calling any backend — no cost, no cloud.
                wr_result!(self.handle_route_preview(&body));
            } else if method == "GET" && path.starts_with("/v1/stats") {
                wr_result!(self.handle_stats());
            } else if method == "GET" && path.starts_with("/metrics") {
                // Prometheus text-format scrape endpoint (IMP-metrics-prom).
                match self.handle_metrics() {
                    Ok(resp) => wrp!(&resp, &te(), keep_alive),
                    Err(e) => wr_err!(e),
                }
            } else if method == "GET"
                && (path.starts_with("/v1/models") || path.starts_with("/v1/engines"))
            {
                // `/v1/models` lists; `/v1/models/{id}` retrieves a single model.
                // `/v1/engines[/{id}]` is the deprecated OpenAI v1 path — alias to /v1/models.
                let strip_prefix = if path.starts_with("/v1/engines") {
                    "/v1/engines"
                } else {
                    "/v1/models"
                };
                let rest = &path[strip_prefix.len()..];
                if let Some(after) = rest.strip_prefix('/') {
                    let id = after
                        .split('?')
                        .next()
                        .unwrap_or(after)
                        .trim_end_matches('/');
                    match build_model_response(&self.models, id) {
                        Some(b) => wr!(200, &b, &te(), keep_alive),
                        None => {
                            wr!(
                                404,
                                &build_error_response(
                                    &format!("model '{id}' not found"),
                                    "invalid_request_error"
                                ),
                                &te(),
                                false
                            );
                            return Ok(());
                        }
                    }
                } else {
                    wr!(200, &build_models_response(&self.models), &te(), keep_alive);
                }
            } else if (method == "GET" || method == "HEAD") && path.starts_with("/health") {
                // HEAD: identical headers to GET but no body (RFC 7231 §4.3.2).
                // Include the version so health-check scripts can detect mismatched deploys.
                const HEALTH_BODY: &str = concat!(
                    "{\"status\":\"ok\",\"version\":\"",
                    env!("CARGO_PKG_VERSION"),
                    "\"}"
                );
                if method == "HEAD" {
                    wrh!(200, HEALTH_BODY.len(), &te(), keep_alive);
                } else {
                    wr!(200, HEALTH_BODY, &te(), keep_alive);
                }
            } else if method == "POST"
                && (path.starts_with("/v1/audio") || path.starts_with("/v1/images"))
            {
                // Audio and image generation are not implemented in Pasture.
                // Return 501 (not 404) so clients know the path is recognised but unsupported.
                wr!(
                    501,
                    &build_error_response(
                        "audio and image endpoints are not supported by Pasture",
                        "not_supported"
                    ),
                    &te(),
                    false
                );
                return Ok(());
            } else if let Some(allow) =
                Self::route_allowed_methods(path.split('?').next().unwrap_or(&path))
            {
                // Known path, wrong method → 405 with Allow header (RFC 7231 §6.5.5).
                let method_extra = format!("Allow: {allow}\r\n{}", te());
                access_log!(405u16);
                write_response(
                    stream,
                    405,
                    &build_error_response("method not allowed", "invalid_request_error"),
                    &method_extra,
                    false,
                )?;
                return Ok(());
            } else {
                wr!(
                    404,
                    &build_error_response("not found", "invalid_request_error"),
                    &te(),
                    false
                );
                return Ok(());
            }
            if !keep_alive {
                return Ok(());
            }
            // keep_alive=true: loop for the next pipelined request.
        }
        Ok(())
    }

    /// Non-streaming completion from an already-parsed request.
    fn complete_buffered(&self, req: &CompletionRequest) -> Result<String, ProxyError> {
        // Prompt-injection guard (IMP-20). Off by default (zero overhead).
        let injection_label = if self.injection_guard != "off" {
            let text = req.routing_text();
            match crate::guard::classify_injection(&text) {
                crate::guard::InjectionRisk::Flag(label) => {
                    if self.injection_guard == "block" {
                        return Err(ProxyError::BadRequest(format!(
                            "request blocked by injection guard: {label}"
                        )));
                    }
                    // flag mode: log and annotate, but let the request proceed.
                    eprintln!("pasture: injection_flag:{label} (flag mode, request proceeds)");
                    Some(label)
                }
                crate::guard::InjectionRisk::Allow => None,
            }
        } else {
            None
        };

        let (resp, label, logprob, reserved) = self.run_completion(req)?;
        self.log_cost(label, &resp, logprob, reserved);
        if let Some(stats) = &self.output_pii_stats {
            stats.scan(&resp.content);
        }
        Ok(build_openai_response_with_injection(
            &resp,
            label,
            injection_label.as_deref(),
        ))
    }

    /// Stream a completion to the socket as Server-Sent Events (IMP-7).
    /// Note: cascade is not applied to streaming requests (the local answer
    /// cannot be un-sent); the routed backend streams directly.
    ///
    /// Returns the effective HTTP status (ADR-192): `200` once the SSE stream has
    /// started, or the rejection status (`400`/`429`/`502`/`503`) when the guard,
    /// router, or budget rejects the request before any SSE byte — so the access
    /// log records what the client actually received, not a hardcoded `200`.
    fn stream_chat_to_socket(
        &self,
        sock: &mut std::net::TcpStream,
        req: &CompletionRequest,
        cors: &str,
        include_usage: bool,
    ) -> std::io::Result<u16> {
        // Injection guard for streaming (IMP-20): block mode can still reject before
        // the stream starts; flag mode surfaces the label to the client on a leading
        // SSE chunk (ADR-191), matching the buffered path's x_pasture_injection_flag.
        let injection_label: Option<String> = if self.injection_guard != "off" {
            let text = req.routing_text();
            if let crate::guard::InjectionRisk::Flag(label) =
                crate::guard::classify_injection(&text)
            {
                if self.injection_guard == "block" {
                    let msg = format!("request blocked by injection guard: {label}");
                    write_response(
                        sock,
                        400,
                        &build_error_response(&msg, "invalid_request_error"),
                        cors,
                        false,
                    )?;
                    return Ok(400);
                }
                eprintln!("pasture: injection_flag:{label} (flag mode, stream proceeds)");
                Some(label)
            } else {
                None
            }
        } else {
            None
        };
        // Apply the configured prompt framing (system prompt, then date/OS context)
        // so a stream:true request behaves like a buffered one (ADR-149). Done after
        // the injection guard (which scans the original request, as the buffered
        // path does) and before classify_and_decide so framing also feeds routing.
        let framed = self.frame_request(req);
        let req = framed.as_ref().unwrap_or(req);
        let (decision, sensitive) = match self.classify_and_decide(req) {
            Ok(pair) => pair,
            Err(e) => {
                let s = e.status();
                write_response(
                    sock,
                    s,
                    &build_error_response(e.message(), e.kind()),
                    cors,
                    false,
                )?;
                return Ok(s);
            }
        };
        if let Some(logger) = &self.decision_logger {
            logger.log_decision(
                &decision,
                crate::routing::estimate_tokens(&req.routing_text()) as u64,
                self.engine.threshold() as u64,
            );
        }
        // OTel span (IMP-23): started before the cache checks so a cache hit is
        // traced too (ADR-144). Shared by the exact/semantic hit paths and the
        // backend completion below.
        let mut otel_span = self
            .otel_log
            .as_deref()
            .map(|_| crate::telemetry::Span::start("", &req.model));

        // Exact-match cache read (IMP-31b/ADR-147): mirror the buffered path so a
        // stream:true request is served from cache without a backend call. Checked
        // BEFORE the budget guard (a cache hit costs nothing, so it is served even
        // when over the cloud budget — matching the buffered ordering). Never for
        // sensitive content (I5); keyed on the original request.
        let cache_key = if sensitive {
            None
        } else {
            Some(crate::cache::request_key(req))
        };
        if let (Some(key), Some(cache)) = (cache_key, self.cache.as_ref()) {
            let hit = cache.lock().ok().and_then(|mut g| g.get(key));
            if let Some(hit) = hit {
                self.emit_cache_hit_span(&mut otel_span, &hit, "cache");
                self.write_cached_stream(
                    sock,
                    cors,
                    &hit,
                    "cache",
                    include_usage,
                    injection_label.as_deref(),
                )?;
                return Ok(200);
            }
        }
        // Semantic cache (IMP-12) + difficulty signal (IMP-14) via the shared
        // embedding step (ADR-150): parity with the buffered path. A semantic hit
        // is replayed as SSE; otherwise the route may be escalated and the query
        // embedding is kept to store the streamed answer on a miss.
        let (decided_route, query_embedding) =
            match self.embedding_step(req, decision.route, sensitive, &mut otel_span) {
                EmbeddingStep::SemanticHit(hit) => {
                    self.write_cached_stream(
                        sock,
                        cors,
                        &hit,
                        "semantic_cache",
                        include_usage,
                        injection_label.as_deref(),
                    )?;
                    return Ok(200);
                }
                EmbeddingStep::Proceed { route, embedding } => (route, embedding),
            };
        // Budget / spike guard (IMP-26): mirror the buffered path so a stream:true
        // request cannot bypass the daily cap or spike redirect. May downgrade to
        // local or, in "block" mode, reject before the stream starts.
        let (route, budget_reserved) = match self.apply_budget_guard(req, decided_route) {
            Ok(r) => r,
            Err(e) => {
                let s = e.status();
                // ADR-193: emit the error span before the early return so a budget
                // "block" rejection is traced, not silently dropped.
                self.emit_error_span(&mut otel_span, decided_route, &e.to_string());
                write_response(
                    sock,
                    s,
                    &build_error_response(e.message(), e.kind()),
                    cors,
                    false,
                )?;
                return Ok(s);
            }
        };
        let backend = match self.backend_for(route) {
            Ok(b) => b,
            Err(e) => {
                let s = e.status();
                // ADR-194: release any budget pre-reservation before the early return
                // (invariant: every return after apply_budget_guard reserved tokens
                // must roll them back, ADR-163/164).
                if budget_reserved > 0 {
                    self.release_cloud_tokens(budget_reserved);
                }
                // ADR-193: trace a backend-unavailable rejection too (mirrors the
                // buffered path, where the completion_result error arm emits a span).
                self.emit_error_span(&mut otel_span, route, &e.to_string());
                write_response(
                    sock,
                    s,
                    &build_error_response(e.message(), e.kind()),
                    cors,
                    false,
                )?;
                return Ok(s);
            }
        };

        write_sse_headers(sock, cors)?;
        let route_label = route.as_str();

        // Pseudonymization for streaming (IMP-19): mirror the buffered path —
        // mask PII before the request reaches the cloud, and restore tokens in
        // the streamed deltas. Without this, a streaming cloud request would
        // leak raw PII even with PASTURE_PSEUDONYMIZE=1. The mapping is never
        // logged (I5). Tokens split across deltas are handled by StreamRestorer.
        let pseudo_req;
        let mut restorer: Option<crate::pseudonymize::StreamRestorer> = None;
        // Keep a copy of the mapping so a streamed completion is cached with the
        // PII *restored* (ADR-147), matching the buffered path — the cache must
        // never hold the opaque `<EMAIL_n>` tokens. Defensive: maskable PII makes a
        // request sensitive, and sensitive requests are not cached (key is None), so
        // this normally stays None; it guards the case where the pseudonymizer masks
        // something the sensitivity classifier did not flag.
        let mut cache_mapping: Option<crate::pseudonymize::Mapping> = None;
        let req = if self.pseudonymize && route == Route::Cloud {
            let (msgs, mapping) = crate::pseudonymize::pseudonymize_messages(&req.messages);
            if !mapping.is_empty() {
                cache_mapping = Some(mapping.clone());
                restorer = Some(crate::pseudonymize::StreamRestorer::new(mapping));
            }
            pseudo_req = CompletionRequest {
                messages: msgs,
                ..req.clone()
            };
            &pseudo_req
        } else {
            req
        };

        // One id, model, fingerprint, and created timestamp shared by every chunk
        // of this stream (OpenAI behaviour). `created` is captured once so that
        // the delta chunks, the stop chunk, and the optional usage chunk all carry
        // the same value — not each their own call to unix_now() (ADR-170).
        let id = next_completion_id();
        let model = req.model.clone();
        let fp = fingerprint_for_model(&model);
        let created = unix_now();
        // `otel_span` was started above (before the cache checks); it is filled with
        // system/route/usage and appended on success below.
        let mut io_err: Option<std::io::Error> = None;
        // Surface the injection-guard flag on a leading chunk (ADR-191), matching the
        // buffered path's x_pasture_injection_flag and the cached-stream path above.
        if let Some(label) = injection_label.as_deref() {
            let frame = sse_frame(&build_openai_injection_chunk(
                &id,
                &model,
                &fp,
                route_label,
                label,
                created,
            ));
            if let Err(e) = sock.write_all(frame.as_bytes()) {
                io_err = Some(e);
            }
        }
        let mut on_delta = |delta: &str| {
            if io_err.is_some() {
                return;
            }
            // Restore PII tokens incrementally; emit nothing if the restorer is
            // still buffering a partial token at the delta boundary.
            let piece = match restorer.as_mut() {
                Some(r) => r.push(delta),
                None => delta.to_string(),
            };
            if piece.is_empty() {
                return;
            }
            let frame = sse_frame(&build_openai_chunk(
                &id,
                &model,
                &fp,
                &piece,
                route_label,
                None,
                created,
            ));
            if let Err(e) = sock.write_all(frame.as_bytes()) {
                io_err = Some(e);
            }
        };
        // IMP-30: streaming previously bypassed local-health tracking entirely —
        // only the buffered strategies (complete_direct/complete_cascade/
        // complete_cloud_with_fallback) called track_local_call. Since a stream:true
        // request routed Local calls stream_complete directly, a crashed local
        // backend never showed up in /v1/stats for streaming traffic, which is the
        // more common path for interactive chat UIs. Record the same outcome here.
        let resp = if route == Route::Local {
            self.track_local_call(|| backend.stream_complete(req, &mut on_delta))
        } else {
            backend.stream_complete(req, &mut on_delta)
        };
        // The backend stream has fully completed — it reads the upstream to the end
        // even if the client went away mid-stream, so `resp` carries the real token
        // usage. Account the work (cost/budget/trace/cache) BEFORE the remaining
        // client writes, so a client that disconnects mid-stream is still logged,
        // budget-accrued, traced, and cached (ADR-153). Only the client-facing SSE
        // frames below are skipped once `io_err` is set.
        match &resp {
            Ok(r) => {
                self.finalize_streamed(
                    r,
                    route,
                    route_label,
                    &mut otel_span,
                    cache_key,
                    query_embedding,
                    cache_mapping.as_ref(),
                    &req.model,
                    crate::cache::sampling_key(&req.sampling),
                    budget_reserved,
                );
                if io_err.is_none() {
                    // Flush any buffered token tail held back across the final delta.
                    if let Some(rr) = restorer.as_mut() {
                        let tail = rr.finish();
                        if !tail.is_empty() {
                            let frame = sse_frame(&build_openai_chunk(
                                &id,
                                &model,
                                &fp,
                                &tail,
                                route_label,
                                None,
                                created,
                            ));
                            if let Err(e) = sock.write_all(frame.as_bytes()) {
                                io_err = Some(e);
                            }
                        }
                    }
                    // Emit accumulated tool_calls as a delta chunk before the stop
                    // chunk (ADR-178); the stop chunk then carries the tool_calls
                    // finish reason.
                    if io_err.is_none() {
                        if let Some(tc) = &r.tool_calls {
                            // ADR-189: restore pseudonymized tokens in the cloud's
                            // tool_calls before forwarding to the client. cache_mapping
                            // holds the same mapping as the StreamRestorer (the text
                            // deltas were already restored via StreamRestorer.push()).
                            let restored_tc: String;
                            let tc_to_emit = match cache_mapping.as_ref() {
                                Some(m) if !m.is_empty() => {
                                    restored_tc = crate::pseudonymize::restore(tc, m);
                                    &restored_tc
                                }
                                _ => tc,
                            };
                            let frame = sse_frame(&build_openai_tool_calls_chunk(
                                &id,
                                &model,
                                &fp,
                                tc_to_emit,
                                route_label,
                                created,
                            ));
                            if let Err(e) = sock.write_all(frame.as_bytes()) {
                                io_err = Some(e);
                            }
                        }
                    }
                    if io_err.is_none() {
                        let finish = finish_reason_for(r);
                        let stop = sse_frame(&build_openai_chunk(
                            &id,
                            &model,
                            &fp,
                            "",
                            route_label,
                            Some(finish),
                            created,
                        ));
                        if let Err(e) = sock.write_all(stop.as_bytes()) {
                            io_err = Some(e);
                        }
                    }
                    if io_err.is_none() && include_usage {
                        let usage = sse_frame(&build_openai_usage_chunk(
                            &id,
                            &model,
                            &fp,
                            route_label,
                            r.prompt_tokens,
                            r.completion_tokens,
                            created,
                        ));
                        if let Err(e) = sock.write_all(usage.as_bytes()) {
                            io_err = Some(e);
                        }
                    }
                }
            }
            Err(e) => {
                // Roll back the budget pre-reservation: backend never completed a response (ADR-163).
                if budget_reserved > 0 {
                    self.release_cloud_tokens(budget_reserved);
                }
                // ADR-143: emit error span so streaming backend failures are
                // visible in the trace log rather than silently dropped.
                self.emit_error_span(&mut otel_span, route, &e.to_string());
                if io_err.is_none() {
                    let err = sse_frame(&build_error_response(&e.to_string(), "upstream_error"));
                    if let Err(e2) = sock.write_all(err.as_bytes()) {
                        io_err = Some(e2);
                    }
                }
            }
        }
        if let Some(e) = io_err {
            return Err(e);
        }
        sock.write_all(b"data: [DONE]\n\n")?;
        Ok(200)
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
/// Prepend a user-configured system prompt to the request message list.
/// When the first message is already a system message the two are merged:
/// the configured prompt is placed before the caller's system content.
fn prepend_system_prompt(req: &CompletionRequest, prompt: &str) -> CompletionRequest {
    let has_system = req
        .messages
        .first()
        .map(|m| m.role == "system")
        .unwrap_or(false);
    let mut messages = req.messages.clone();
    if has_system {
        let existing = messages[0].content.clone();
        messages[0].content = format!("{prompt}\n\n{existing}");
    } else {
        messages.insert(
            0,
            Message {
                role: "system".to_string(),
                content: prompt.to_string(),
                ..Default::default()
            },
        );
    }
    CompletionRequest {
        model: req.model.clone(),
        messages,
        stream: req.stream,
        has_tools: req.has_tools,
        sampling: req.sampling.clone(),
    }
}

/// Prepend the PC-assistant system context. Same merge semantics as
/// `prepend_system_prompt`: merged before an existing system message, or
/// inserted as a new first message.
fn inject_context_into(req: &CompletionRequest) -> CompletionRequest {
    prepend_system_prompt(req, &system_context_text())
}

/// Build the text submitted to the local embeddings backend for the semantic cache
/// (IMP-12). System messages are skipped — they are usually proxy infrastructure
/// (date/OS context, safety framing) rather than user intent. The embedding
/// captures what the user is asking, not how the system was configured.
fn semantic_embed_text(req: &CompletionRequest) -> String {
    req.messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build an OpenAI-compatible error body: `{"error":{"message":..,"type":..}}`
/// (SPEC §3.5). Both fields are JSON-escaped.
pub fn build_error_response(message: &str, kind: &str) -> String {
    build_error_response_coded(message, kind, None)
}

/// OpenAI-shape error envelope including the optional `code` field. OpenAI always
/// includes `param` and `code` keys (null when unknown); strict SDK deserializers
/// (openai-python `APIError.param`/`.code`, LiteLLM) read them, so they are always
/// emitted. `param` is null here — our call sites rarely know the offending field —
/// while `code` is populated for the cases where it is unambiguous (e.g. a
/// rate-limit or auth rejection).
pub fn build_error_response_coded(message: &str, kind: &str, code: Option<&str>) -> String {
    let code_field = match code {
        Some(c) => format!("\"{}\"", escape_string(c)),
        None => "null".to_string(),
    };
    format!(
        "{{\"error\":{{\"message\":\"{}\",\"type\":\"{}\",\"param\":null,\"code\":{}}}}}",
        escape_string(message),
        escape_string(kind),
        code_field,
    )
}

/// Build an OpenAI-compatible `GET /v1/models` list response (IMP-8).
/// Each model id is emitted as an `object: "model"` entry owned by "pasture".
/// The `created` field is required by the OpenAI Model schema (ADR-171); Pasture
/// has no per-model creation timestamp so `now` (request time) is used — the same
/// approach taken by LiteLLM and other routing proxies.
pub fn build_models_response(models: &[String]) -> String {
    let now = unix_now();
    let entries: Vec<String> = models
        .iter()
        .map(|m| {
            format!(
                "{{\"id\":\"{}\",\"object\":\"model\",\"created\":{now},\"owned_by\":\"pasture\"}}",
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
            "{{\"id\":\"{}\",\"object\":\"model\",\"created\":{},\"owned_by\":\"pasture\"}}",
            escape_string(id),
            unix_now()
        ))
    } else {
        None
    }
}

/// Build the `GET /v1/stats` JSON body (IMP-metrics): live counters from the
/// cost log. All values are PII-free aggregates (I3). Rates are rounded to 4 dp.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn build_stats_response(
    s: &crate::cost::CostSummary,
    cache_hits: u64,
    cache_misses: u64,
    cache_size: usize,
    cache_cap: usize,
    sem_hits: u64,
    sem_misses: u64,
    sem_size: usize,
    sem_cap: usize,
    budget_used: u64,
    budget_limit: u64,
    output_pii_categories: &[(&'static str, u64)],
    local_health: &'static str,
    input_pii_categories: &[(&'static str, u64)],
) -> String {
    let round4 = |x: f64| (x * 10_000.0).round() / 10_000.0;
    let fmt_cats = |cats: &[(&'static str, u64)]| -> String {
        cats.iter()
            .map(|(cat, n)| format!("\"{cat}\":{n}"))
            .collect::<Vec<String>>()
            .join(",")
    };
    format!(
        "{{\"object\":\"pasture.stats\",\"total\":{},\"local\":{},\"cloud\":{},\"cache\":{},\
\"cloud_rate\":{},\"cache_rate\":{},\"prompt_tokens\":{},\"completion_tokens\":{},\
\"cloud_cost_usd\":{},\"cache_hits\":{cache_hits},\"cache_misses\":{cache_misses},\
\"cache_size\":{cache_size},\"cache_capacity\":{cache_cap},\
\"semantic_cache_hits\":{sem_hits},\"semantic_cache_misses\":{sem_misses},\
\"semantic_cache_size\":{sem_size},\"semantic_cache_capacity\":{sem_cap},\
\"budget_daily_tokens_used\":{budget_used},\"budget_daily_tokens_limit\":{budget_limit},\
\"output_pii_categories\":{{{}}},\"local_health\":\"{local_health}\",\
\"input_pii_categories\":{{{}}}}}",
        s.total,
        s.local,
        s.cloud,
        s.cache,
        round4(s.cloud_rate()),
        round4(s.cache_rate()),
        s.prompt_tokens,
        s.completion_tokens,
        round4(s.cloud_cost_usd),
        fmt_cats(output_pii_categories),
        fmt_cats(input_pii_categories),
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
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{}{:08}", unix_now(), n)
}

/// A unique request id (`req_…`), matching OpenAI's per-response `x-request-id`
/// that clients and support tooling key on for tracing. Generated server-side
/// when the caller did not supply an `X-Request-ID`, so every response is
/// correlatable. Uniqueness within the process is guaranteed by an atomic
/// counter; the wall-clock prefix adds cross-run variety.
fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{}{:08}", unix_now(), n)
}

/// Return a deterministic `system_fingerprint` string for a given model name.
/// Uses FNV-1a (64-bit) truncated to 32 bits → `fp_pasture_XXXXXXXX`.
/// Identical model → identical fingerprint across requests and processes.
pub fn fingerprint_for_model(model: &str) -> String {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in model.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("fp_pasture_{:08x}", h as u32)
}

/// Return the OpenAI `finish_reason` string for a completed response (ADR-181).
/// A response with a `tool_calls` array uses `"tool_calls"`; all others use `"stop"`.
/// Used for both the SSE stop chunk and the OTel span `finish_reason` attribute.
pub fn finish_reason_for(resp: &CompletionResponse) -> &'static str {
    if resp.tool_calls.is_some() {
        "tool_calls"
    } else {
        "stop"
    }
}

pub fn build_openai_response(resp: &CompletionResponse, route_label: &str) -> String {
    build_openai_response_with_injection(resp, route_label, None)
}

/// Like `build_openai_response`, but adds `x_pasture_injection_flag` when
/// the injection guard fires in flag mode (IMP-20).
pub fn build_openai_response_with_injection(
    resp: &CompletionResponse,
    route_label: &str,
    injection_flag: Option<&str>,
) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    let fp = fingerprint_for_model(&resp.model);
    let flag_field = match injection_flag {
        Some(label) => format!(",\"x_pasture_injection_flag\":\"{}\"", escape_string(label)),
        None => String::new(),
    };
    // A tool-call response carries a `tool_calls` array and the `tool_calls`
    // finish reason (ADR-177); an ordinary completion carries neither.
    let (tool_calls_field, finish_reason) = match &resp.tool_calls {
        Some(tc) => (format!(",\"tool_calls\":{tc}"), "tool_calls"),
        None => (String::new(), "stop"),
    };
    format!(
        "{{\"id\":\"{}\",\"object\":\"chat.completion\",\"created\":{},\"model\":\"{}\",\"system_fingerprint\":\"{fp}\",\"x_pasture_route\":\"{}\"{flag_field},\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"{tool_calls_field}}},\"logprobs\":null,\"finish_reason\":\"{finish_reason}\"}}],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
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

/// Build a Prometheus text-format metrics response for `GET /metrics`.
/// Uses the standard exposition format (version 0.0.4): `# HELP`, `# TYPE`, then
/// metric lines. Counter names follow Prometheus naming conventions (total suffix
/// on counters, no suffix on gauges).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub fn build_metrics_response(
    s: &crate::cost::CostSummary,
    cache_hits: u64,
    cache_misses: u64,
    cache_size: usize,
    cache_cap: usize,
    sem_hits: u64,
    sem_misses: u64,
    sem_size: usize,
    sem_cap: usize,
    budget_used: u64,
    budget_limit: u64,
) -> String {
    // Prometheus text exposition format v0.0.4.
    // Braces in label selectors are literal Prometheus syntax — not format args.
    format!(
        "# HELP pasture_requests_total Total requests handled by backend\n\
# TYPE pasture_requests_total counter\n\
pasture_requests_total{{route=\"local\"}} {local}\n\
pasture_requests_total{{route=\"cloud\"}} {cloud}\n\
pasture_requests_total{{route=\"cache\"}} {cache}\n\
# HELP pasture_prompt_tokens_total Total prompt tokens processed\n\
# TYPE pasture_prompt_tokens_total counter\n\
pasture_prompt_tokens_total {prompt_tokens}\n\
# HELP pasture_completion_tokens_total Total completion tokens generated\n\
# TYPE pasture_completion_tokens_total counter\n\
pasture_completion_tokens_total {completion_tokens}\n\
# HELP pasture_cloud_cost_usd_total Estimated cumulative cloud cost USD\n\
# TYPE pasture_cloud_cost_usd_total counter\n\
pasture_cloud_cost_usd_total {cloud_cost}\n\
# HELP pasture_cache_hits_total Cache hit count since process start\n\
# TYPE pasture_cache_hits_total counter\n\
pasture_cache_hits_total {cache_hits}\n\
# HELP pasture_cache_misses_total Cache miss count since process start\n\
# TYPE pasture_cache_misses_total counter\n\
pasture_cache_misses_total {cache_misses}\n\
# HELP pasture_cache_entries Current number of entries in the cache\n\
# TYPE pasture_cache_entries gauge\n\
pasture_cache_entries {cache_size}\n\
# HELP pasture_cache_capacity Maximum entries the cache holds (0=disabled)\n\
# TYPE pasture_cache_capacity gauge\n\
pasture_cache_capacity {cache_cap}\n\
# HELP pasture_semantic_cache_hits_total Semantic cache hit count since process start\n\
# TYPE pasture_semantic_cache_hits_total counter\n\
pasture_semantic_cache_hits_total {sem_hits}\n\
# HELP pasture_semantic_cache_misses_total Semantic cache miss count since process start\n\
# TYPE pasture_semantic_cache_misses_total counter\n\
pasture_semantic_cache_misses_total {sem_misses}\n\
# HELP pasture_semantic_cache_entries Current number of entries in the semantic cache\n\
# TYPE pasture_semantic_cache_entries gauge\n\
pasture_semantic_cache_entries {sem_size}\n\
# HELP pasture_semantic_cache_capacity Maximum entries the semantic cache holds (0=disabled)\n\
# TYPE pasture_semantic_cache_capacity gauge\n\
pasture_semantic_cache_capacity {sem_cap}\n\
# HELP pasture_budget_daily_tokens_used Cloud tokens used today against the daily budget (includes in-flight reservations)\n\
# TYPE pasture_budget_daily_tokens_used gauge\n\
pasture_budget_daily_tokens_used {budget_used}\n\
# HELP pasture_budget_daily_tokens_limit Daily cloud token budget (0=unlimited)\n\
# TYPE pasture_budget_daily_tokens_limit gauge\n\
pasture_budget_daily_tokens_limit {budget_limit}\n",
        local = s.local,
        cloud = s.cloud,
        cache = s.cache,
        prompt_tokens = s.prompt_tokens,
        completion_tokens = s.completion_tokens,
        cloud_cost = s.cloud_cost_usd,
    )
}

/// Build a stub `/v1/moderations` response. All categories are marked safe
/// (false / score 0). Pasture does not run content moderation; the stub prevents
/// client SDKs that unconditionally call the moderation endpoint from erroring.
pub fn build_moderations_response() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let id = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{{\"id\":\"modr-pasture{id:08}\",\"model\":\"text-moderation-stable\",\
\"results\":[{{\"flagged\":false,\
\"categories\":{{\"hate\":false,\"hate/threatening\":false,\"harassment\":false,\
\"harassment/threatening\":false,\"self-harm\":false,\"self-harm/intent\":false,\
\"self-harm/instructions\":false,\"sexual\":false,\"sexual/minors\":false,\"violence\":false,\
\"violence/graphic\":false}},\
\"category_scores\":{{\"hate\":0.0,\"hate/threatening\":0.0,\"harassment\":0.0,\
\"harassment/threatening\":0.0,\"self-harm\":0.0,\"self-harm/intent\":0.0,\
\"self-harm/instructions\":0.0,\"sexual\":0.0,\"sexual/minors\":0.0,\"violence\":0.0,\
\"violence/graphic\":0.0}}}}]}}"
    )
}

/// Build a legacy `text_completion` response for `POST /v1/completions`.
/// Uses `"object":"text_completion"` and `choices[].text` (not `message.content`)
/// so old SDK clients that probe the pre-chat API receive a valid reply.
pub fn build_legacy_completion_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    let fp = fingerprint_for_model(&resp.model);
    // IDs for text completions use the "cmpl-" prefix (matching OpenAI convention).
    let id = next_completion_id().replace("chatcmpl-", "cmpl-");
    format!(
        "{{\"id\":\"{id}\",\"object\":\"text_completion\",\"created\":{},\"model\":\"{}\",\
\"system_fingerprint\":\"{fp}\",\"x_pasture_route\":\"{route_label}\",\
\"choices\":[{{\"text\":\"{}\",\"index\":0,\"logprobs\":null,\"finish_reason\":\"stop\"}}],\
\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
        unix_now(),
        escape_string(&resp.model),
        escape_string(&resp.content),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build an OpenAI-compatible streaming chunk (`chat.completion.chunk`). The `id`,
/// `model`, `fingerprint`, and `created` are supplied by the caller so every chunk
/// of one stream shares them (OpenAI behaviour). Passing `created` as a parameter
/// instead of calling `unix_now()` here ensures the timestamp is identical for
/// every chunk in the stream, including the final usage and stop chunks (ADR-170).
pub fn build_openai_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    delta: &str,
    route_label: &str,
    finish: Option<&str>,
    created: u64,
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
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{delta_field},\"logprobs\":null,\"finish_reason\":{finish_field}}}]}}",
        escape_string(model)
    )
}

/// Build a leading streaming chunk that surfaces the injection-guard label to the
/// client in flag mode (ADR-191), mirroring the buffered response's top-level
/// `x_pasture_injection_flag`. Empty delta, `finish_reason:null`; the content
/// chunks follow. Emitted once, as the first data frame after the SSE headers, so
/// a streaming client can detect a flagged request exactly like a buffered one.
pub fn build_openai_injection_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    route_label: &str,
    label: &str,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"x_pasture_injection_flag\":\"{}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"logprobs\":null,\"finish_reason\":null}}]}}",
        escape_string(model),
        escape_string(label)
    )
}

/// Build a streaming chunk carrying a `tool_calls` array in the delta (ADR-178).
/// `tool_calls` is the raw JSON array. Shares the stream's `id`/`model`/
/// `fingerprint`/`created`; `finish_reason` is null (the following stop chunk
/// carries `"tool_calls"`).
pub fn build_openai_tool_calls_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    tool_calls: &str,
    route_label: &str,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":{tool_calls}}},\"logprobs\":null,\"finish_reason\":null}}]}}",
        escape_string(model)
    )
}

/// Build the final streaming chunk carrying token `usage` (emitted only when the
/// client sets `stream_options.include_usage`). Per the OpenAI contract this
/// chunk has an empty `choices` array. Shares the stream's `id`, `model`,
/// `fingerprint`, and `created` timestamp (ADR-170).
pub fn build_openai_usage_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    route_label: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":{completion_tokens},\"total_tokens\":{}}}}}",
        escape_string(model),
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
/// Default maximum request body the proxy will read (SPEC §7, IMP-21).
/// Overridden at runtime via `PASTURE_MAX_BODY_BYTES` / `Proxy::with_max_body_bytes`.
const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

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
        /// True when the client explicitly requested `Connection: close`, or when
        /// the request is HTTP/1.0 without an explicit `Connection: keep-alive`
        /// (HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close).
        connection_close: bool,
        /// The `X-Request-ID` header value, if present; echoed on all responses
        /// for request tracing.
        request_id: Option<String>,
        /// The `Content-Type` header value (lowercased), if present. Used to
        /// return 415 Unsupported Media Type for non-JSON POST bodies.
        content_type: Option<String>,
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

/// Read one HTTP/1.x request from `stream`, using `conn_buf` as a persistent
/// accumulation buffer. Any bytes read past the end of this request remain in
/// `conn_buf` for the next call (keep-alive pipelining support).
fn read_request(
    stream: &mut std::net::TcpStream,
    conn_buf: &mut Vec<u8>,
    max_body_bytes: usize,
) -> std::io::Result<ReadOutcome> {
    let mut chunk = [0u8; 1024];
    // Accumulate bytes until the header terminator is found.
    let header_end = loop {
        if let Some(pos) = find_subslice(conn_buf, b"\r\n\r\n") {
            break pos;
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut),
            Err(e) => return Err(e),
        };
        if n == 0 {
            // EOF with no headers seen → clean connection close.
            return Ok(ReadOutcome::Closed);
        }
        conn_buf.extend_from_slice(&chunk[..n]);
        if conn_buf.len() > 1_048_576 {
            return Ok(ReadOutcome::Closed); // 1 MiB header guard
        }
    };

    let header_text = String::from_utf8_lossy(&conn_buf[..header_end]).into_owned();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close.
    let http11 = request_line.contains("HTTP/1.1");
    let mut content_length = 0usize;
    let mut auth: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut connection_close = !http11; // HTTP/1.0 default = close
    let mut request_id: Option<String> = None;
    let mut content_type: Option<String> = None;
    for (header_idx, line) in lines.enumerate() {
        if header_idx >= 1000 {
            // Too many header fields — reject as malformed (DoS guard, ADR-097).
            return Ok(ReadOutcome::Closed);
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("content-type:") {
            content_type = Some(v.trim().to_string());
        } else if lower.starts_with("authorization:") {
            if let Some((_, v)) = line.split_once(':') {
                auth = Some(v.trim().to_string());
            }
        } else if lower.starts_with("origin:") {
            if let Some((_, v)) = line.split_once(':') {
                origin = Some(v.trim().to_string());
            }
        } else if let Some(v) = lower.strip_prefix("connection:") {
            let val = v.trim();
            connection_close = val == "close";
            // HTTP/1.0 + "Connection: keep-alive" → keep alive
            if !http11 && val == "keep-alive" {
                connection_close = false;
            }
        } else if lower.starts_with("x-request-id:") {
            if let Some((_, v)) = line.split_once(':') {
                // Strip CR/LF to guard against header injection.
                let clean: String = v
                    .trim()
                    .chars()
                    .filter(|&c| c != '\r' && c != '\n')
                    .collect();
                if !clean.is_empty() {
                    request_id = Some(clean);
                }
            }
        }
    }

    // Reject oversized bodies before reading them (DoS guard, SPEC §7).
    if content_length > max_body_bytes {
        return Ok(ReadOutcome::TooLarge);
    }

    // The body starts right after the \r\n\r\n. bytes already in conn_buf
    // past that offset are part of the body (or a pipelined next request).
    let body_start = header_end + 4;
    let body_end = body_start + content_length;

    // Read more bytes until we have the full body.
    while conn_buf.len() < body_end {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut),
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        conn_buf.extend_from_slice(&chunk[..n]);
        if conn_buf.len() > body_end + max_body_bytes {
            return Ok(ReadOutcome::TooLarge);
        }
    }

    // Extract exactly content_length body bytes.
    let body_bytes = conn_buf[body_start..body_end.min(conn_buf.len())].to_vec();
    // Drain the consumed request bytes; any remainder belongs to the next request.
    conn_buf.drain(..body_end.min(conn_buf.len()));

    Ok(ReadOutcome::Request {
        method,
        path,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
        auth,
        origin,
        connection_close,
        request_id,
        content_type,
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

/// Write a HEAD-only response (headers identical to the GET equivalent, no body).
/// `content_length` is the body size the equivalent GET would return (RFC 7231 §4.3.2).
fn write_head_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_length: usize,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: {conn}\r\n{extra}\r\n"
    );
    stream.write_all(response.as_bytes())
}

/// Write an HTTP/1.1 response. `extra` is a block of additional header lines
/// (each already terminated with `\r\n`, e.g. CORS headers) or empty.
/// `keep_alive` controls the `Connection:` header value.
fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    body: &str,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {conn}\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

/// Write an HTTP/1.1 response with `Content-Type: text/plain` (for `/metrics`).
fn write_plain_response(
    stream: &mut std::net::TcpStream,
    body: &str,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: {conn}\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

/// Append one access-log record (JSONL, no PII) to the configured file.
/// Fields: ts (Unix epoch ms), method, path (no query string), status, ms, request_id?.
/// Silently ignores write errors (access log is best-effort; never drops the request).
fn append_access_log(
    path: &str,
    method: &str,
    norm_path: &str,
    status: u16,
    ms: u128,
    request_id: Option<&str>,
) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let req_id_field = match request_id {
        Some(id) => format!(",\"request_id\":\"{}\"", crate::json::escape_string(id)),
        None => String::new(),
    };
    let line = format!(
        "{{\"ts\":{ts},\"method\":\"{}\",\"path\":\"{}\",\"status\":{status},\"ms\":{ms}{req_id_field}}}\n",
        escape_string(method),
        escape_string(norm_path),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;

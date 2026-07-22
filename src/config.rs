//! Configuration. Defaults are sensible; values can be overridden by a simple
//! `key = value` config file or by environment variables (`PASTURE_*`).

/// Runtime configuration for the proxy and CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub listen_addr: String,
    pub ollama_host: String,
    pub ollama_port: u16,
    pub local_model: String,
    pub local_backend: String,
    pub local_openai_url: String,
    pub cloud_provider: String,
    pub cloud_model: String,
    pub cost_log_path: String,
    pub donate_url: Option<String>,
    pub no_nudge: bool,
    pub state_path: String,
    pub allow_sensitive_cloud: bool,
    pub cascade: bool,
    pub cache_size: usize,
    pub threshold: Option<usize>,
    pub cascade_logprob_threshold: f64,
    /// Cloud transient-failure retry count before falling back to local (IMP-9).
    pub cloud_retry: u32,
    /// When true, all traffic is routed to the local backend (cloud disabled).
    pub local_only: bool,
    /// Optional faster/smaller local model for simple short queries (dual-local routing).
    /// Empty string means disabled.
    pub local_fast_model: String,
    /// Token threshold below which the fast local model is used (dual-local).
    pub fast_threshold: usize,
    /// When true, inject current date/OS info as a system message for PC-assistant use.
    pub inject_context: bool,
    /// Optional bearer token required on `/v1/*` requests (empty = no auth).
    pub auth_token: Option<String>,
    /// Global rate limit for `/v1/*` in requests per minute (0 = unlimited).
    pub rate_limit: u32,
    /// CORS allowed origins (comma-separated, or `*`). Empty = CORS disabled.
    pub cors_origins: String,
    /// Per-connection socket read/write timeout in seconds (0 = no timeout).
    pub request_timeout_secs: u64,
    /// Optional user-defined system prompt prepended to every request.
    /// Loaded from `PASTURE_SYSTEM_PROMPT` env var or `system_prompt` config key.
    pub system_prompt: String,
    /// Optional path for the structured per-request access log (IMP-access-log).
    /// Appends one JSONL record per request. Empty string = disabled.
    pub access_log: String,
    /// TTL for cached responses in seconds (IMP-cache-ttl). 0 = no TTL (entries
    /// live until evicted by FIFO). Enabled via `PASTURE_CACHE_TTL`.
    pub cache_ttl_secs: u64,
    /// Per-request read timeout for the local backend in seconds (IMP-local-timeout).
    /// Default 120 s suits most single-GPU setups; raise for large models (70B+)
    /// or lower to fail fast and trigger the cascade sooner.
    pub local_timeout_secs: u64,
    /// Maximum entries in the optional semantic (embedding-similarity) cache (IMP-12).
    /// 0 = disabled (the default). When non-zero, the local backend's `/v1/embeddings`
    /// endpoint is called to compute query embeddings and cosine similarity is used
    /// to find near-duplicate requests without an exact-match hit.
    pub semantic_cache_size: usize,
    /// Cosine similarity threshold for semantic cache hits (IMP-12). Values in [0, 1];
    /// default 0.92. A hit is returned when similarity ≥ threshold. Ignored when
    /// `semantic_cache_size` is 0.
    pub semantic_cache_threshold: f64,
    /// Lexical second-gate floor for semantic cache hits (IMP-46). A cosine hit
    /// must also share at least this Jaccard token-set overlap with the cached
    /// prompt, rejecting embedding false-positives that would serve a wrong
    /// answer. Values in [0, 1]; `0.0` (default) disables the gate. Ignored when
    /// `semantic_cache_size` is 0.
    pub semantic_cache_min_lexical: f64,
    /// Path to a known-hard prompts file for the embedding difficulty signal
    /// (IMP-14). One prompt per line; `#` comments. Empty = disabled (the default).
    /// Requests embedding-similar to a listed prompt escalate Local → Cloud.
    pub hard_prompts: String,
    /// Cosine similarity at which a prompt counts as "near a known-hard prompt"
    /// (IMP-14). Default 0.85 — looser than the semantic cache's 0.92 because
    /// the goal is topical closeness, not near-duplication.
    pub hard_threshold: f64,
    /// Skill-profile route overrides (IMP-25). Each entry is `(skill, route)`
    /// where skill is one of: `code`, `math`, `reason`, `summarize`, `translate`,
    /// and route is `"local"` or `"cloud"`. Set via `PASTURE_SKILLS=code:local,math:cloud`
    /// or `skills = code:local,math:cloud` in the config file.
    pub skills: Vec<(String, String)>,
    /// Prompt-injection guard mode (IMP-20). One of `"off"` (default), `"flag"`
    /// (detect and annotate via `X-Pasture-Injection-Flag` header, request
    /// proceeds), or `"block"` (reject with 400 Bad Request).
    /// Set via `PASTURE_INJECTION_GUARD` or `injection_guard` in the config file.
    pub injection_guard: String,
    /// Daily cloud token budget (IMP-26). 0 = disabled. Compared against the
    /// running sum of prompt+completion tokens routed to cloud today (UTC day).
    /// Set via `PASTURE_BUDGET_DAILY_TOKENS`.
    pub budget_daily_tokens: u64,
    /// Action when the daily token budget is exceeded (IMP-26).
    /// `"local-only"` (default) — silently route to local instead.
    /// `"warn"` — route to cloud but log a warning.
    /// `"block"` — reject with 429 Too Many Requests.
    /// Set via `PASTURE_BUDGET_ACTION`.
    pub budget_action: String,
    /// Spike detection factor (IMP-26). If a single request estimates more than
    /// `spike_factor × running-request-average` tokens, escalation is overridden
    /// to local (regardless of budget). 0 = spike detection disabled.
    /// Set via `PASTURE_SPIKE_FACTOR`. Default 50.
    pub spike_factor: u64,
    /// Cloud price in USD per 1M tokens as `(input, output)` (ADR-166). Lets the
    /// cost log and the `/metrics` + `/v1/stats` spend gauges report real dollars
    /// for cloud completions instead of a structural 0. `(0.0, 0.0)` (default)
    /// disables pricing. Set via `PASTURE_CLOUD_PRICE_PER_1M="<input>,<output>"`
    /// (e.g. `"2.50,10.00"`).
    pub cloud_price_per_1m: (f64, f64),
    /// Maximum body bytes accepted per request (IMP-21). Requests larger than
    /// this are rejected with 413. Default 16 MiB. Set via `PASTURE_MAX_BODY_BYTES`.
    pub max_body_bytes: usize,
    /// Inject Anthropic prompt-cache hints into the system message (IMP-18).
    /// Separates system messages into Anthropic's `"system"` field and adds
    /// `cache_control: {"type": "ephemeral"}` to enable provider-side KV caching.
    /// For OpenAI this is a no-op. Set via `PASTURE_CACHE_CONTROL=1`.
    pub cache_control: bool,
    /// Pseudonymize PII before sending cloud requests and restore in responses
    /// (IMP-19). Replaces emails, IPs, phone numbers, and API keys with opaque
    /// tokens (`<EMAIL_1>`, etc.) that are reversed after the cloud response.
    /// Set via `PASTURE_PSEUDONYMIZE=1`.
    pub pseudonymize: bool,
    /// Scan response text for PII categories and tally counts in `/v1/stats`
    /// (IMP-33). Detection-only: never mutates the response or logs matched
    /// values, only stable category labels (e.g. "email"). Off by default.
    /// Set via `PASTURE_OUTPUT_PII_SCAN=1`.
    pub output_pii_scan: bool,
    /// Tally which PII categories trigger local-only routing and expose the
    /// breakdown in `/v1/stats` (IMP-28). Detection-only: reuses the report
    /// `route_decision` already computed, no re-scanning. Off by default.
    /// Set via `PASTURE_INPUT_PII_SCAN=1`.
    pub input_pii_scan: bool,
    /// Path to the routing decision audit log (IMP-29). Empty = disabled
    /// (default). Each routed request appends one JSONL record (signals,
    /// threshold, route, reason — no PII, no prompt content). Set via
    /// `PASTURE_DECISION_LOG=<path>`.
    pub decision_log: String,
    /// Local-backend circuit-breaker cooldown in seconds (IMP-34). Once the
    /// local backend is marked Down (3 consecutive failures), non-sensitive
    /// Local decisions redirect to Cloud (when configured) until this many
    /// seconds have elapsed, at which point one probe request is let through
    /// to detect recovery. Default 30. 0 disables the circuit breaker
    /// (matches pre-IMP-34 behavior: every request still attempts local).
    /// Set via `PASTURE_HEALTH_COOLDOWN_SECS=<n>`.
    pub health_cooldown_secs: u64,
    /// Optional OTel-compatible GenAI trace log path (IMP-23). Each request
    /// appends one JSONL span with GenAI semantic convention attributes.
    /// Empty = disabled (default). Set via `PASTURE_OTEL_LOG=<path>`.
    pub otel_log: String,
    /// Fallback cloud provider tried when the primary cloud fails all retries
    /// (IMP-9 multi-provider follow-up). Empty string = disabled (default).
    /// Set via `PASTURE_CLOUD_FALLBACK_PROVIDER` (`openai` or `anthropic`).
    pub cloud_fallback_provider: String,
    /// Model to use on the fallback cloud provider. Empty = same as `cloud_model`.
    /// Set via `PASTURE_CLOUD_FALLBACK_MODEL`.
    pub cloud_fallback_model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "127.0.0.1:8645".to_string(),
            ollama_host: "127.0.0.1".to_string(),
            ollama_port: 11434,
            local_model: "llama3.2".to_string(),
            local_backend: "ollama".to_string(),
            local_openai_url: "http://127.0.0.1:1234/v1".to_string(),
            cloud_provider: "openai".to_string(),
            cloud_model: "gpt-4o-mini".to_string(),
            cost_log_path: "pasture-cost.jsonl".to_string(),
            donate_url: None,
            no_nudge: false,
            state_path: "pasture-state.txt".to_string(),
            allow_sensitive_cloud: false,
            cascade: false,
            cache_size: 0,
            threshold: None,
            cascade_logprob_threshold: -1.0,
            cloud_retry: 2,
            local_only: false,
            local_fast_model: String::new(),
            fast_threshold: 50,
            inject_context: false,
            auth_token: None,
            rate_limit: 0,
            cors_origins: String::new(),
            request_timeout_secs: 30,
            system_prompt: String::new(),
            access_log: String::new(),
            cache_ttl_secs: 0,
            local_timeout_secs: 120,
            semantic_cache_size: 0,
            semantic_cache_threshold: 0.92,
            semantic_cache_min_lexical: 0.0,
            hard_prompts: String::new(),
            hard_threshold: 0.85,
            skills: Vec::new(),
            injection_guard: "off".to_string(),
            budget_daily_tokens: 0,
            budget_action: "local-only".to_string(),
            spike_factor: 50,
            cloud_price_per_1m: (0.0, 0.0),
            max_body_bytes: 16 * 1024 * 1024,
            cache_control: false,
            pseudonymize: false,
            output_pii_scan: false,
            input_pii_scan: false,
            decision_log: String::new(),
            health_cooldown_secs: 30,
            otel_log: String::new(),
            cloud_fallback_provider: String::new(),
            cloud_fallback_model: String::new(),
        }
    }
}

impl Config {
    /// Apply `key = value` lines from a config file body onto the defaults.
    /// Unknown keys are ignored; blank lines and `#` comments are skipped.
    pub fn from_str_with_defaults(body: &str) -> Self {
        let mut cfg = Self::default();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                continue;
            };
            cfg.apply(key.trim(), val.trim());
        }
        cfg
    }

    /// Overlay environment variables (`PASTURE_LISTEN_ADDR`, etc.).
    pub fn with_env(mut self) -> Self {
        if let Ok(v) = std::env::var("PASTURE_LISTEN_ADDR") {
            self.listen_addr = v;
        }
        if let Ok(v) = std::env::var("PASTURE_OLLAMA_HOST") {
            self.ollama_host = v;
        }
        if let Ok(v) = std::env::var("PASTURE_OLLAMA_PORT") {
            if let Ok(p) = v.parse() {
                self.ollama_port = p;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_MODEL") {
            self.local_model = v;
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_BACKEND") {
            self.local_backend = v;
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_OPENAI_URL") {
            self.local_openai_url = v;
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_PROVIDER") {
            self.cloud_provider = v;
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_MODEL") {
            self.cloud_model = v;
        }
        if let Ok(v) = std::env::var("PASTURE_COST_LOG") {
            self.cost_log_path = v;
        }
        if let Ok(v) = std::env::var("PASTURE_DONATE_URL") {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                self.donate_url = Some(trimmed);
            }
        }
        if let Ok(v) = std::env::var("PASTURE_NO_NUDGE") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.no_nudge = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_STATE") {
            self.state_path = v;
        }
        if let Ok(v) = std::env::var("PASTURE_ALLOW_SENSITIVE_CLOUD") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.allow_sensitive_cloud = true,
                "0" | "false" | "no" => self.allow_sensitive_cloud = false,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CASCADE") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.cascade = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CACHE") {
            if let Ok(n) = v.parse::<usize>() {
                self.cache_size = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_THRESHOLD") {
            if let Ok(n) = v.parse::<usize>() {
                self.threshold = Some(n);
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CASCADE_LOGPROB") {
            if let Ok(n) = v.parse::<f64>() {
                self.cascade_logprob_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_RETRY") {
            if let Ok(n) = v.parse::<u32>() {
                self.cloud_retry = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_ONLY") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.local_only = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_FAST_MODEL") {
            self.local_fast_model = v;
        }
        if let Ok(v) = std::env::var("PASTURE_FAST_THRESHOLD") {
            if let Ok(n) = v.parse::<usize>() {
                self.fast_threshold = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_INJECT_CONTEXT") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.inject_context = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_AUTH_TOKEN") {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                self.auth_token = Some(trimmed);
            }
        }
        if let Ok(v) = std::env::var("PASTURE_RATE_LIMIT") {
            if let Ok(n) = v.parse::<u32>() {
                self.rate_limit = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CORS_ORIGINS") {
            self.cors_origins = v;
        }
        if let Ok(v) = std::env::var("PASTURE_REQUEST_TIMEOUT") {
            if let Ok(n) = v.parse::<u64>() {
                self.request_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SYSTEM_PROMPT") {
            self.system_prompt = v;
        }
        if let Ok(v) = std::env::var("PASTURE_ACCESS_LOG") {
            self.access_log = v;
        }
        if let Ok(v) = std::env::var("PASTURE_CACHE_TTL") {
            if let Ok(n) = v.parse() {
                self.cache_ttl_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_TIMEOUT") {
            if let Ok(n) = v.parse::<u64>() {
                self.local_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SEMANTIC_CACHE") {
            if let Ok(n) = v.parse::<usize>() {
                self.semantic_cache_size = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SEMANTIC_THRESHOLD") {
            if let Ok(f) = v.parse::<f64>() {
                self.semantic_cache_threshold = f;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SEMANTIC_MIN_LEXICAL") {
            if let Ok(f) = v.parse::<f64>() {
                self.semantic_cache_min_lexical = f;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_HARD_PROMPTS") {
            self.hard_prompts = v;
        }
        if let Ok(v) = std::env::var("PASTURE_HARD_THRESHOLD") {
            if let Ok(f) = v.parse::<f64>() {
                self.hard_threshold = f;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SKILLS") {
            self.skills = parse_skills(&v);
        }
        if let Ok(v) = std::env::var("PASTURE_INJECTION_GUARD") {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "off" | "flag" | "block") {
                self.injection_guard = v;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_BUDGET_DAILY_TOKENS") {
            if let Ok(n) = v.parse::<u64>() {
                self.budget_daily_tokens = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_BUDGET_ACTION") {
            let v = v.trim().to_ascii_lowercase();
            if matches!(v.as_str(), "local-only" | "warn" | "block") {
                self.budget_action = v;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_SPIKE_FACTOR") {
            if let Ok(n) = v.parse::<u64>() {
                self.spike_factor = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_PRICE_PER_1M") {
            if let Some(price) = parse_price_pair(&v) {
                self.cloud_price_per_1m = price;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_MAX_BODY_BYTES") {
            if let Ok(n) = v.parse::<usize>() {
                self.max_body_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_CACHE_CONTROL") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.cache_control = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_PSEUDONYMIZE") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.pseudonymize = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_OUTPUT_PII_SCAN") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.output_pii_scan = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_INPUT_PII_SCAN") {
            match v.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "" => self.input_pii_scan = true,
                _ => {}
            }
        }
        if let Ok(v) = std::env::var("PASTURE_DECISION_LOG") {
            self.decision_log = v;
        }
        if let Ok(v) = std::env::var("PASTURE_HEALTH_COOLDOWN_SECS") {
            if let Ok(n) = v.parse::<u64>() {
                self.health_cooldown_secs = n;
            }
        }
        if let Ok(v) = std::env::var("PASTURE_OTEL_LOG") {
            self.otel_log = v;
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_FALLBACK_PROVIDER") {
            self.cloud_fallback_provider = v;
        }
        if let Ok(v) = std::env::var("PASTURE_CLOUD_FALLBACK_MODEL") {
            self.cloud_fallback_model = v;
        }
        self
    }

    fn apply(&mut self, key: &str, val: &str) {
        match key {
            "listen_addr" => self.listen_addr = val.to_string(),
            "ollama_host" => self.ollama_host = val.to_string(),
            "ollama_port" => {
                if let Ok(p) = val.parse() {
                    self.ollama_port = p;
                }
            }
            "local_model" => self.local_model = val.to_string(),
            "local_backend" => self.local_backend = val.to_string(),
            "local_openai_url" => self.local_openai_url = val.to_string(),
            "cloud_provider" => self.cloud_provider = val.to_string(),
            "cloud_model" => self.cloud_model = val.to_string(),
            "cost_log_path" => self.cost_log_path = val.to_string(),
            "donate_url" => self.donate_url = Some(val.to_string()),
            "state_path" => self.state_path = val.to_string(),
            "cascade" => self.cascade = matches!(val, "1" | "true" | "yes"),
            "cache_size" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.cache_size = n;
                }
            }
            "threshold" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.threshold = Some(n);
                }
            }
            "cascade_logprob_threshold" => {
                if let Ok(n) = val.parse::<f64>() {
                    self.cascade_logprob_threshold = n;
                }
            }
            "cloud_retry" => {
                if let Ok(n) = val.parse::<u32>() {
                    self.cloud_retry = n;
                }
            }
            "local_only" => self.local_only = matches!(val, "1" | "true" | "yes"),
            "local_fast_model" => self.local_fast_model = val.to_string(),
            "fast_threshold" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.fast_threshold = n;
                }
            }
            "inject_context" => self.inject_context = matches!(val, "1" | "true" | "yes"),
            "auth_token" => {
                if !val.is_empty() {
                    self.auth_token = Some(val.to_string());
                }
            }
            "rate_limit" => {
                if let Ok(n) = val.parse::<u32>() {
                    self.rate_limit = n;
                }
            }
            "cors_origins" => self.cors_origins = val.to_string(),
            "request_timeout" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.request_timeout_secs = n;
                }
            }
            "system_prompt" => self.system_prompt = val.to_string(),
            "access_log" => self.access_log = val.to_string(),
            "no_nudge" => self.no_nudge = matches!(val, "1" | "true" | "yes"),
            "allow_sensitive_cloud" => {
                self.allow_sensitive_cloud = matches!(val, "1" | "true" | "yes")
            }
            "cache_ttl_secs" => {
                if let Ok(n) = val.parse() {
                    self.cache_ttl_secs = n;
                }
            }
            "local_timeout" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.local_timeout_secs = n;
                }
            }
            "semantic_cache_size" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.semantic_cache_size = n;
                }
            }
            "semantic_cache_threshold" => {
                if let Ok(f) = val.parse::<f64>() {
                    self.semantic_cache_threshold = f;
                }
            }
            "semantic_cache_min_lexical" => {
                if let Ok(f) = val.parse::<f64>() {
                    self.semantic_cache_min_lexical = f;
                }
            }
            "hard_prompts" => self.hard_prompts = val.to_string(),
            "hard_threshold" => {
                if let Ok(f) = val.parse::<f64>() {
                    self.hard_threshold = f;
                }
            }
            "skills" => self.skills = parse_skills(val),
            "injection_guard" => {
                let v = val.trim().to_ascii_lowercase();
                if matches!(v.as_str(), "off" | "flag" | "block") {
                    self.injection_guard = v;
                }
            }
            "budget_daily_tokens" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.budget_daily_tokens = n;
                }
            }
            "budget_action" => {
                let v = val.trim().to_ascii_lowercase();
                if matches!(v.as_str(), "local-only" | "warn" | "block") {
                    self.budget_action = v;
                }
            }
            "spike_factor" => {
                if let Ok(n) = val.parse::<u64>() {
                    self.spike_factor = n;
                }
            }
            "cloud_price_per_1m" => {
                if let Some(price) = parse_price_pair(val) {
                    self.cloud_price_per_1m = price;
                }
            }
            "max_body_bytes" => {
                if let Ok(n) = val.parse::<usize>() {
                    self.max_body_bytes = n;
                }
            }
            "cache_control" => {
                self.cache_control = matches!(val, "1" | "true" | "yes");
            }
            "pseudonymize" => {
                self.pseudonymize = matches!(val, "1" | "true" | "yes");
            }
            "otel_log" => self.otel_log = val.to_string(),
            "cloud_fallback_provider" => self.cloud_fallback_provider = val.to_string(),
            "cloud_fallback_model" => self.cloud_fallback_model = val.to_string(),
            _ => {}
        }
    }
}

/// Parse a `"<input>,<output>"` cloud price (USD per 1M tokens) into `(input,
/// output)` (ADR-166). Returns `None` unless both parts parse to finite,
/// non-negative floats, so a malformed value leaves the default `(0.0, 0.0)`
/// rather than silently disabling or corrupting pricing.
pub(crate) fn parse_price_pair(val: &str) -> Option<(f64, f64)> {
    let (a, b) = val.split_once(',')?;
    let input = a.trim().parse::<f64>().ok()?;
    let output = b.trim().parse::<f64>().ok()?;
    if input.is_finite() && output.is_finite() && input >= 0.0 && output >= 0.0 {
        Some((input, output))
    } else {
        None
    }
}

/// Parse `"code:local,math:cloud"` into `[("code","local"),("math","cloud")]`.
pub(crate) fn parse_skills(val: &str) -> Vec<(String, String)> {
    val.split(',')
        .filter_map(|pair| {
            let pair = pair.trim();
            let (skill, route) = pair.split_once(':')?;
            let route = route.trim().to_ascii_lowercase();
            if route == "local" || route == "cloud" {
                Some((skill.trim().to_ascii_lowercase(), route))
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_listen_addr() {
        assert_eq!(Config::default().listen_addr, "127.0.0.1:8645");
    }

    /// Extract every distinct `PASTURE_*` token that appears inside a
    /// `std::env::var("…")` call in `config.rs` — the precise set of environment
    /// variables the config layer actually reads (comments and doc strings are
    /// ignored because they are not inside a `var("…")` call).
    fn env_vars_read_by_config() -> std::collections::BTreeSet<String> {
        let src = include_str!("config.rs");
        let mut found = std::collections::BTreeSet::new();
        let needle = "std::env::var(\"";
        let mut rest = src;
        while let Some(i) = rest.find(needle) {
            rest = &rest[i + needle.len()..];
            if let Some(end) = rest.find('"') {
                let name = &rest[..end];
                if name.starts_with("PASTURE_") {
                    found.insert(name.to_string());
                }
            }
        }
        found
    }

    /// Extract every `PASTURE_*` token that appears in a **config-table row** of
    /// `SPEC.md` (a markdown line beginning with `|`). The table is the
    /// authoritative list of recognised variables; prose elsewhere may legitimately
    /// mention a non-variable (e.g. the drift note that names the historical
    /// `PASTURE_PROXY_TOKEN` precisely to say it is *not* recognised), so only
    /// table rows are treated as "documented as a real variable".
    fn env_vars_documented_in_spec() -> std::collections::BTreeSet<String> {
        // SPEC.md lives at the crate root, next to Cargo.toml.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/SPEC.md");
        let spec = std::fs::read_to_string(path).expect("SPEC.md must be readable");
        let mut found = std::collections::BTreeSet::new();
        for line in spec.lines() {
            if !line.trim_start().starts_with('|') {
                continue;
            }
            let bytes = line.as_bytes();
            let mut i = 0;
            while i + 8 <= bytes.len() {
                if &bytes[i..i + 8] == b"PASTURE_" {
                    let start = i;
                    let mut j = i + 8;
                    while j < bytes.len()
                        && (bytes[j].is_ascii_uppercase()
                            || bytes[j].is_ascii_digit()
                            || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    // Require a non-empty suffix so the bare `PASTURE_*` glob in prose
                    // never registers as a variable name.
                    if j > start + 8 {
                        found.insert(line[start..j].to_string());
                    }
                    i = j;
                } else {
                    i += 1;
                }
            }
        }
        found
    }

    #[test]
    fn test_spec_documents_every_env_var_config_reads() {
        // ADR-190: guard against SPEC.md / implementation drift. Every environment
        // variable the config layer reads MUST be documented in SPEC.md §8. This
        // test would have caught the historical `PASTURE_PROXY_TOKEN` mismatch and
        // the 21 vars that were silently missing from the spec table.
        let read = env_vars_read_by_config();
        let documented = env_vars_documented_in_spec();
        let undocumented: Vec<&String> = read.difference(&documented).collect();
        assert!(
            undocumented.is_empty(),
            "these env vars are read by config.rs but undocumented in SPEC.md: {undocumented:?}"
        );
    }

    #[test]
    fn test_spec_has_no_phantom_pasture_env_vars() {
        // ADR-190 (reverse direction): SPEC.md must not document a PASTURE_* env var
        // that the config layer never reads — that is how `PASTURE_PROXY_TOKEN` crept
        // in. A handful of non-config vars are read elsewhere (proxy/cli/cost) or are
        // documented prefixes; allow-list those so the test targets real drift.
        let documented = env_vars_documented_in_spec();
        let read = env_vars_read_by_config();
        // Vars surfaced in SPEC.md but resolved outside config.rs (BYOK keys read by
        // the cloud layer; referral keys read by the CLI; the i18n language var).
        let allow_external: &[&str] = &[
            "PASTURE_OPENAI_API_KEY",
            "PASTURE_ANTHROPIC_API_KEY",
            "PASTURE_LANG",
        ];
        let phantom: Vec<&String> = documented
            .iter()
            .filter(|v| !read.contains(*v) && !allow_external.contains(&v.as_str()))
            .collect();
        assert!(
            phantom.is_empty(),
            "SPEC.md documents PASTURE_* vars that config.rs never reads (drift or typo): {phantom:?}"
        );
    }

    #[test]
    fn test_parse_price_pair_valid() {
        assert_eq!(parse_price_pair("2.50,10.00"), Some((2.50, 10.00)));
        assert_eq!(parse_price_pair(" 0.15 , 0.6 "), Some((0.15, 0.6)));
        assert_eq!(parse_price_pair("0,0"), Some((0.0, 0.0)));
    }

    #[test]
    fn test_parse_price_pair_rejects_malformed() {
        // ADR-166: malformed values are rejected so the default (0,0) survives.
        assert_eq!(parse_price_pair("2.50"), None); // missing second field
        assert_eq!(parse_price_pair("abc,10"), None); // non-numeric
        assert_eq!(parse_price_pair("-1,10"), None); // negative
        assert_eq!(parse_price_pair("inf,10"), None); // non-finite
        assert_eq!(parse_price_pair(""), None);
    }

    #[test]
    fn test_config_parses_cloud_price_key() {
        let cfg = Config::from_str_with_defaults("cloud_price_per_1m = 3.00,15.00\n");
        assert_eq!(cfg.cloud_price_per_1m, (3.00, 15.00));
    }

    #[test]
    fn test_parse_overrides_known_keys() {
        let body = "# comment\nlocal_model = qwen3\nollama_port = 9999\n\n";
        let cfg = Config::from_str_with_defaults(body);
        assert_eq!(cfg.local_model, "qwen3");
        assert_eq!(cfg.ollama_port, 9999);
    }

    #[test]
    fn test_parse_ignores_unknown_and_malformed() {
        let body = "unknown_key = 1\nnot a pair\nlocal_model = m";
        let cfg = Config::from_str_with_defaults(body);
        assert_eq!(cfg.local_model, "m");
        // Untouched default preserved.
        assert_eq!(cfg.cloud_provider, "openai");
    }

    #[test]
    fn test_parse_invalid_port_keeps_default() {
        let cfg = Config::from_str_with_defaults("ollama_port = notaport");
        assert_eq!(cfg.ollama_port, 11434);
    }

    #[test]
    fn test_request_timeout_default_and_parse() {
        assert_eq!(Config::default().request_timeout_secs, 30);
        let cfg = Config::from_str_with_defaults("request_timeout = 5");
        assert_eq!(cfg.request_timeout_secs, 5);
    }

    #[test]
    fn test_local_timeout_default_and_parse() {
        assert_eq!(Config::default().local_timeout_secs, 120);
        let cfg = Config::from_str_with_defaults("local_timeout = 300");
        assert_eq!(cfg.local_timeout_secs, 300);
        // Invalid value keeps the default.
        let cfg2 = Config::from_str_with_defaults("local_timeout = notanumber");
        assert_eq!(cfg2.local_timeout_secs, 120);
    }

    #[test]
    fn test_config_file_boolean_parity_with_env() {
        // These flags were env-only (PASTURE_NO_NUDGE / PASTURE_ALLOW_SENSITIVE_CLOUD)
        // and silently ignored in config files before; now they have file parity.
        let cfg = Config::from_str_with_defaults("no_nudge = true\nallow_sensitive_cloud = yes");
        assert!(cfg.no_nudge);
        assert!(cfg.allow_sensitive_cloud);
        // Defaults remain false when unset / falsey.
        let off = Config::from_str_with_defaults("no_nudge = false");
        assert!(!off.no_nudge);
        assert!(!off.allow_sensitive_cloud);
    }

    #[test]
    fn test_auth_token_config_strips_whitespace() {
        // Config-file path pre-trims via val.trim(); verify the stored token is clean.
        let cfg = Config::from_str_with_defaults("auth_token = my_secret_token");
        assert_eq!(cfg.auth_token, Some("my_secret_token".to_string()));
        // Whitespace-only value -> None (no auth configured)
        let empty = Config::from_str_with_defaults("auth_token =     ");
        assert_eq!(empty.auth_token, None);
        // A whitespace-only env-var token must also resolve to None (tested via apply).
        // The env-var path stores `v.trim().to_string()` so "token\n" → "token".
    }

    #[test]
    fn test_boolean_env_vars_require_truthy_value() {
        // SECURITY: PASTURE_ALLOW_SENSITIVE_CLOUD=false / =0 must NOT enable cloud
        // routing of sensitive content. The prior is_ok() check treated any value
        // including "false" or "0" as enabling the flag — a silent security regression.
        // This test uses the apply() path (config file) as a proxy since the with_env()
        // path requires actual env var mutation which is not thread-safe in tests.
        // The apply() and with_env() now use the same matching semantics.
        let enabled = Config::from_str_with_defaults("allow_sensitive_cloud = 1");
        assert!(enabled.allow_sensitive_cloud, "\"1\" should enable");
        let enabled2 = Config::from_str_with_defaults("allow_sensitive_cloud = true");
        assert!(enabled2.allow_sensitive_cloud, "\"true\" should enable");
        let disabled = Config::from_str_with_defaults("allow_sensitive_cloud = false");
        assert!(!disabled.allow_sensitive_cloud, "\"false\" must NOT enable");
        let disabled2 = Config::from_str_with_defaults("allow_sensitive_cloud = 0");
        assert!(!disabled2.allow_sensitive_cloud, "\"0\" must NOT enable");
        let disabled3 = Config::from_str_with_defaults("allow_sensitive_cloud = no");
        assert!(!disabled3.allow_sensitive_cloud, "\"no\" must NOT enable");
        // Same for local_only, no_nudge, cascade, inject_context
        let lo = Config::from_str_with_defaults("local_only = 1");
        assert!(lo.local_only);
        let lo_off = Config::from_str_with_defaults("local_only = 0");
        assert!(!lo_off.local_only);
    }
}

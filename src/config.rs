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
            if !v.trim().is_empty() {
                self.donate_url = Some(v);
            }
        }
        if std::env::var("PASTURE_NO_NUDGE").is_ok() {
            self.no_nudge = true;
        }
        if let Ok(v) = std::env::var("PASTURE_STATE") {
            self.state_path = v;
        }
        if std::env::var("PASTURE_ALLOW_SENSITIVE_CLOUD").is_ok() {
            self.allow_sensitive_cloud = true;
        }
        if std::env::var("PASTURE_CASCADE").is_ok() {
            self.cascade = true;
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
        if std::env::var("PASTURE_LOCAL_ONLY").is_ok() {
            self.local_only = true;
        }
        if let Ok(v) = std::env::var("PASTURE_LOCAL_FAST_MODEL") {
            self.local_fast_model = v;
        }
        if let Ok(v) = std::env::var("PASTURE_FAST_THRESHOLD") {
            if let Ok(n) = v.parse::<usize>() {
                self.fast_threshold = n;
            }
        }
        if std::env::var("PASTURE_INJECT_CONTEXT").is_ok() {
            self.inject_context = true;
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_listen_addr() {
        assert_eq!(Config::default().listen_addr, "127.0.0.1:8645");
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
}

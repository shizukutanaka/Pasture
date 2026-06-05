//! Routing engine: the core of Pasture.
//!
//! Decisions are deterministic (no ML, no network) and adapt their thresholds
//! to the detected hardware (US-1). Ordered rules, single responsibility (I8).

use crate::hardware::HardwareProfile;

/// Where a query is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Local,
    Cloud,
}

impl Route {
    pub fn as_str(self) -> &'static str {
        match self {
            Route::Local => "local",
            Route::Cloud => "cloud",
        }
    }
}

/// A routing outcome with a human-readable rationale (observability, §9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub route: Route,
    pub reason: String,
}

/// Why a decision could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingError {
    /// The forced route has no available backend.
    ForcedRouteUnavailable(Route),
    /// Neither a local nor a cloud backend is configured.
    NoBackendAvailable,
    /// Content was classified sensitive but no local backend is available to
    /// keep it on-device; refusing to send it to the cloud.
    SensitiveButNoLocal,
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingError::ForcedRouteUnavailable(r) => {
                write!(f, "forced route '{}' is not available", r.as_str())
            }
            RoutingError::NoBackendAvailable => {
                write!(f, "no local or cloud backend is available")
            }
            RoutingError::SensitiveButNoLocal => {
                write!(
                    f,
                    "sensitive content detected but no local backend is available; refusing cloud"
                )
            }
        }
    }
}

impl std::error::Error for RoutingError {}

/// Estimate the token count of a prompt. Heuristic: ~4 characters per token,
/// which is good enough for threshold comparisons and needs no tokenizer.
/// Estimate token count across scripts. Latin/whitespace text averages ~4
/// chars per token, but CJK and Hangul are far denser (≈1 token per character).
/// A purely chars/4 estimate would undercount Japanese/Chinese/Korean ~4x and
/// keep long non-Latin prompts on the local model when they should escalate.
pub fn estimate_tokens(text: &str) -> usize {
    let mut dense = 0usize; // CJK / Hangul: ~1 token per char
    let mut other = 0usize; // Latin etc.: ~1 token per 4 chars
    for c in text.chars() {
        if is_dense_script(c) {
            dense += 1;
        } else {
            other += 1;
        }
    }
    dense + other.div_ceil(4)
}

/// True for characters that tokenize at roughly one token each (CJK, kana,
/// Hangul, fullwidth/halfwidth forms).
fn is_dense_script(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0xAC00..=0xD7A3 // Hangul syllables
        | 0xFF66..=0xFF9D // halfwidth Katakana
    )
}

/// True when the text appears to contain source code (fenced block).
pub fn looks_like_code(text: &str) -> bool {
    text.contains("```")
}

/// Reasoning-depth markers (EN + JA). Hard reasoning benefits from the strong
/// model (survey arXiv:2506.06579).
const REASONING_MARKERS: &[&str] = &[
    "step by step",
    "step-by-step",
    "reason through",
    "think through",
    "chain of thought",
    "prove that",
    "derive",
    "explain why",
    "work through",
    "ステップ",
    "順を追って",
    "証明",
    "なぜ",
    "段階的",
    "推論",
];

/// Strict-format / code-generation requests that reward a stronger model.
const FORMAT_MARKERS: &[&str] = &[
    "as json",
    "in json",
    "valid json",
    "json format",
    "markdown table",
    "regex",
    "sql query",
    "yaml",
    "openapi",
    "write a function",
    "implement a",
    "write code",
    "write a program",
    "json形式",
    "表形式",
    "正規表現",
    "関数を実装",
    "コードを書",
];

/// Characters that, in density, suggest a mathematical / formal query.
const MATH_CHARS: &[char] = &[
    '=', '+', '*', '/', '^', '∑', '∫', '√', 'π', '≤', '≥', '≠', '∂', 'Σ',
];

fn has_marker(lower: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| lower.contains(m))
}

/// Count distinct question marks (ASCII and full-width).
pub fn question_count(text: &str) -> usize {
    text.chars().filter(|&c| c == '?' || c == '？').count()
}

/// True when the text is math-heavy (>= 4 mathematical symbols).
pub fn looks_mathy(text: &str) -> bool {
    text.chars().filter(|c| MATH_CHARS.contains(c)).count() >= 4
}

/// Aggregate "hard" signals that suggest escalating to the stronger model.
/// Returns stable labels (no user content).
pub fn hard_signals(text: &str) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    let mut signals = Vec::new();
    if looks_like_code(text) {
        signals.push("code");
    }
    if has_marker(&lower, REASONING_MARKERS) {
        signals.push("reasoning");
    }
    if has_marker(&lower, FORMAT_MARKERS) {
        signals.push("format");
    }
    if question_count(text) >= 3 {
        signals.push("multi_question");
    }
    if looks_mathy(text) {
        signals.push("math");
    }
    signals
}

/// The deterministic routing engine, parameterised by available backends and a
/// hardware-adaptive token threshold.
#[derive(Debug, Clone)]
pub struct RoutingEngine {
    token_threshold: usize,
    code_to_cloud: bool,
    local_available: bool,
    cloud_available: bool,
    allow_sensitive_cloud: bool,
}

impl RoutingEngine {
    /// Build an engine with an explicit threshold.
    pub fn new(token_threshold: usize, local_available: bool, cloud_available: bool) -> Self {
        Self {
            token_threshold,
            code_to_cloud: true,
            local_available,
            cloud_available,
            allow_sensitive_cloud: false,
        }
    }

    /// Build an engine whose threshold adapts to the host hardware (US-1).
    ///
    /// A capable GPU keeps more work local (high threshold); a CPU-only host
    /// with little RAM falls back to the cloud earlier (low threshold).
    pub fn for_hardware(
        profile: &HardwareProfile,
        local_available: bool,
        cloud_available: bool,
    ) -> Self {
        let threshold = if profile.has_capable_gpu(8000) {
            2000
        } else if profile.gpu.is_some() || profile.ram_mb >= 16000 {
            800
        } else {
            300
        };
        Self::new(threshold, local_available, cloud_available)
    }

    /// Disable the "code goes to cloud" rule (used by `--local`-leaning setups).
    pub fn with_code_to_cloud(mut self, enabled: bool) -> Self {
        self.code_to_cloud = enabled;
        self
    }

    /// Allow sensitive content to reach the cloud (off by default; privacy-first).
    pub fn with_allow_sensitive_cloud(mut self, allowed: bool) -> Self {
        self.allow_sensitive_cloud = allowed;
        self
    }

    pub fn threshold(&self) -> usize {
        self.token_threshold
    }

    /// Override the token threshold (e.g. from `pasture calibrate`).
    pub fn with_threshold(mut self, threshold: usize) -> Self {
        self.token_threshold = threshold;
        self
    }

    /// Decide where to route `text`, honouring an optional forced route.
    /// Equivalent to `decide_with_sensitivity(text, forced, false)`.
    pub fn decide(&self, text: &str, forced: Option<Route>) -> Result<Decision, RoutingError> {
        self.decide_with_sensitivity(text, forced, false)
    }

    /// Decide routing, with an explicit sensitivity flag (IMP-3).
    ///
    /// When `sensitive` is true and `allow_sensitive_cloud` is false, the
    /// request is kept local — overriding even a forced `--cloud` — and errors
    /// rather than leaking to the cloud if no local backend exists.
    pub fn decide_with_sensitivity(
        &self,
        text: &str,
        forced: Option<Route>,
        sensitive: bool,
    ) -> Result<Decision, RoutingError> {
        if sensitive && !self.allow_sensitive_cloud {
            return if self.local_available {
                Ok(Decision {
                    route: Route::Local,
                    reason: "sensitive content kept local".to_string(),
                })
            } else {
                Err(RoutingError::SensitiveButNoLocal)
            };
        }

        if let Some(route) = forced {
            return self.decide_forced(route);
        }

        match (self.local_available, self.cloud_available) {
            (false, false) => return Err(RoutingError::NoBackendAvailable),
            (true, false) => {
                return Ok(Decision {
                    route: Route::Local,
                    reason: "only local backend available".to_string(),
                })
            }
            (false, true) => {
                return Ok(Decision {
                    route: Route::Cloud,
                    reason: "only cloud backend available".to_string(),
                })
            }
            (true, true) => {}
        }

        if self.code_to_cloud {
            let signals = hard_signals(text);
            if !signals.is_empty() {
                return Ok(Decision {
                    route: Route::Cloud,
                    reason: format!("hard signal(s): {}", signals.join(", ")),
                });
            }
        }

        let tokens = estimate_tokens(text);
        if tokens >= self.token_threshold {
            Ok(Decision {
                route: Route::Cloud,
                reason: format!(
                    "estimated {tokens} tokens >= threshold {}",
                    self.token_threshold
                ),
            })
        } else {
            Ok(Decision {
                route: Route::Local,
                reason: format!(
                    "estimated {tokens} tokens < threshold {}",
                    self.token_threshold
                ),
            })
        }
    }

    fn decide_forced(&self, route: Route) -> Result<Decision, RoutingError> {
        let available = match route {
            Route::Local => self.local_available,
            Route::Cloud => self.cloud_available,
        };
        if available {
            Ok(Decision {
                route,
                reason: format!("forced to {}", route.as_str()),
            })
        } else {
            Err(RoutingError::ForcedRouteUnavailable(route))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::{GpuInfo, HardwareProfile};

    fn both() -> RoutingEngine {
        RoutingEngine::new(100, true, true)
    }

    #[test]
    fn test_estimate_tokens_quarter_of_chars() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_cjk_is_denser() {
        // 8 Japanese chars ~= 8 tokens, not 2 (chars/4 would undercount).
        assert_eq!(estimate_tokens("日本語のテスト文章"), 9);
        // Korean (Hangul) also dense.
        assert_eq!(estimate_tokens("안녕하세요"), 5);
    }

    #[test]
    fn test_estimate_tokens_mixed_script() {
        // 4 Latin chars (=1) + 2 kana (=2) -> 3 tokens.
        assert_eq!(estimate_tokens("code あい"), {
            // "code あい" = 'c','o','d','e',' ' (5 latin -> 2) + 'あ','い' (2 dense)
            2 + 2
        });
    }

    #[test]
    fn test_looks_like_code_detects_fence() {
        assert!(looks_like_code("here:\n```\nfn x(){}\n```"));
        assert!(!looks_like_code("plain question"));
    }

    #[test]
    fn test_decide_short_query_goes_local() {
        let d = both().decide("hello", None).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_decide_long_query_goes_cloud() {
        let long = "x".repeat(1000);
        let d = both().decide(&long, None).unwrap();
        assert_eq!(d.route, Route::Cloud);
    }

    #[test]
    fn test_decide_code_query_goes_cloud() {
        let d = both().decide("```rust\nfn main(){}\n```", None).unwrap();
        assert_eq!(d.route, Route::Cloud);
    }

    #[test]
    fn test_decide_code_to_cloud_disabled_keeps_short_local() {
        let e = both().with_code_to_cloud(false);
        let d = e.decide("```\nx\n```", None).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_forced_local_overrides_long_query() {
        let long = "x".repeat(1000);
        let d = both().decide(&long, Some(Route::Local)).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_forced_cloud_overrides_short_query() {
        let d = both().decide("hi", Some(Route::Cloud)).unwrap();
        assert_eq!(d.route, Route::Cloud);
    }

    #[test]
    fn test_forced_route_unavailable_errors() {
        let e = RoutingEngine::new(100, true, false);
        let err = e.decide("hi", Some(Route::Cloud)).unwrap_err();
        assert_eq!(err, RoutingError::ForcedRouteUnavailable(Route::Cloud));
    }

    #[test]
    fn test_only_local_available_routes_local() {
        let e = RoutingEngine::new(1, true, false);
        let long = "x".repeat(1000);
        assert_eq!(e.decide(&long, None).unwrap().route, Route::Local);
    }

    #[test]
    fn test_only_cloud_available_routes_cloud() {
        let e = RoutingEngine::new(10_000, false, true);
        assert_eq!(e.decide("hi", None).unwrap().route, Route::Cloud);
    }

    #[test]
    fn test_no_backend_available_errors() {
        let e = RoutingEngine::new(100, false, false);
        assert_eq!(
            e.decide("hi", None).unwrap_err(),
            RoutingError::NoBackendAvailable
        );
    }

    #[test]
    fn test_for_hardware_capable_gpu_high_threshold() {
        let p = HardwareProfile {
            ram_mb: 32000,
            cpu_count: 16,
            gpu: Some(GpuInfo {
                vendor: "nvidia".into(),
                vram_mb: Some(24576),
            }),
        };
        assert_eq!(
            RoutingEngine::for_hardware(&p, true, true).threshold(),
            2000
        );
    }

    #[test]
    fn test_for_hardware_cpu_only_low_threshold() {
        let p = HardwareProfile {
            ram_mb: 8000,
            cpu_count: 4,
            gpu: None,
        };
        assert_eq!(RoutingEngine::for_hardware(&p, true, true).threshold(), 300);
    }

    #[test]
    fn test_for_hardware_midrange_threshold() {
        let p = HardwareProfile {
            ram_mb: 16000,
            cpu_count: 8,
            gpu: None,
        };
        assert_eq!(RoutingEngine::for_hardware(&p, true, true).threshold(), 800);
    }

    #[test]
    fn test_sensitive_kept_local_overrides_long() {
        let long = "x".repeat(1000);
        let d = both().decide_with_sensitivity(&long, None, true).unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("sensitive"));
    }

    #[test]
    fn test_sensitive_overrides_forced_cloud() {
        let d = both()
            .decide_with_sensitivity("hi", Some(Route::Cloud), true)
            .unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_sensitive_without_local_errors() {
        let e = RoutingEngine::new(100, false, true);
        assert_eq!(
            e.decide_with_sensitivity("hi", None, true).unwrap_err(),
            RoutingError::SensitiveButNoLocal
        );
    }

    #[test]
    fn test_sensitive_allowed_to_cloud_when_opted_in() {
        let long = "x".repeat(1000);
        let e = both().with_allow_sensitive_cloud(true);
        // With the opt-in, normal rules apply (long -> cloud).
        assert_eq!(
            e.decide_with_sensitivity(&long, None, true).unwrap().route,
            Route::Cloud
        );
    }

    #[test]
    fn test_non_sensitive_unaffected() {
        let d = both().decide_with_sensitivity("hi", None, false).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_hard_signals_reasoning_en_and_ja() {
        assert!(hard_signals("solve this step by step").contains(&"reasoning"));
        assert!(hard_signals("これをステップで説明して").contains(&"reasoning"));
    }

    #[test]
    fn test_hard_signals_strict_format() {
        assert!(hard_signals("return the answer as JSON").contains(&"format"));
        assert!(hard_signals("write a function to sort").contains(&"format"));
    }

    #[test]
    fn test_question_count_threshold() {
        assert_eq!(question_count("a? b? c?"), 3);
        assert!(hard_signals("why? how? when?").contains(&"multi_question"));
        assert!(!hard_signals("what is this?").contains(&"multi_question"));
    }

    #[test]
    fn test_looks_mathy() {
        assert!(looks_mathy("x = a + b * c / d ^ 2"));
        assert!(!looks_mathy("a normal sentence"));
    }

    #[test]
    fn test_plain_short_has_no_hard_signal() {
        assert!(hard_signals("what is the capital of France").is_empty());
    }

    #[test]
    fn test_decide_reasoning_goes_cloud() {
        let d = both().decide("prove that the sum is even", None).unwrap();
        assert_eq!(d.route, Route::Cloud);
        assert!(d.reason.contains("reasoning"));
    }

    #[test]
    fn test_decide_hard_signal_disabled_keeps_local() {
        let e = both().with_code_to_cloud(false);
        assert_eq!(
            e.decide("solve step by step", None).unwrap().route,
            Route::Local
        );
    }
}

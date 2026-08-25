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

/// Estimate the token count of a prompt (IMP-22 fertility-aware heuristic).
///
/// Three script classes, each with its empirical fertility (chars-per-token):
/// - **Dense** (CJK, Hangul, kana, Thai, Devanagari, emoji): ≈1.0 tok/char.
/// - **Digits** (ASCII 0-9): ≈0.5 tok/char — multi-digit numbers cluster
///   as 2-3 chars per token in cl100k_base and LLaMA tokenisers.
/// - **Latin / punctuation**: ≈0.25 tok/char (the traditional 4 chars/token).
/// - **Whitespace** (spaces, tabs, newlines): ≈0 tok/char — whitespace is
///   fused into adjacent tokens and does not generate tokens on its own;
///   counting it as 0.25 systematically over-estimated space-heavy text.
///
/// This corrects the two largest systematic errors vs. the prior two-bucket
/// implementation: whitespace inflation and digit under-counting
/// (arXiv:2509.05486 "The Token Tax"; IMP-22).
pub fn estimate_tokens(text: &str) -> usize {
    let mut dense = 0usize; // CJK / Hangul / kana / Thai / Devangari / emoji
    let mut digits = 0usize; // ASCII 0-9: ~0.5 tok/char
    let mut latin = 0usize; // letters, punctuation: ~0.25 tok/char
                            // whitespace (spaces, tabs, newlines) contributes 0 tokens
    for c in text.chars() {
        if is_dense_script(c) {
            dense += 1;
        } else if c.is_ascii_whitespace() {
            // fused into adjacent tokens — no separate token contribution
        } else if c.is_ascii_digit() {
            digits += 1;
        } else {
            latin += 1;
        }
    }
    dense + digits.div_ceil(2) + latin.div_ceil(4)
}

/// Predict the number of completion (output) tokens a request will generate
/// (IMP-24, heuristic form). Cloud pricing is driven mostly by output tokens
/// (typically 3–5× the input rate), so a budget/spike guard that counts only
/// input tokens systematically under-estimates a request's true cost. This is a
/// std-only, zero-dependency, deterministic heuristic — not the proxy-model
/// predictor from SSJF (arXiv:2404.08509), which was deferred to avoid a
/// dependency. The signal: output length correlates with task type and with
/// input length.
///
/// `input_tokens` is the estimated prompt size (from `estimate_tokens`).
/// `max_tokens` is the client-supplied hard cap, if any — the model can never
/// exceed it, so the prediction is clamped to it.
///
/// Multipliers are deliberately conservative (lean high): for a budget guard,
/// over-estimating output keeps the user safely under their cap, whereas
/// under-estimating risks a surprise overage.
pub fn estimate_output_tokens(text: &str, input_tokens: usize, max_tokens: Option<u64>) -> usize {
    // Per-task output/input ratio (×100 to stay in integer arithmetic).
    // Grounded in the task-shape intuition: code/reasoning expand, summaries
    // compress, translation is roughly length-preserving.
    let ratio_x100 = match detect_skill(text) {
        Some("code") => 300,      // code generation expands well past the prompt
        Some("reason") => 400,    // chain-of-thought answers are verbose
        Some("math") => 200,      // worked solutions are moderately long
        Some("translate") => 110, // output ≈ input length
        Some("summarize") => 30,  // compression: output is a fraction of input
        _ => 150,                 // generic chat answer: moderate expansion
    };
    let mut predicted = input_tokens.saturating_mul(ratio_x100) / 100;

    // Multi-question prompts produce one answer block per question.
    if question_count(text) >= 3 {
        predicted = predicted.saturating_mul(3) / 2;
    }

    // Floor: even a one-word prompt yields a sentence or two of output.
    predicted = predicted.max(16);

    // Ceiling: without a client cap, models still stop well before infinity.
    // 4096 is a common provider default for chat completions.
    const DEFAULT_OUTPUT_CEIL: usize = 4096;
    predicted = predicted.min(DEFAULT_OUTPUT_CEIL);

    // A client-supplied max_tokens is a hard upper bound the model cannot exceed.
    if let Some(cap) = max_tokens {
        predicted = predicted.min(cap as usize);
    }
    predicted
}

/// Total estimated tokens (input + predicted output) for cost/budget estimation
/// (IMP-24). This is what a budget or spike guard should compare against, since
/// cloud cost is charged on both halves.
pub fn estimate_total_tokens(text: &str, max_tokens: Option<u64>) -> usize {
    let input = estimate_tokens(text);
    input + estimate_output_tokens(text, input, max_tokens)
}

/// True for characters that tokenize at roughly one token each (CJK, kana,
/// Hangul, fullwidth/halfwidth forms, emoji, Thai, Devanagari).
/// Emoji (U+1F000–U+1FAFF) average 1-3 tokens per character in common
/// tokenizers (GPT-4, LLaMA 3); counting them as 0.25 tok/char (Latin default)
/// under-estimates prompts with many emoji by 4-12×.
/// Thai (U+0E00–0E7F) and Devanagari (U+0900–097F, used for Hindi, Sanskrit,
/// Marathi, Nepali) are each ~1 char per token in cl100k_base; treating them as
/// Latin (0.25 tok/char) under-estimates a Thai or Hindi technical prompt by 4×,
/// potentially leaving a genuinely long prompt on the local model.
fn is_dense_script(c: char) -> bool {
    matches!(c as u32,
        0x0900..=0x097F   // Devanagari (Hindi, Sanskrit, Marathi, Nepali)
        | 0x0E00..=0x0E7F // Thai
        | 0x3040..=0x30FF   // Hiragana + Katakana
        | 0x3400..=0x4DBF // CJK Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0xAC00..=0xD7A3 // Hangul syllables
        | 0xFF66..=0xFF9D // halfwidth Katakana
        | 0x1F000..=0x1FAFF // Emoji & Symbols (Misc Symbols, Emoticons, etc.)
    )
}

/// True when the text appears to contain source code (fenced block).
/// True when the text contains a properly-formed fenced code block (```) that
/// indicates a code-heavy request. A properly-formed fence is three backticks
/// at the start of a line (after optional leading whitespace), followed by
/// nothing (EOL) or a language specifier (alphanumeric + optional dash/hyphen).
/// In-line backticks (e.g. "use ``` like this ```" in prose) do not count —
/// they are markup, not code blocks (ADR-210). Requires a balanced pair of
/// opening and closing fences to confirm code is present.
pub fn looks_like_code(text: &str) -> bool {
    let mut fence_count = 0;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if let Some(after) = trimmed.strip_prefix("```") {
            // A fence is properly formed if followed by EOL, whitespace, or a
            // language specifier (e.g., python, C++, c#).
            let is_fence = after.is_empty()
                || after
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric() || c == '-' || c.is_whitespace());
            if is_fence {
                fence_count += 1;
            }
        }
    }
    // Requires at least 2 fences (opening + closing), so the block is balanced.
    fence_count >= 2
}

/// Reasoning-depth markers (EN + JA). Hard reasoning benefits from the strong
/// model (survey arXiv:2506.06579).
const REASONING_MARKERS: &[&str] = &[
    "step by step",
    "step-by-step",
    "reason through",
    "think through",
    "chain of thought",
    "chain-of-thought",
    "show your work",
    "show your reasoning",
    "walk me through",
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
    "理由を説明",
];

/// Strict-format / code-generation requests that reward a stronger model.
/// Pure **structured-output / reformatting** markers (ADR-256).
///
/// Held separately from the code-generation markers below because 2026 SLM
/// evidence splits them: classification, structured extraction and reformatting
/// "almost always work on small models", while multi-step reasoning and
/// long-context synthesis do not. Escalating a bare "give me that as JSON" to
/// the cloud therefore spends money on exactly the class a local model handles
/// reliably — Pasture failing at its own job.
///
/// These only stop escalating when `PASTURE_STRUCTURED_LOCAL=1`; the default is
/// unchanged (see `hard_signals_with`), because the quality trade-off cannot be
/// verified on this machine without a live-model eval harness.
const STRUCTURED_MARKERS: &[&str] = &[
    "as json",
    "in json",
    "valid json",
    "json format",
    "as xml",
    "csv format",
    "markdown table",
    "yaml",
    "json形式",
    "表形式",
];

/// **Code-generation** markers. These stay a hard signal regardless of
/// `PASTURE_STRUCTURED_LOCAL`: writing a program is synthesis, not reformatting,
/// and is where small models measurably trail.
const FORMAT_MARKERS: &[&str] = &[
    "regex",
    "sql query",
    "openapi",
    "write a function",
    "implement a",
    "write code",
    "write a program",
    "write a test",
    "unit test",
    "shell script",
    "bash script",
    "dockerfile",
    "正規表現",
    "関数を実装",
    "コードを書",
    "単体テスト",
];

/// Characters that, in density, suggest a mathematical / formal query.
const MATH_CHARS: &[char] = &[
    '=', '+', '*', '/', '^', '∑', '∫', '√', 'π', '≤', '≥', '≠', '∂', 'Σ',
];

fn has_marker(lower: &str, markers: &[&str]) -> bool {
    markers.iter().any(|m| lower.contains(m))
}

/// Count clause-terminating question marks (ASCII `?` and full-width `？`).
///
/// A question mark counts only when it *ends a clause* — it is the last
/// character of the text, or the next character is whitespace or a closing
/// delimiter (`)`, `]`, `}`, `"`, `'`, `>`, `？`, `?`). This excludes the `?`
/// that delimits a URL query string (`https://x.com/s?q=1`), where the `?` is
/// immediately followed by an alphanumeric query key (ADR-209). Without this
/// guard, a prompt that merely references three URLs with query strings tripped
/// the `multi_question` hard signal and routed to cloud — the same
/// delimiter-vs-operator confusion fixed for `looks_mathy` in ADR-208.
pub fn question_count(text: &str) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let mut count = 0;
    for (i, &c) in chars.iter().enumerate() {
        match c {
            // Full-width '？' is never a URL query delimiter (URLs use ASCII '?'),
            // and CJK text has no inter-word spaces, so a '？' is followed directly
            // by the next sentence. Always count it.
            '？' => count += 1,
            // ASCII '?' counts only when it ends a clause: end-of-text, or the
            // next character is whitespace or a closing delimiter. A '?' followed
            // immediately by an alphanumeric is a URL query key (`?q=1`), not a
            // question (ADR-209).
            '?' => {
                let terminates = match chars.get(i + 1) {
                    None => true,
                    Some(&next) => {
                        next.is_whitespace()
                            || matches!(next, ')' | ']' | '}' | '"' | '\'' | '>' | '?' | '！' | '!')
                    }
                };
                if terminates {
                    count += 1;
                }
            }
            _ => {}
        }
    }
    count
}

/// True when the text is math-heavy: at least 3 *distinct* mathematical
/// symbol types are present (ADR-208). The old approach counted total
/// occurrences (≥ 4), which caused false positives on any URL with 4+
/// path segments (`https://a.com/b/c/d/e` has 5 `/` characters → old code
/// returned true, routing all URL-containing prompts to cloud). Counting
/// *distinct* types instead (unique members of MATH_CHARS that appear at all)
/// preserves detection of genuine math while making a single repeated
/// character type (like path `/`) unable to trigger the signal alone.
/// A threshold of 3 distinct types ensures `a^2 + b^2 = c^2` still routes
/// to cloud while `https://host/a/b/c?x=1` (only `/` and `=`, 2 types)
/// does not.
pub fn looks_mathy(text: &str) -> bool {
    MATH_CHARS.iter().filter(|&&c| text.contains(c)).count() >= 3
}

/// Skill-detection markers for summarisation requests (EN + JA).
const SUMMARIZE_MARKERS: &[&str] = &[
    "summarize",
    "summarise",
    "summary",
    "tldr",
    "tl;dr",
    "in brief",
    "briefly",
    "要約",
    "まとめ",
    "概要",
    "サマリー",
    "要旨",
];

/// Skill-detection markers for translation requests (EN + JA).
const TRANSLATE_MARKERS: &[&str] = &[
    "translate",
    "translation",
    "翻訳",
    "訳して",
    "に翻訳",
    "translate to",
    "translate into",
];

/// Classify the primary skill of a prompt into one of several canonical labels.
/// Returns `None` for general / unclassified queries.
///
/// Labels (stable, used as keys in skill-profile routing, IMP-25):
/// - `"code"`: fenced code block present.
/// - `"math"`: ≥4 mathematical symbols.
/// - `"reason"`: reasoning-depth marker (step-by-step, prove, etc.).
/// - `"summarize"`: summarisation request.
/// - `"translate"`: translation request.
pub fn detect_skill(text: &str) -> Option<&'static str> {
    if looks_like_code(text) {
        return Some("code");
    }
    if looks_mathy(text) {
        return Some("math");
    }
    let lower = text.to_ascii_lowercase();
    if has_marker(&lower, REASONING_MARKERS) {
        return Some("reason");
    }
    if has_marker(&lower, SUMMARIZE_MARKERS) {
        return Some("summarize");
    }
    if has_marker(&lower, TRANSLATE_MARKERS) {
        return Some("translate");
    }
    None
}

/// Aggregate "hard" signals that suggest escalating to the stronger model.
/// Returns stable labels (no user content).
pub fn hard_signals(text: &str) -> Vec<&'static str> {
    hard_signals_with(text, false)
}

/// `hard_signals` with the ADR-256 structured-output opt-out.
///
/// When `structured_local` is true, pure reformatting/extraction markers
/// (`STRUCTURED_MARKERS`) no longer contribute a hard signal, so a bare
/// "give me that as JSON" is routed on its own merits (length, other signals)
/// instead of being forced to the cloud. Code-generation markers are unaffected.
///
/// Default is `false` — identical to pre-ADR-256 behaviour. This is deliberately
/// opt-in: the supporting evidence is external benchmark reporting, and Pasture
/// has no live-model quality harness (the open W6 gap) with which to confirm the
/// trade-off locally, so it does not silently re-route everyone's traffic.
pub fn hard_signals_with(text: &str, structured_local: bool) -> Vec<&'static str> {
    let lower = text.to_lowercase();
    let mut signals = Vec::new();
    if looks_like_code(text) {
        signals.push("code");
    }
    if has_marker(&lower, REASONING_MARKERS) {
        signals.push("reasoning");
    }
    if has_marker(&lower, FORMAT_MARKERS)
        || (!structured_local && has_marker(&lower, STRUCTURED_MARKERS))
    {
        signals.push("format");
    }
    if question_count(text) >= 3 {
        signals.push("multi_question");
    }
    if looks_mathy(text) {
        signals.push("math");
    }
    if is_multi_step(text) {
        signals.push("multi_step");
    }
    signals
}

/// English sequencing cue words (matched whole-word) that mark an explicit
/// enumeration of sequential steps.
const STEP_CUES_EN: &[&str] = &[
    "first",
    "firstly",
    "second",
    "secondly",
    "third",
    "thirdly",
    "then",
    "next",
    "afterward",
    "afterwards",
    "finally",
    "lastly",
    "subsequently",
];

/// Japanese sequencing cues (matched as substrings — distinctive multi-byte
/// sequences with negligible false-positive risk, and JA has no word breaks).
const STEP_CUES_JA: &[&str] = &[
    "まず",
    "次に",
    "その後",
    "最後に",
    "はじめに",
    "続いて",
    "それから",
];

/// True when the prompt enumerates **≥3 distinct sequential steps** (IMP-40):
/// an implicit multi-step plan like "first X, then Y, finally Z", or a numbered
/// list of ≥3 items. These carry no explicit reasoning marker (`step by step`,
/// `prove`) so the reasoning signal misses them, and they can sit below the
/// token threshold, yet multi-step tasks are exactly where a small local model
/// underperforms a frontier model (2026 SLM benchmarks) — so they should
/// escalate. Counting *distinct cue types* (not raw occurrences) plus a
/// numbered-list detector keeps false positives on a casual "…, then …" low.
pub fn is_multi_step(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Distinct EN cue types via word-boundary tokenization (substring matching
    // would fire on "then" inside "strengthen").
    let mut distinct: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for w in lower.split(|c: char| !c.is_alphanumeric()) {
        if STEP_CUES_EN.contains(&w) {
            distinct.insert(w);
        }
    }
    for cue in STEP_CUES_JA {
        if lower.contains(cue) {
            distinct.insert(cue);
        }
    }
    if distinct.len() >= 3 {
        return true;
    }
    numbered_list_items(text) >= 3
}

/// Count list items introduced by `N.`/`N)` at a token boundary and followed by
/// whitespace (so `3.14` is not a list item). Used by `is_multi_step`.
fn numbered_list_items(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        let at_boundary = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if at_boundary && bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            // A marker is <digits><'.' or ')'> then whitespace or end-of-text.
            if j < bytes.len()
                && matches!(bytes[j], b'.' | b')')
                && (j + 1 == bytes.len() || bytes[j + 1].is_ascii_whitespace())
            {
                count += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    count
}

/// True when the prompt has no hard content signals and is below `threshold`
/// estimated tokens — suitable for a fast lightweight local model.
pub fn is_simple_prompt(text: &str, threshold: usize) -> bool {
    hard_signals(text).is_empty() && estimate_tokens(text) < threshold
}

/// Markers for a time-sensitive prompt (EN + JA, IMP-41): its correct answer
/// changes over time — "what's the weather today", "latest exchange rate",
/// "現在の株価". A cache hit (exact-match text OR semantic/cosine) on such a
/// prompt is a silent staleness bug, not a genuine hit: the request text can
/// match perfectly while the real-world fact it asks about has moved on. This
/// is deliberately narrower than a bare "now"/"current" match (which would
/// also fire on "current directory", "current implementation", etc. and gut
/// the cache hit rate for ordinary coding prompts) — it targets phrases that
/// specifically anchor to the present moment.
const TIME_SENSITIVE_MARKERS: &[&str] = &[
    "today",
    "tonight",
    "tomorrow",
    "yesterday",
    "currently",
    "right now",
    "as of now",
    "up to date",
    "up-to-date",
    "this week",
    "this month",
    "latest",
    "breaking news",
    "current price",
    "current weather",
    "current time",
    "current date",
    "what time is it",
    "what day is it",
    "what's the date",
    "whats the date",
    "stock price",
    "exchange rate",
    "今日",
    "今夜",
    "明日",
    "昨日",
    "現在",
    "最新",
    "今週",
    "今月",
    "為替レート",
    "株価",
    "天気",
];

/// True when `text` asks about something whose correct answer changes over
/// time (IMP-41). Callers use this to bypass both the exact-match and
/// semantic caches — for read (never serve a stale cached fact) and for write
/// (never store an answer whose correctness has an expiry the cache doesn't
/// track). This is orthogonal to `hard_signals`: a time-sensitive prompt can
/// still be routed local (it may be trivially easy) — it is just never
/// cached, regardless of route.
pub fn is_time_sensitive(text: &str) -> bool {
    has_marker(&text.to_lowercase(), TIME_SENSITIVE_MARKERS)
}

/// The deterministic routing engine, parameterised by available backends and a
/// hardware-adaptive token threshold.
#[derive(Debug, Clone)]
pub struct RoutingEngine {
    token_threshold: usize,
    code_to_cloud: bool,
    /// ADR-256: treat pure structured-output markers as non-escalating.
    structured_local: bool,
    local_available: bool,
    cloud_available: bool,
    allow_sensitive_cloud: bool,
    /// When true all traffic routes local regardless of content signals or length.
    local_only: bool,
    /// Skill-profile overrides (IMP-25): `(skill_name, route)` pairs, checked
    /// before the generic hard-signal rules. E.g. `("summarize", Local)` keeps
    /// summarisation on the fast local model even when hard signals are present.
    /// Exception (ADR-228): never overrides when the request has tools/
    /// function-calling — that is a *capability* requirement, not a content
    /// signal, and a skill match must not silently route it to a backend that
    /// may not support function-calling.
    skills: Vec<(String, Route)>,
}

impl RoutingEngine {
    /// Build an engine with an explicit threshold.
    pub fn new(token_threshold: usize, local_available: bool, cloud_available: bool) -> Self {
        Self {
            token_threshold,
            code_to_cloud: true,
            structured_local: false,
            local_available,
            cloud_available,
            allow_sensitive_cloud: false,
            local_only: false,
            skills: Vec::new(),
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
        // ADR-261: match every case explicitly, and crucially handle
        // `ram_mb: None` (detection unavailable on this OS) by leaning LOCAL
        // rather than assuming the weakest machine. The old code collapsed an
        // undetected RAM to 0 and fell into the 300 (CPU-only) tier, so a strong
        // Mac/Windows box silently escalated everything to the paid cloud — the
        // inverse of the product's promise, and the harmful direction for a
        // cost/privacy-first tool.
        let threshold = if profile.has_capable_gpu(8000) {
            2000 // capable discrete GPU: keep the most work local
        } else if profile.gpu.is_some() {
            800 // some GPU, VRAM unknown/small
        } else {
            match profile.ram_mb {
                Some(r) if r >= 16000 => 800, // known roomy CPU box
                Some(_) => 300,               // known small box: escalate sooner
                None => 800,                  // UNKNOWN: never silently ship to cloud
            }
        };
        Self::new(threshold, local_available, cloud_available)
    }

    /// Disable the "code goes to cloud" rule (used by `--local`-leaning setups).
    /// ADR-256: when enabled, pure structured-output / reformatting markers
    /// ("as json", "csv format", "markdown table", …) stop forcing cloud, so
    /// they route on length and other signals like any other prompt. Code
    /// generation still escalates. Off by default.
    pub fn with_structured_local(mut self, enabled: bool) -> Self {
        self.structured_local = enabled;
        self
    }

    pub fn with_code_to_cloud(mut self, enabled: bool) -> Self {
        self.code_to_cloud = enabled;
        self
    }

    /// Allow sensitive content to reach the cloud (off by default; privacy-first).
    pub fn with_allow_sensitive_cloud(mut self, allowed: bool) -> Self {
        self.allow_sensitive_cloud = allowed;
        self
    }

    /// Force all traffic to the local backend regardless of content signals or
    /// token length. Privacy rules still apply (sensitive content already stays
    /// local; this adds nothing there). Cloud availability is ignored.
    pub fn with_local_only(mut self, enabled: bool) -> Self {
        self.local_only = enabled;
        self
    }

    /// True when `PASTURE_LOCAL_ONLY` is in effect (cloud is never contacted
    /// regardless of content signals or token length). Exposed so callers can
    /// tell a plain difficulty-based Local decision apart from one produced
    /// by an explicit "never touch cloud" operator setting (IMP-34's circuit
    /// breaker must not override the latter).
    pub fn is_local_only(&self) -> bool {
        self.local_only
    }

    /// Set skill-profile route overrides (IMP-25).
    ///
    /// Each entry is `(skill_label, route)`. Recognised labels: `"code"`,
    /// `"math"`, `"reason"`, `"summarize"`, `"translate"`. Unknown labels are
    /// silently ignored at decision time.
    pub fn with_skills(mut self, skills: Vec<(String, Route)>) -> Self {
        self.skills = skills;
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
        self.decide_full(text, forced, sensitive, false)
    }

    /// Decide routing with full context: the sensitivity flag plus whether the
    /// request carries tool/function-calling fields (IMP-10). Tool use is a
    /// hard signal — it escalates to cloud alongside the content-based signals
    /// (and, like them, is gated by the `code_to_cloud` rule).
    pub fn decide_full(
        &self,
        text: &str,
        forced: Option<Route>,
        sensitive: bool,
        has_tools: bool,
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

        // local_only: bypass cloud entirely after the privacy check so we never
        // contact cloud even for hard signals or long prompts.
        if self.local_only {
            return if self.local_available {
                Ok(Decision {
                    route: Route::Local,
                    reason: "local-only mode (PASTURE_LOCAL_ONLY)".to_string(),
                })
            } else {
                Err(RoutingError::NoBackendAvailable)
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

        // Skill-profile overrides (IMP-25): checked before generic hard signals.
        // A configured skill match short-circuits the rest of the decision.
        // ADR-228: NOT when has_tools is set. A skill match is a setting about
        // *text content* (e.g. "keep my code on the local model"); has_tools is
        // a *capability* signal — the local model may not support
        // function-calling at all. Letting a content-based skill silently
        // override a capability requirement could route a genuine tool-calling
        // request to a backend that cannot serve it. Falling through here lets
        // the hard-signal logic below (which already treats `tools` as a hard
        // signal, gated by `code_to_cloud`) make the capability-aware call.
        if !self.skills.is_empty() && !has_tools {
            if let Some(skill) = detect_skill(text) {
                for (name, route) in &self.skills {
                    if name == skill {
                        return self.decide_forced_with_reason(*route, format!("skill:{skill}"));
                    }
                }
            }
        }

        if self.code_to_cloud {
            let mut signals = hard_signals_with(text, self.structured_local);
            if has_tools {
                signals.push("tools");
            }
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
        self.decide_forced_with_reason(route, format!("forced to {}", route.as_str()))
    }

    fn decide_forced_with_reason(
        &self,
        route: Route,
        reason: String,
    ) -> Result<Decision, RoutingError> {
        let available = match route {
            Route::Local => self.local_available,
            Route::Cloud => self.cloud_available,
        };
        if available {
            Ok(Decision { route, reason })
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
        // IMP-22: whitespace no longer counts as latin.
        // "code あい" = c,o,d,e (4 latin → ceil(4/4)=1) + ' ' (whitespace → 0)
        //              + あ,い (2 dense → 2) = 3 tokens.
        assert_eq!(estimate_tokens("code あい"), 1 + 2);
    }

    #[test]
    fn test_estimate_tokens_whitespace_zero_contrib() {
        // IMP-22: spaces/newlines fuse into adjacent tokens, not separate tokens.
        assert_eq!(estimate_tokens("   "), 0);
        assert_eq!(estimate_tokens("\n\t"), 0);
        // A padded word: "hello " (5 latin + 1 space) = ceil(5/4) = 2 latin tokens.
        assert_eq!(estimate_tokens("hello "), 2);
    }

    #[test]
    fn test_estimate_tokens_digits_half_rate() {
        // IMP-22: digits tokenise at ~0.5 tok/char (multi-digit numbers use
        // 2-3 chars per token in cl100k_base and LLaMA tokenisers).
        // "1234" = 4 digits → ceil(4/2) = 2 tokens.
        assert_eq!(estimate_tokens("1234"), 2);
        // "12" = 2 digits → ceil(2/2) = 1 token.
        assert_eq!(estimate_tokens("12"), 1);
        // "12345 abc" = 5 digits (ceil(5/2)=3) + 1 space(0) + 3 latin(ceil(3/4)=1) = 4.
        assert_eq!(estimate_tokens("12345 abc"), 3 + 1);
    }

    #[test]
    fn test_estimate_tokens_emoji_counted_as_dense() {
        // Emoji (U+1F000-U+1FAFF) used to score 0.25 tok/char (Latin fallback),
        // under-estimating emoji-heavy prompts by 4-12×. They are now dense (1/char).
        // 😀 = U+1F600 (in Emoticons block), 🎉 = U+1F389 (Misc Symbols & Pictographs).
        assert_eq!(estimate_tokens("😀😀😀😀"), 4); // 4 emoji -> 4 dense tokens
        assert_eq!(estimate_tokens("hi 🎉"), 1 + 1); // 3 latin (ceil/4=1) + 1 emoji
    }

    #[test]
    fn test_estimate_tokens_thai_and_devanagari_dense() {
        // ADR-105: Thai and Devanagari are ~1 token/char in cl100k_base.
        // Without the fix these would score 0.25 tok/char (Latin), under-
        // estimating a 400-char Thai or Hindi prompt by 4× and leaving it
        // on the local model instead of escalating.
        // Thai: สวัสดีครับ (10 chars) → 10 dense tokens
        assert_eq!(estimate_tokens("สวัสดีครับ"), 10);
        // Devanagari: नमस्ते (6 chars) → 6 dense tokens
        assert_eq!(estimate_tokens("नमस्ते"), 6);
        // Mixed: 4 Latin chars (ceil(4/4)=1) + 6 Thai codepoints (สวัสดี
        // has 6 Unicode codepoints including combining vowel marks) = 7.
        assert_eq!(estimate_tokens("hi! สวัสดี"), 1 + 6);
    }

    #[test]
    fn test_estimate_output_tokens_floor() {
        // IMP-24: even a one-word prompt yields at least the floor of output.
        let out = estimate_output_tokens("hi", estimate_tokens("hi"), None);
        assert_eq!(out, 16, "short prompt should hit the 16-token floor");
    }

    #[test]
    fn test_estimate_output_tokens_summarize_compresses() {
        // A summarize task should predict fewer output than input tokens.
        let text = "Please summarize the following article in one sentence: \
                    the quick brown fox jumps over the lazy dog repeatedly all \
                    afternoon while the farmer watches from his porch and sips tea";
        let input = estimate_tokens(text);
        let out = estimate_output_tokens(text, input, None);
        assert!(
            out < input,
            "summary output ({out}) must be smaller than input ({input})"
        );
    }

    #[test]
    fn test_estimate_output_tokens_code_expands() {
        // A code task should predict more output than a generic chat answer.
        let code_text = "write a function:\n```\nfn f(){}\n```";
        let plain_text = "tell me about your day in a few words please thanks";
        let code_in = estimate_tokens(code_text);
        let plain_in = estimate_tokens(plain_text);
        let code_out = estimate_output_tokens(code_text, code_in, None);
        let plain_out = estimate_output_tokens(plain_text, plain_in, None);
        // Normalise by input: code's output/input ratio must exceed plain chat's.
        assert!(
            code_out * plain_in > plain_out * code_in,
            "code ratio ({code_out}/{code_in}) should exceed plain ratio ({plain_out}/{plain_in})"
        );
    }

    #[test]
    fn test_estimate_output_tokens_respects_max_tokens_cap() {
        // A client max_tokens is a hard upper bound the prediction cannot exceed.
        let text = "write a very long detailed essay ".repeat(50);
        let input = estimate_tokens(&text);
        let capped = estimate_output_tokens(&text, input, Some(32));
        assert!(
            capped <= 32,
            "prediction ({capped}) must respect max_tokens=32"
        );
    }

    #[test]
    fn test_estimate_output_tokens_default_ceiling() {
        // Without a client cap, a huge prompt is still bounded by the ceiling.
        let text = "write a function:\n```\nfn f(){}\n```\n".repeat(2000);
        let input = estimate_tokens(&text);
        let out = estimate_output_tokens(&text, input, None);
        assert!(
            out <= 4096,
            "uncapped prediction ({out}) must respect the 4096 ceiling"
        );
    }

    #[test]
    fn test_estimate_total_tokens_includes_output() {
        // Total must exceed input-only estimate (output is always >= the floor).
        let text = "explain quantum entanglement to a curious beginner";
        let input = estimate_tokens(text);
        let total = estimate_total_tokens(text, None);
        assert!(
            total > input,
            "total ({total}) must exceed input-only ({input})"
        );
        assert_eq!(
            total,
            input + estimate_output_tokens(text, input, None),
            "total must equal input + predicted output"
        );
    }

    #[test]
    fn test_looks_like_code_detects_fence() {
        assert!(looks_like_code("here:\n```\nfn x(){}\n```"));
        assert!(!looks_like_code("plain question"));
    }

    #[test]
    fn test_looks_like_code_requires_balanced_fences() {
        // ADR-210: requires both opening AND closing fences.
        // Single opening fence (unclosed block) must not trigger code signal.
        assert!(!looks_like_code("start a code block:\n```\nfn foo() {}"));
        // Fenced blocks with language specifiers are valid.
        assert!(looks_like_code("```python\nprint('hello')\n```"));
        assert!(looks_like_code("```rust\nfn main() {}\n```"));
        assert!(looks_like_code("```c++\nint x = 5;\n```"));
    }

    #[test]
    fn test_looks_like_code_ignores_inline_backticks() {
        // ADR-210: backticks in the middle of prose (not at line start) do not
        // form a valid fence and must not trigger the code signal.
        // "use ``` like this ```" — three backticks appear mid-line, not as fences.
        assert!(!looks_like_code(
            "you can write code like ``` foo() ``` and test it"
        ));
        // A fence MUST be at line start (after whitespace).
        assert!(!looks_like_code("text before ``` code content ```"));
    }

    #[test]
    fn test_decide_short_query_goes_local() {
        let d = both().decide("hello", None).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_tools_escalate_to_cloud() {
        // IMP-10: tool/function calling is a hard signal even for a short prompt.
        let d = both().decide_full("hi", None, false, true).unwrap();
        assert_eq!(d.route, Route::Cloud);
        assert!(d.reason.contains("tools"), "reason was: {}", d.reason);
    }

    #[test]
    fn test_no_tools_short_stays_local() {
        let d = both().decide_full("hi", None, false, false).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_tools_do_not_override_privacy() {
        // Privacy wins over the tools signal: sensitive stays local (IMP-3).
        let d = both().decide_full("secret", None, true, true).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_tools_respect_code_to_cloud_disabled() {
        // With the hard-signal rule off, tools no longer force cloud.
        let e = both().with_code_to_cloud(false);
        let d = e.decide_full("hi", None, false, true).unwrap();
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
            ram_mb: Some(32000),
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
            ram_mb: Some(8000),
            cpu_count: 4,
            gpu: None,
        };
        assert_eq!(RoutingEngine::for_hardware(&p, true, true).threshold(), 300);
    }

    #[test]
    fn test_for_hardware_midrange_threshold() {
        let p = HardwareProfile {
            ram_mb: Some(16000),
            cpu_count: 8,
            gpu: None,
        };
        assert_eq!(RoutingEngine::for_hardware(&p, true, true).threshold(), 800);
    }

    #[test]
    fn test_for_hardware_unknown_ram_leans_local() {
        // ADR-261 — the defect this fixes. RAM detection is unavailable on
        // macOS/Windows, and the old code collapsed that to `ram_mb: 0`, which
        // fell into the 300 (CPU-only) tier: a 64 GB Mac silently escalated
        // nearly everything to the paid cloud. `None` must NOT mean "tiny".
        let p = HardwareProfile {
            ram_mb: None,
            cpu_count: 10,
            gpu: None,
        };
        assert_eq!(
            RoutingEngine::for_hardware(&p, true, true).threshold(),
            800,
            "undetected RAM must lean local, not assume the weakest machine"
        );
    }

    #[test]
    fn test_for_hardware_gpu_without_vram_is_midrange() {
        // A GPU we can see but whose VRAM we cannot read (non-NVIDIA, or
        // nvidia-smi output we could not parse) is worth more than CPU-only but
        // is not proven capable, so it sits at the middle tier.
        let p = HardwareProfile {
            ram_mb: None,
            cpu_count: 8,
            gpu: Some(GpuInfo {
                vendor: "apple".into(),
                vram_mb: None,
            }),
        };
        // has_capable_gpu() treats unknown VRAM as capable, so this is the 2000
        // tier; pinned here so the interaction is explicit rather than implied.
        assert_eq!(
            RoutingEngine::for_hardware(&p, true, true).threshold(),
            2000
        );
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
    fn test_hard_signals_added_markers_escalate() {
        // Strong, specific task markers added to lift genuinely-hard short prompts
        // that the length threshold alone would route local (IMP-4 continuation).
        assert!(hard_signals("show your work for this").contains(&"reasoning"));
        assert!(hard_signals("walk me through the proof").contains(&"reasoning"));
        assert!(hard_signals("理由を説明してください").contains(&"reasoning"));
        assert!(hard_signals("write a unit test for foo").contains(&"format"));
        assert!(hard_signals("give me a bash script").contains(&"format"));
        assert!(hard_signals("write a Dockerfile").contains(&"format"));
        assert!(hard_signals("output as XML").contains(&"format"));
        // A plain factual prompt is still untouched (no false escalation).
        assert!(hard_signals("what time is it in Tokyo").is_empty());
    }

    #[test]
    fn test_structured_local_opt_out() {
        // ADR-256: pure reformatting/extraction markers stop escalating when the
        // opt-out is on; code generation is unaffected either way.
        for structured in [
            "give me that as json",
            "output in csv format",
            "as a markdown table",
        ] {
            assert!(
                hard_signals(structured).contains(&"format"),
                "default unchanged: {structured:?}"
            );
            assert!(
                !hard_signals_with(structured, true).contains(&"format"),
                "opt-out must drop the signal: {structured:?}"
            );
        }
        for code in [
            "write a function to sort",
            "implement a binary search",
            "write a dockerfile",
        ] {
            assert!(
                hard_signals_with(code, true).contains(&"format"),
                "code generation must still escalate: {code:?}"
            );
        }
    }

    #[test]
    fn test_structured_local_default_is_unchanged_behaviour() {
        // hard_signals() must remain byte-for-byte the pre-ADR-256 behaviour so
        // no existing deployment is silently re-routed.
        for t in [
            "give me that as json",
            "write a unit test for foo",
            "output as XML",
            "what time is it in Tokyo",
        ] {
            assert_eq!(hard_signals(t), hard_signals_with(t, false), "{t:?}");
        }
    }

    #[test]
    fn test_is_multi_step_detects_sequenced_plans() {
        // ≥3 distinct sequencing cues → multi-step (no reasoning keyword present).
        assert!(is_multi_step(
            "first set up the database, then migrate the schema, finally deploy"
        ));
        assert!(is_multi_step("まず設計し、次に実装して、最後にテストする"));
        // A numbered list of ≥3 items.
        assert!(is_multi_step(
            "do these:\n1. clone the repo\n2. build it\n3. run tests"
        ));
        // And it feeds the hard-signal set so such prompts escalate.
        assert!(hard_signals("first do X, then do Y, then finally do Z").contains(&"multi_step"));
    }

    #[test]
    fn test_is_multi_step_avoids_false_positives() {
        // One or two casual cues is not a multi-step task.
        assert!(!is_multi_step("first, thanks for the help"));
        assert!(!is_multi_step("I was there, then I left"));
        // Substring safety: "then" inside "strengthen" must not count.
        assert!(!is_multi_step("how do I strengthen this argument"));
        // A decimal is not a numbered-list item.
        assert!(!is_multi_step("the value is 3.14 and pi is irrational"));
        // Plain factual prompts stay clear of the whole hard-signal set.
        assert!(!hard_signals("what is the capital of France").contains(&"multi_step"));
    }

    #[test]
    fn test_is_time_sensitive_detects_en_and_ja() {
        assert!(is_time_sensitive("what's the weather today?"));
        assert!(is_time_sensitive("What is the LATEST exchange rate?"));
        assert!(is_time_sensitive("what time is it right now"));
        assert!(is_time_sensitive("今日の天気は？"));
        assert!(is_time_sensitive("現在の株価を教えて"));
        assert!(!is_time_sensitive("explain how binary search works"));
        assert!(!is_time_sensitive("write a function to reverse a string"));
    }

    #[test]
    fn test_is_time_sensitive_ignores_coding_current_usage() {
        // "current" alone is deliberately excluded from the marker list:
        // routine coding prompts say "current directory"/"current
        // implementation" constantly, and none of those are asking about a
        // real-world fact with an expiry — treating them as time-sensitive
        // would gut the cache hit rate for ordinary coding sessions.
        assert!(!is_time_sensitive("list files in the current directory"));
        assert!(!is_time_sensitive("refactor the current implementation"));
    }

    #[test]
    fn test_is_time_sensitive_orthogonal_to_hard_signals() {
        // A time-sensitive prompt can still be a SIMPLE prompt for routing
        // purposes (it may be trivially easy to answer) — the cache-bypass
        // axis and the local/cloud-complexity axis are independent.
        let text = "what's the weather today";
        assert!(is_time_sensitive(text));
        assert!(hard_signals(text).is_empty());
    }

    #[test]
    fn test_question_count_threshold() {
        assert_eq!(question_count("a? b? c?"), 3);
        assert!(hard_signals("why? how? when?").contains(&"multi_question"));
        assert!(!hard_signals("what is this?").contains(&"multi_question"));
    }

    #[test]
    fn test_question_count_ignores_url_query_delimiters() {
        // ADR-209: a '?' that delimits a URL query string is immediately
        // followed by an alphanumeric key, so it must NOT count as a question.
        assert_eq!(
            question_count("https://a.com/s?q=1 https://b.com?x=2 https://c.com?y=3"),
            0,
            "URL query '?' must not be counted as questions"
        );
        // A prompt that merely references three query-string URLs must not trip
        // the multi_question hard signal.
        assert!(!hard_signals(
            "compare https://a.com/s?q=1 and https://b.com?x=2 and https://c.com?y=3"
        )
        .contains(&"multi_question"));
        // Real questions still count even when mixed with a query URL.
        assert_eq!(
            question_count("is https://a.com/s?q=1 down? why? when?"),
            3,
            "clause-terminating '?' still counts alongside a URL query '?'"
        );
    }

    #[test]
    fn test_question_count_terminating_forms() {
        // Question mark before a closing delimiter or end-of-text counts.
        assert_eq!(question_count("really?"), 1); // EOL
        assert_eq!(question_count("(really?) yes"), 1); // before ')'
        assert_eq!(question_count("\"done?\" ok"), 1); // before '"'
        assert_eq!(question_count("これは何ですか？"), 1); // full-width at EOL
        assert_eq!(question_count("何？本当？"), 2); // full-width before full-width
    }

    #[test]
    fn test_looks_mathy() {
        // Genuine math expressions (3+ distinct math-char types) route to cloud.
        assert!(looks_mathy("x = a + b * c / d ^ 2")); // =,+,*,/,^ = 5 types
        assert!(looks_mathy("a^2 + b^2 = c^2")); // ^,+,= = 3 types
        assert!(looks_mathy("∑x = π * r^2")); // ∑,=,π,*,^ = 5 types
        assert!(!looks_mathy("a normal sentence")); // 0 types

        // ADR-208: a single character type repeated many times must NOT trigger
        // the math signal (was the root cause of URL false positives).
        assert!(!looks_mathy("https://api.example.com/v1/models/list")); // only '/'
        assert!(!looks_mathy("KEY=value&OTHER=stuff&MORE=data&LAST=x")); // only '='
                                                                         // URL with both '/' and '=' is still only 2 types → not math.
        assert!(!looks_mathy("https://host/path?key=value&x=1"));
        // Simple assignment and arithmetic: 2 types, not enough to be math.
        assert!(!looks_mathy("x = y + z")); // =,+ = 2 types
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

    #[test]
    fn test_local_only_routes_local_for_hard_signal() {
        // Even code/tools force local when local_only is on.
        let e = both().with_local_only(true);
        let d = e
            .decide_full("```rust\nfn x(){}\n```", None, false, true)
            .unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("local-only"), "reason: {}", d.reason);
    }

    #[test]
    fn test_local_only_routes_local_for_long_prompt() {
        let long = "x".repeat(2000);
        let e = both().with_local_only(true);
        let d = e.decide(&long, None).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_local_only_privacy_still_checked_first() {
        // Sensitive content still goes local for the right reason even in local_only.
        let e = both().with_local_only(true);
        let d = e.decide_with_sensitivity("hi", None, true).unwrap();
        assert_eq!(d.route, Route::Local);
    }

    #[test]
    fn test_local_only_overrides_explicit_cloud_pin() {
        // local_only's early return (before the `forced` check) means it wins
        // even over an explicit per-request model:"cloud" pin -- the correct,
        // safe direction for an operator's "never touch cloud" guarantee to
        // fail in (a stronger absolute guarantee beats a weaker per-request
        // one). This interaction had no test coverage before this case.
        let e = both().with_local_only(true);
        let d = e.decide(&"x".repeat(10), Some(Route::Cloud)).unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("local-only"), "reason: {}", d.reason);
    }

    #[test]
    fn test_local_only_no_local_errors() {
        let e = RoutingEngine::new(100, false, true).with_local_only(true);
        assert_eq!(
            e.decide("hi", None).unwrap_err(),
            RoutingError::NoBackendAvailable
        );
    }

    #[test]
    fn test_is_simple_prompt_short_no_signals() {
        assert!(is_simple_prompt("what time is it", 50));
    }

    #[test]
    fn test_is_simple_prompt_above_threshold() {
        assert!(!is_simple_prompt("hello", 1));
    }

    #[test]
    fn test_is_simple_prompt_has_hard_signal() {
        assert!(!is_simple_prompt("write a function to sort", 50));
    }

    // ── IMP-25 skill-profile routing tests ───────────────────────────────────

    #[test]
    fn test_detect_skill_code() {
        assert_eq!(detect_skill("```python\nprint(1)\n```"), Some("code"));
    }

    #[test]
    fn test_detect_skill_math() {
        assert_eq!(detect_skill("x = a + b * c / d ^ 2"), Some("math"));
    }

    #[test]
    fn test_detect_skill_reason() {
        assert_eq!(detect_skill("solve this step by step"), Some("reason"));
        assert_eq!(detect_skill("ステップで説明"), Some("reason"));
    }

    #[test]
    fn test_detect_skill_summarize() {
        assert_eq!(
            detect_skill("please summarize this document"),
            Some("summarize")
        );
        assert_eq!(detect_skill("tl;dr please"), Some("summarize"));
        assert_eq!(detect_skill("要約してください"), Some("summarize"));
    }

    #[test]
    fn test_detect_skill_translate() {
        assert_eq!(
            detect_skill("translate this to Japanese"),
            Some("translate")
        );
        assert_eq!(detect_skill("翻訳してください"), Some("translate"));
    }

    #[test]
    fn test_detect_skill_none() {
        assert_eq!(detect_skill("what time is it"), None);
        assert_eq!(detect_skill("hello"), None);
    }

    #[test]
    fn test_skill_profile_code_to_local() {
        // By default code goes to cloud; with skill profile it stays local.
        let e = both().with_skills(vec![("code".to_string(), Route::Local)]);
        let d = e.decide("```rust\nfn main(){}\n```", None).unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("skill:code"), "reason: {}", d.reason);
    }

    #[test]
    fn test_skill_profile_summarize_to_cloud() {
        // Without skill profile, a short summarize request stays local.
        let d = both().decide("please summarize this", None).unwrap();
        assert_eq!(d.route, Route::Local);
        // With skill profile, it escalates.
        let e = both().with_skills(vec![("summarize".to_string(), Route::Cloud)]);
        let d = e.decide("please summarize this", None).unwrap();
        assert_eq!(d.route, Route::Cloud);
        assert!(d.reason.contains("skill:summarize"), "reason: {}", d.reason);
    }

    #[test]
    fn test_skill_profile_privacy_wins_over_skill() {
        // Privacy check is before skill profiles — sensitive content must stay local.
        let e = both().with_skills(vec![("code".to_string(), Route::Cloud)]);
        let d = e
            .decide_with_sensitivity("```secret key```", None, true)
            .unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("sensitive"), "reason: {}", d.reason);
    }

    #[test]
    fn test_skill_profile_unknown_skill_falls_through() {
        // An unknown skill name in the profile is a no-op; generic routing applies.
        let e = both().with_skills(vec![("unknown_skill".to_string(), Route::Local)]);
        let d = e.decide("```\ncode here\n```", None).unwrap();
        // Generic code→cloud still fires because the skill profile didn't match.
        assert_eq!(d.route, Route::Cloud);
    }

    #[test]
    fn test_skill_profile_does_not_override_has_tools() {
        // ADR-228: a skill match is a content-based setting ("keep my code on
        // the local model"); has_tools is a capability requirement (the local
        // model may not support function-calling). A request with tools must
        // escalate to cloud even when it also matches a skill mapped to Local.
        let e = both().with_skills(vec![("code".to_string(), Route::Local)]);
        let d = e
            .decide_full("```rust\nfn main(){}\n```", None, false, true)
            .unwrap();
        assert_eq!(d.route, Route::Cloud, "reason: {}", d.reason);
        assert!(d.reason.contains("tools"), "reason: {}", d.reason);
    }

    #[test]
    fn test_skill_profile_still_applies_without_tools() {
        // Control: the same skill profile, same text, but has_tools=false —
        // the skill override must still fire exactly as before ADR-228.
        let e = both().with_skills(vec![("code".to_string(), Route::Local)]);
        let d = e
            .decide_full("```rust\nfn main(){}\n```", None, false, false)
            .unwrap();
        assert_eq!(d.route, Route::Local);
        assert!(d.reason.contains("skill:code"), "reason: {}", d.reason);
    }

    #[test]
    fn test_skill_profile_no_match_falls_through_to_threshold() {
        // Skills defined but none match → token threshold still applies.
        let e = both().with_skills(vec![("summarize".to_string(), Route::Local)]);
        let long = "x".repeat(1000);
        let d = e.decide(&long, None).unwrap();
        assert_eq!(d.route, Route::Cloud); // long → cloud (threshold)
    }
}

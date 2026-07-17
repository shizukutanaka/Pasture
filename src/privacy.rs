//! Privacy classification (IMP-3, grounded in PRISM arXiv:2511.22788 and the
//! "sensitive data stays local" pattern in competitor tools).
//!
//! Detects whether a prompt likely contains sensitive content so the router
//! can keep it on the local model and never send it to the cloud. Only
//! *category labels* are returned — never the matched values (I5: PII is not
//! stored, logged, or transmitted).

/// The outcome of classifying a prompt. Categories are stable labels, never
/// the matched substrings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitivityReport {
    pub categories: Vec<&'static str>,
}

impl SensitivityReport {
    pub fn is_sensitive(&self) -> bool {
        !self.categories.is_empty()
    }
}

/// Sensitive keyword markers (case-insensitive, EN + JA).
const KEYWORDS: &[&str] = &[
    // --- credentials / keys ---
    "password",
    "passwd",
    "api key",
    "api_key",
    "apikey",
    "secret key",
    "client_secret",
    "access_token",
    "private key",
    "bearer token",
    "auth token",
    "refresh token",
    "signing key",
    "encryption key",
    // --- financial ---
    "credit card",
    "bank account",
    "account number",
    "routing number",
    "swift code",
    "iban",
    // --- identity / government ---
    "social security",
    "ssn",
    "passport",
    "national id",
    "taxpayer id",
    "driver's license",
    "driver license",
    "date of birth",
    // --- Japanese identity & PII ---
    "パスワード",
    "秘密鍵",
    "マイナンバー",
    "個人番号",
    "クレジットカード",
    "生年月日",
    "口座番号",
    "保険証",
    "年金番号",
    "運転免許",
    "在留カード",
    "住所",
    "氏名",
    "電話番号",
    "銀行口座",
];

/// Token prefixes that strongly indicate a leaked credential.
const KEY_PREFIXES: &[&str] = &[
    // OpenAI / Anthropic
    "sk-",
    // GitHub
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    // Slack (bot, user, and app-level tokens)
    "xoxb-",
    "xoxp-",
    "xapp-",
    // GitLab
    "glpat-",
    // AWS
    "AKIA",
    "ASIA",
    // Google (service account / OAuth)
    "AIza",
    "ya29.",
    // Stripe
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "whsec_",
    // SendGrid
    "SG.",
    // npm
    "npm_",
    // DigitalOcean
    "dop_v1_",
    // HashiCorp Vault
    "hvs.",
    // HuggingFace (user/org/fine-grained access tokens)
    "hf_",
    // Twilio (auth token is 32 hex chars, but SID starts with AC — too generic; skip)
    // Cloudflare
    "v1.0-",
];

/// Substrings in environment-variable names that suggest a secret value.
const ENV_SECRET_SUBSTRINGS: &[&str] = &[
    "pass",       // PASSWORD, DB_PASS
    "secret",     // CLIENT_SECRET, SECRET_KEY
    "token",      // ACCESS_TOKEN, AUTH_TOKEN
    "api_key",    // STRIPE_API_KEY
    "apikey",     // APIKEY
    "auth",       // OAUTH_TOKEN, AUTH_SECRET
    "credential", // AWS_CREDENTIALS
    "private",    // PRIVATE_KEY
    "pwd",        // DB_PWD
    "_key",       // SIGNING_KEY, STRIPE_KEY
];

/// Normalize text for numeric-PII detection (ADR-213, ADR-214). Maps:
///   - full-width digits (U+FF10..=U+FF19, `０`..`９`) → ASCII `0`..`9`;
///   - the full-width full stop (`．`, U+FF0E) → `.` (so a full-width IPv4
///     `１９２．１６８．１．１` is detected);
///   - the full-width hyphen-minus (`－`, U+FF0D) and the common Unicode dash
///     variants (`‐‑‒–—―`, U+2010..=U+2015) → ASCII `-` (so a full-width
///     credit card / My Number with dash separators is detected);
///   - the ideographic space (`　`, U+3000) → ASCII space (so full-width-spaced
///     groups tokenize correctly).
/// All other characters are unchanged. This is the "Layer 1 normalization" step
/// common to Japanese-PII pipelines; it is std-only and deliberately targeted
/// (not a full NFKC, which would require an external crate) — it covers exactly
/// the digit and separator characters that appear in numeric PII. The classifier
/// runs the digit-based detectors over the normalized text; because the
/// classifier returns only category labels (never byte offsets), the byte-length
/// change from normalization does not matter here.
pub fn normalize_for_detection(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            // '０' (U+FF10) → '0' … '９' (U+FF19) → '9'
            '\u{FF10}'..='\u{FF19}' => char::from(b'0' + (c as u32 - 0xFF10) as u8),
            // Full-width full stop → ASCII dot.
            '\u{FF0E}' => '.',
            // Full-width hyphen-minus and the Unicode dash family → ASCII hyphen.
            '\u{FF0D}' | '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}'
            | '\u{2015}' => '-',
            // Ideographic space → ASCII space.
            '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Classify a prompt's sensitivity. Returns category labels only.
pub fn classify(text: &str) -> SensitivityReport {
    let mut categories: Vec<&'static str> = Vec::new();
    let lower = text.to_lowercase();
    // ADR-213/214: normalize full-width digits and separators (dot/dash/space)
    // to ASCII for the digit-based detectors so numeric PII typed in full-width
    // form (common in Japanese input) is still detected and kept local.
    let normalized = normalize_for_detection(text);

    if KEYWORDS.iter().any(|k| lower.contains(k)) {
        categories.push("keyword");
    }
    if text.split_whitespace().any(looks_like_email) {
        categories.push("email");
    }
    if normalized
        .split_whitespace()
        .any(|t| looks_like_ipv4(t) || looks_like_ipv6(t))
    {
        categories.push("ip");
    }
    if contains_credit_card(&normalized) {
        categories.push("credit_card");
    }
    if contains_my_number(&normalized) {
        categories.push("my_number");
    }
    if contains_iban(&normalized) {
        categories.push("iban");
    }
    if normalized.split_whitespace().any(looks_like_phone) || contains_intl_phone(&normalized) {
        categories.push("phone");
    }
    if text.split_whitespace().any(looks_like_api_key) || contains_embedded_api_key(text) {
        categories.push("api_key");
    }
    if text.split_whitespace().any(looks_like_jwt) {
        categories.push("jwt");
    }
    if contains_pem_key(text) {
        categories.push("pem_key");
    }
    if contains_url_credential(text) {
        categories.push("url_credential");
    }
    if contains_env_secret(text) {
        categories.push("env_secret");
    }

    SensitivityReport { categories }
}

/// Detect a PEM-encoded private key anywhere in the text.
/// Near-zero false positives: the exact marker only appears in PEM key files.
pub fn contains_pem_key(text: &str) -> bool {
    text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----")
}

/// Returns byte ranges of complete PEM private-key blocks in `text` (ADR-204).
/// Each span covers the entire block from `-----BEGIN` to the closing `-----`
/// of the matching `-----END` footer, so the pseudonymizer can replace the whole
/// block (header + base64 body + footer) with a single `<PEM_n>` token.
///
/// Supported types: RSA PRIVATE KEY, EC PRIVATE KEY, PRIVATE KEY (PKCS#8),
/// OPENSSH PRIVATE KEY. Public keys (`PUBLIC KEY`) and certificates (`CERTIFICATE`)
/// are not matched — they do not contain "PRIVATE KEY".
///
/// Note: `contains_pem_key` is deliberately kept more permissive (an unpaired
/// BEGIN header is enough to flag the text as sensitive and keep it local).
/// `pem_key_spans` only returns spans for complete, paired blocks; an unpaired
/// BEGIN with no END is not returned (there is nothing coherent to mask).
pub fn pem_key_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut search_start = 0;
    while let Some(begin_rel) = text[search_start..].find("-----BEGIN") {
        let begin_abs = search_start + begin_rel;
        let after_begin = &text[begin_abs + 10..];
        // The begin header ends with "PRIVATE KEY-----" (covers RSA/EC/PKCS8/OPENSSH).
        let Some(pk_rel) = after_begin.find("PRIVATE KEY-----") else {
            // No "PRIVATE KEY-----" after this "-----BEGIN" → not a private-key block.
            search_start = begin_abs + 10;
            continue;
        };
        let header_end = begin_abs + 10 + pk_rel + "PRIVATE KEY-----".len();
        // Find the "-----END" footer after the base64 body.
        let Some(end_rel) = text[header_end..].find("-----END") else {
            search_start = header_end;
            continue;
        };
        let end_abs = header_end + end_rel;
        // Find the closing "-----" at the end of the footer line.
        let Some(close_rel) = text[end_abs + 8..].find("-----") else {
            search_start = end_abs + 8;
            continue;
        };
        let block_end = end_abs + 8 + close_rel + 5;
        spans.push((begin_abs, block_end));
        search_start = block_end;
    }
    spans
}

/// Returns byte ranges of the `userinfo` (user:password) inside each URL with
/// embedded credentials (ADR-203). Each span covers the userinfo only (not the
/// `://` or `@`), so the pseudonymizer can replace only the credential while
/// preserving the scheme and host. Example:
///   `https://user:pass@host/path` → span = byte range of `user:pass`.
/// Port-only URLs (`http://host:8080/path`) are not matched — they have no `@`.
pub fn url_credential_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut search_start = 0;
    while let Some(rel) = text[search_start..].find("://") {
        let after_start = search_start + rel + 3;
        let after = &text[after_start..];
        let authority_end = after.find('/').unwrap_or(after.len());
        let authority = &after[..authority_end];
        if let Some(at_pos) = authority.find('@') {
            let user_info = &authority[..at_pos];
            if let Some(colon) = user_info.find(':') {
                let password = &user_info[colon + 1..];
                // A password that is purely numeric (all digits) is likely a port
                // number from a host:port construct (e.g. "api.example.com:8080")
                // rather than an actual password. Skip it to avoid false positives
                // (ADR-211).
                let is_numeric_port =
                    !password.is_empty() && password.chars().all(|c| c.is_ascii_digit());
                if !password.is_empty() && !is_numeric_port {
                    spans.push((after_start, after_start + at_pos));
                }
            }
        }
        search_start = after_start;
    }
    spans
}

/// Detect URL-embedded credentials: `scheme://user:password@host`.
/// Requires a non-empty password part after the colon to avoid matching
/// `http://host:8080/path` (port-only, no user info).
pub fn contains_url_credential(text: &str) -> bool {
    !url_credential_spans(text).is_empty()
}

/// Returns byte ranges of the **value** (the secret part) in each secret
/// environment-variable assignment found in `text` (ADR-203). The span covers
/// the raw value text including any surrounding quotes, so the pseudonymizer can
/// replace the value while preserving the key name and `=`.
/// Example: `DB_PASSWORD=hunter2\n` → span = byte range of `hunter2`.
pub fn env_secret_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut line_start = 0;
    for line in text.lines() {
        let raw_line = &text[line_start..line_start + line.len()];
        let trimmed_offset = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        let (trimmed, export_add) = if let Some(s) = trimmed.strip_prefix("export ") {
            (s, "export ".len())
        } else {
            (trimmed, 0)
        };
        if let Some(eq_pos) = trimmed.find('=') {
            let key = trimmed[..eq_pos].trim();
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                let key_lower = key.to_lowercase();
                if ENV_SECRET_SUBSTRINGS.iter().any(|s| key_lower.contains(s)) {
                    let raw_val = &trimmed[eq_pos + 1..];
                    let val = raw_val.trim().trim_matches('"').trim_matches('\'').trim();
                    let trivial = val.is_empty()
                        || matches!(val, "true" | "false" | "0" | "1" | "none" | "null" | "''");
                    if !trivial && val.len() >= 4 {
                        // Span covers the raw value (including any surrounding quotes)
                        // from after the `=` sign in the original text.
                        let val_start_in_raw = trimmed_offset + export_add + eq_pos + 1;
                        // Trim leading whitespace from the value start.
                        let leading_ws = raw_val.len() - raw_val.trim_start().len();
                        let span_start = line_start + val_start_in_raw + leading_ws;
                        // Trim trailing whitespace/newline from the value end.
                        let raw_val_trimmed = raw_val.trim();
                        let span_end = span_start + raw_val_trimmed.len();
                        if span_start < span_end && span_end <= line_start + raw_line.len() {
                            spans.push((span_start, span_end));
                        }
                    }
                }
            }
        }
        // Advance past this line + the newline character (\n or nothing at EOF).
        line_start += line.len();
        if line_start < text.len()
            && (text.as_bytes()[line_start] == b'\n' || text.as_bytes()[line_start] == b'\r')
        {
            // Skip \r\n or \n
            if text.as_bytes()[line_start] == b'\r'
                && line_start + 1 < text.len()
                && text.as_bytes()[line_start + 1] == b'\n'
            {
                line_start += 2;
            } else {
                line_start += 1;
            }
        }
    }
    spans
}

/// Detect environment-variable secret assignments, e.g.:
/// `SECRET_KEY=abc123`, `export DB_PASSWORD='hunter2'`, `API_TOKEN="xyz"`.
/// Matches lines where the variable name contains a secret-sounding substring
/// and the value is non-trivial (length ≥ 4, not a bare boolean/null/empty).
pub fn contains_env_secret(text: &str) -> bool {
    !env_secret_spans(text).is_empty()
}

/// Heuristic email: `local@domain.tld`, no spaces, dot after the `@`.
pub fn looks_like_email(token: &str) -> bool {
    let t = token.trim_matches(|c: char| !c.is_alphanumeric());
    let Some((local, domain)) = t.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-')
}

/// Heuristic IPv4: four dot-separated octets in 0..=255.
pub fn looks_like_ipv4(token: &str) -> bool {
    let t = token.trim_matches(|c: char| c != '.' && !c.is_ascii_digit());
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty() && p.len() <= 3 && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
    })
}

/// Heuristic IPv6: a whitespace token that parses as a valid IPv6 address after
/// trimming wrapping punctuation (e.g. the brackets in a `[2001:db8::1]` URL
/// authority). Delegates the hard part to the std parser, so false positives on
/// ordinary text or code are near-zero: the `>= 2` colon pre-check rejects times
/// and ranges (`10:30`, `a:b`) before the parser ever sees them, and any token
/// containing a non-hex, non-colon character fails to parse. IPv6 addresses
/// identify a host exactly as IPv4 does, so they share the `"ip"` category and
/// the same force-local treatment.
pub fn looks_like_ipv6(token: &str) -> bool {
    let t = token.trim_matches(|c: char| !(c.is_ascii_hexdigit() || c == ':'));
    // Require at least two colons so a bare `h:m` time or `a:b` range never
    // reaches the parser (a valid IPv6 address always has two or more).
    if t.matches(':').count() < 2 {
        return false;
    }
    t.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Heuristic phone number. Matches international `+` form (8..=15 digits) and
/// common Japanese domestic forms: mobile `0[789]0XXXXXXXX` (11 digits, with or
/// without hyphens) and hyphenated landline/other `0...` numbers (10..=11
/// digits). Requiring a leading `0` and either a mobile prefix or a hyphen keeps
/// false positives low for a Japanese-first audience.
pub fn looks_like_phone(token: &str) -> bool {
    // International: +<country><number>.
    if let Some(rest) = token.strip_prefix('+') {
        let digits = rest.chars().filter(|c| c.is_ascii_digit()).count();
        let only_phone_chars = rest
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == ' ' || c == '(' || c == ')');
        return only_phone_chars && (8..=15).contains(&digits);
    }
    // Domestic: digits with optional hyphens only.
    if !token.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return false;
    }
    let has_hyphen = token.contains('-');
    let digits: Vec<char> = token.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.len();
    if digits.first() != Some(&'0') {
        return false;
    }
    // JP mobile (070/080/090 + 8 digits): unambiguous even without hyphens.
    let mobile = n == 11 && matches!(digits[1], '7' | '8' | '9') && digits[2] == '0';
    if mobile {
        return true;
    }
    // Other domestic numbers: only when hyphenated, to avoid matching bare IDs.
    has_hyphen && (10..=11).contains(&n)
}

/// Strip the JSON/prose delimiters that wrap a credential quoted in code or
/// text (`"sk-…"`, `(sk-…)`, `[eyJ…]`, `` `tok` ``). These characters never
/// appear inside a real token, so trimming them from both ends lets the prefix
/// checks see the bare token. `-` and `_` are deliberately **not** stripped —
/// they are valid inside prefixes (`ghp_`, `glpat-`, `xoxb-`) and JWT segments.
fn trim_token_delimiters(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '<' | '>'
        )
    })
}

/// Heuristic credential token: a known prefix and enough length. Surrounding
/// punctuation is stripped first so a key quoted in JSON or prose
/// (`"sk-…"`, `(sk-…)`, `sk-…,`) is still detected.
pub fn looks_like_api_key(token: &str) -> bool {
    let t = trim_token_delimiters(token);
    KEY_PREFIXES
        .iter()
        .any(|p| t.starts_with(p) && t.len() >= p.len() + 12)
}

/// Full-text scan for API-key prefixes that are adjacent to non-whitespace
/// content with no surrounding spaces — the case that `split_whitespace` +
/// `trim_token_delimiters` cannot reach (ADR-101).
///
/// Example: `{"authorization":"sk-abcdef1234567890"}` is a single whitespace
/// token whose ends trim to `"authorization":"sk-abcdef1234567890"` — the
/// interior `sk-` is never exposed to `starts_with`. This function finds the
/// prefix anywhere in the text as long as (a) the preceding character is not
/// alphanumeric (so "skiing" never matches "sk-") and (b) at least 12
/// non-whitespace characters follow the prefix (same minimum as
/// `looks_like_api_key`).
pub fn contains_embedded_api_key(text: &str) -> bool {
    for prefix in KEY_PREFIXES {
        let mut haystack = text;
        while let Some(pos) = haystack.find(prefix) {
            // Preceding character must not be alphanumeric to avoid false
            // positives from longer words that happen to contain the prefix.
            let preceding_ok = pos == 0
                || haystack[..pos]
                    .chars()
                    .last()
                    .map(|c| !c.is_alphanumeric())
                    .unwrap_or(true);
            if preceding_ok {
                let after = &haystack[pos + prefix.len()..];
                let non_ws = after.chars().take_while(|c| !c.is_whitespace()).count();
                if non_ws >= 12 {
                    return true;
                }
            }
            // Advance past this occurrence to keep searching.
            haystack = &haystack[pos + prefix.len()..];
        }
    }
    false
}

/// Heuristic JWT / bearer token: header.payload.signature where the header is
/// base64url of `{"...` (begins with `eyJ`). Near-zero false positives.
/// Surrounding punctuation is stripped first (shared with `looks_like_api_key`)
/// so a JWT quoted or parenthesised in prose is still detected.
pub fn looks_like_jwt(token: &str) -> bool {
    let t = trim_token_delimiters(token);
    if !t.starts_with("eyJ") {
        return false;
    }
    let parts: Vec<&str> = t.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let is_b64url = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
    };
    parts.iter().all(|p| is_b64url(p)) && t.len() >= 20
}

/// Detect a credit-card-like number anywhere in the text: a run of digits
/// (with optional spaces/hyphens) of length 13..=19 that passes the Luhn check.
pub fn contains_credit_card(text: &str) -> bool {
    !credit_card_spans(text).is_empty()
}

/// Byte ranges of Luhn-valid 13–19 digit credit-card numbers, where the number
/// may contain internal space/hyphen separators. The end of each span is the
/// byte just after the final digit, so trailing separators are not included.
/// Shared by `contains_credit_card` (any hit → sensitive) and the pseudonymizer
/// (mask each span with a `<CARD_n>` token, ADR-196) so card detection has a
/// single source of truth.
pub fn credit_card_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            // Consume a maximal digit/sep run, tracking the end of the last digit
            // so a trailing separator is excluded from the reported span.
            let mut digits: Vec<u8> = Vec::new();
            let mut j = i;
            let mut last_digit_end = i;
            while j < bytes.len() {
                let cj = bytes[j];
                if cj.is_ascii_digit() {
                    digits.push(cj - b'0');
                    j += 1;
                    last_digit_end = j;
                } else if cj == b' ' || cj == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            if (13..=19).contains(&digits.len()) && luhn_valid(&digits) {
                spans.push((i, last_digit_end));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    spans
}

/// Detect an international (`+`-prefixed) phone number anywhere in the text,
/// including the space- or hyphen-separated form that spans multiple whitespace
/// tokens (e.g. `+1 555 123 4567`). The classifier and the per-token
/// pseudonymizer both split on whitespace before calling `looks_like_phone`, so
/// the space-allowing branch of that function is unreachable for a number whose
/// groups are space-separated; this whitespace-agnostic scan closes that gap
/// (ADR-207).
pub fn contains_intl_phone(text: &str) -> bool {
    !phone_spans(text).is_empty()
}

/// Byte ranges of international (`+`-prefixed) phone numbers, where the number
/// may contain internal space/hyphen/paren separators. Each span starts at the
/// `+` and ends just after the final digit, so trailing separators are excluded.
/// The digit-count window (8..=15) mirrors `looks_like_phone`'s international
/// branch, so detection has a single source of truth; the only difference is
/// that this scan is whitespace-agnostic and therefore catches the
/// `+1 555 123 4567` form the per-token path cannot (ADR-207). Shared by
/// `contains_intl_phone` (any hit → sensitive) and the pseudonymizer (mask each
/// span with a `<PHONE_n>` token).
pub fn phone_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' {
            // Consume a maximal phone-char run after the '+', tracking the end of
            // the last digit so trailing separators are excluded from the span.
            let mut digit_count = 0usize;
            let mut j = i + 1;
            let mut last_digit_end = i; // no digit yet
            while j < bytes.len() {
                let cj = bytes[j];
                if cj.is_ascii_digit() {
                    digit_count += 1;
                    j += 1;
                    last_digit_end = j;
                } else if matches!(cj, b' ' | b'-' | b'(' | b')') {
                    j += 1;
                } else {
                    break;
                }
            }
            // Same window as looks_like_phone's '+<country><number>' branch.
            if (8..=15).contains(&digit_count) {
                spans.push((i, last_digit_end));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    spans
}

fn luhn_valid(digits: &[u8]) -> bool {
    let mut sum = 0u32;
    let mut alt = false;
    for &d in digits.iter().rev() {
        let mut v = d as u32;
        if alt {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
        alt = !alt;
    }
    sum % 10 == 0
}

/// Detect a Japanese Individual Number (マイナンバー / My Number) anywhere in
/// the text (ADR-212). My Number is a 12-digit identifier with a check digit,
/// so detection is checksum-validated like credit cards, keeping false
/// positives low (a random 12-digit run passes with probability ~1/11).
pub fn contains_my_number(text: &str) -> bool {
    !my_number_spans(text).is_empty()
}

/// Byte ranges of valid 12-digit Japanese My Numbers (マイナンバー), where the
/// number may contain internal space/hyphen separators (common forms:
/// `123456789018`, `1234 5678 9018`, `1234-5678-9018`). The end of each span is
/// the byte just after the final digit, so trailing separators are not included.
/// Shared by `contains_my_number` (any hit → sensitive) and the pseudonymizer
/// (mask each span with a `<MYNUMBER_n>` token) so detection has a single source
/// of truth. The check digit (12th digit) is validated to suppress false
/// positives on arbitrary 12-digit runs (ADR-212).
pub fn my_number_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        // Only start a candidate at a digit boundary (preceding byte not a digit)
        // so a 13+ digit run is not mistaken for a 12-digit My Number prefix.
        let at_boundary = i == 0 || !bytes[i - 1].is_ascii_digit();
        if c.is_ascii_digit() && at_boundary {
            let mut digits: Vec<u8> = Vec::new();
            let mut j = i;
            let mut last_digit_end = i;
            while j < bytes.len() {
                let cj = bytes[j];
                if cj.is_ascii_digit() {
                    digits.push(cj - b'0');
                    j += 1;
                    last_digit_end = j;
                } else if cj == b' ' || cj == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            // Exactly 12 digits AND a valid check digit. The exact-length guard
            // distinguishes My Number from credit cards (13-19 digits).
            if digits.len() == 12 && my_number_check_valid(&digits) {
                spans.push((i, last_digit_end));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    spans
}

/// Validate the My Number check digit (検査用数字). The 12th digit is computed
/// from the first 11 via a weighted modulo-11 sum:
///   check = 11 - (Σ P_n·Q_n mod 11), where the result is 0 if the remainder ≤ 1.
/// P_n is the n-th digit counting from the lowest non-check digit (P_1 = 11th
/// digit from the left), and Q_n is the weight: n+1 for 1≤n≤6, n-5 for 7≤n≤11.
fn my_number_check_valid(digits: &[u8]) -> bool {
    if digits.len() != 12 {
        return false;
    }
    // digits[0..11] are P_11..P_1 (left-to-right); P_1 is the lowest non-check
    // digit = digits[10]. Iterate n = 1..=11 with P_n = digits[11 - n].
    let mut sum = 0u32;
    for n in 1..=11u32 {
        let p = digits[(11 - n) as usize] as u32;
        let q = if n <= 6 { n + 1 } else { n - 5 };
        sum += p * q;
    }
    let rem = sum % 11;
    let check = if rem <= 1 { 0 } else { 11 - rem };
    check == digits[11] as u32
}

/// Detect an IBAN (International Bank Account Number) *value* anywhere in the
/// text (ADR-240). Distinct from the `"iban"` keyword (which only fires when the
/// literal word appears): this catches a bare account number like
/// `DE89370400440532013000` that carries no keyword, mirroring how bare
/// Luhn-valid credit cards are caught without the words "credit card". Any hit
/// → sensitive (kept local) and → masked as `<IBAN_n>` before any cloud call.
pub fn contains_iban(text: &str) -> bool {
    !iban_spans(text).is_empty()
}

/// Byte ranges of checksum-valid IBANs in `text`. Detection is limited to the
/// **compact** form (no internal spaces): 2 letters (country) + 2 check digits +
/// 11–30 alphanumeric BBAN, 15–34 chars total, validated by the ISO 7064
/// MOD-97-10 checksum so a random alphanumeric run passes with probability
/// ~1/97. Run as a pre-pass *before* credit-card masking (ADR-196) because a
/// short all-digit IBAN (e.g. a 15-char Norwegian IBAN = NO + 13 digits) would
/// otherwise fall inside the 13–19-digit card window; masking the whole IBAN
/// (including its `NO` prefix) first removes those digits from the card scan.
/// The grouped print form (`DE89 3704 …`) is intentionally out of scope here:
/// gluing a trailing word onto a space-separated run is hard to bound without a
/// per-country length table, and the compact form is the safe, unambiguous
/// subset. Shared by `contains_iban` (any hit → sensitive) and the pseudonymizer
/// so IBAN detection has a single source of truth.
pub fn iban_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Anchor on <alpha><alpha><digit><digit> at an alphanumeric boundary so a
        // longer identifier ending in that shape is not mistaken for an IBAN start.
        let boundary = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        if boundary
            && i + 4 <= bytes.len()
            && bytes[i].is_ascii_alphabetic()
            && bytes[i + 1].is_ascii_alphabetic()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            // Consume the maximal alphanumeric run (compact form; a space or any
            // punctuation ends it, so a trailing word is never glued on).
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let count = j - i;
            if (15..=34).contains(&count) {
                let upper: Vec<u8> = bytes[i..j].iter().map(|b| b.to_ascii_uppercase()).collect();
                if iban_check_valid(&upper) {
                    spans.push((i, j));
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    spans
}

/// ISO 7064 MOD-97-10 validation of an uppercase ASCII-alphanumeric IBAN
/// (country[2] + check[2] + BBAN). Move the first four characters to the end,
/// map each letter to two digits (A=10 … Z=35), and the resulting integer is
/// valid iff it is ≡ 1 (mod 97). The integer is folded digit-by-digit so no
/// big-integer type is needed (std-only).
fn iban_check_valid(chars: &[u8]) -> bool {
    if !(15..=34).contains(&chars.len()) {
        return false;
    }
    // Rearranged order: BBAN (chars[4..]) then country+check (chars[0..4]).
    let mut rem: u32 = 0;
    let mut fold = |c: u8| -> bool {
        if c.is_ascii_digit() {
            rem = (rem * 10 + (c - b'0') as u32) % 97;
            true
        } else if c.is_ascii_uppercase() {
            let v = (c - b'A') as u32 + 10; // 10..=35, always two digits
            rem = (rem * 10 + v / 10) % 97;
            rem = (rem * 10 + v % 10) % 97;
            true
        } else {
            false
        }
    };
    for &c in chars[4..].iter().chain(chars[0..4].iter()) {
        if !fold(c) {
            return false;
        }
    }
    rem == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plain_text_not_sensitive() {
        let r = classify("what is the capital of France?");
        assert!(!r.is_sensitive(), "categories: {:?}", r.categories);
    }

    #[test]
    fn test_email_detected() {
        let r = classify("email me at alice@example.com please");
        assert!(r.categories.contains(&"email"));
    }

    #[test]
    fn test_email_negative_plain_at() {
        assert!(!looks_like_email("@everyone"));
        assert!(!looks_like_email("a@b"));
    }

    #[test]
    fn test_ipv4_detected() {
        assert!(looks_like_ipv4("192.168.0.1"));
        assert!(classify("server at 10.0.0.5 down")
            .categories
            .contains(&"ip"));
    }

    #[test]
    fn test_ipv4_negative() {
        assert!(!looks_like_ipv4("999.1.1.1"));
        assert!(!looks_like_ipv4("1.2.3"));
    }

    #[test]
    fn test_ipv6_detected() {
        // ADR-148: IPv6 addresses identify a host like IPv4 and must route local.
        assert!(looks_like_ipv6("2001:db8::1"));
        assert!(looks_like_ipv6("fe80::1"));
        assert!(looks_like_ipv6("::1"));
        assert!(looks_like_ipv6("2001:0db8:85a3:0000:0000:8a2e:0370:7334"));
        // Wrapped in URL-authority brackets and trailing punctuation.
        assert!(looks_like_ipv6("[2001:db8::1]"));
        assert!(looks_like_ipv6("2001:db8::1,"));
        // classify() must surface it under the shared "ip" category.
        assert!(classify("server at 2001:db8::1 is down")
            .categories
            .contains(&"ip"));
    }

    #[test]
    fn test_ipv6_negative_no_false_positives() {
        // Times, ranges, and code tokens must not be mistaken for IPv6.
        assert!(!looks_like_ipv6("10:30"));
        assert!(!looks_like_ipv6("10:30:45")); // a clock time, not an address
        assert!(!looks_like_ipv6("a:b"));
        assert!(!looks_like_ipv6("std::vector"));
        assert!(!looks_like_ipv6("https://example.com"));
        assert!(!looks_like_ipv6("not-an-address"));
        // A plain English sentence stays non-sensitive.
        assert!(!classify("the meeting is at 10:30 today").is_sensitive());
    }

    #[test]
    fn test_credit_card_luhn() {
        // Valid Luhn test number.
        assert!(contains_credit_card("card 4111 1111 1111 1111 expires"));
        // Invalid Luhn.
        assert!(!contains_credit_card("number 1234 5678 9012 3456"));
    }

    #[test]
    fn test_credit_card_short_digits_ignored() {
        assert!(!contains_credit_card("order 12345 shipped"));
    }

    // --- IBAN value detection (ADR-240) ---

    #[test]
    fn test_iban_check_valid_mod97() {
        // Published valid example IBANs (compact, uppercase).
        assert!(iban_check_valid(b"DE89370400440532013000"));
        assert!(iban_check_valid(b"GB82WEST12345698765432"));
        assert!(iban_check_valid(b"NO9386011117947")); // 15 chars, all-digit BBAN
        // Flip one digit → checksum fails.
        assert!(!iban_check_valid(b"DE89370400440532013001"));
        // Too short / too long → rejected outright.
        assert!(!iban_check_valid(b"DE8937"));
    }

    #[test]
    fn test_iban_detected_without_keyword() {
        // A bare IBAN with no "iban" keyword must still be caught (→ sensitive).
        assert!(contains_iban("please wire to DE89370400440532013000 today"));
        assert!(contains_iban("GB82WEST12345698765432"));
        // Lowercase country code is accepted (normalised to uppercase for the check).
        assert!(contains_iban("acct gb82west12345698765432 ok"));
    }

    #[test]
    fn test_iban_rejects_non_iban_runs() {
        // A random alphanumeric run of IBAN-ish length must not validate.
        assert!(!contains_iban("token AB12CDEF34567890QRSTUVWX is not a bank code"));
        // A plain long digit run (no 2-letter country prefix) is not an IBAN.
        assert!(!contains_iban("id 370400440532013000 here"));
    }

    #[test]
    fn test_iban_span_excludes_trailing_word() {
        // The compact scan ends at the first non-alphanumeric byte, so a following
        // word is never glued onto the span.
        let spans = iban_spans("to DE89370400440532013000 now");
        assert_eq!(spans.len(), 1);
        let (s, e) = spans[0];
        assert_eq!(&"to DE89370400440532013000 now"[s..e], "DE89370400440532013000");
    }

    // --- My Number (マイナンバー, ADR-212) ---

    #[test]
    fn test_my_number_check_digit_valid() {
        // 123456789018 is a structurally-valid My Number (check digit 8).
        assert!(my_number_check_valid(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 8]));
        // Flip the check digit → invalid.
        assert!(!my_number_check_valid(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 1, 7
        ]));
        // Wrong length → invalid.
        assert!(!my_number_check_valid(&[1, 2, 3]));
    }

    #[test]
    fn test_my_number_detected_forms() {
        // Bare, space-separated, and hyphen-separated forms all detect.
        assert!(contains_my_number("マイナンバーは 123456789018 です"));
        assert!(contains_my_number("番号: 1234 5678 9018"));
        assert!(contains_my_number("number 1234-5678-9018"));
        // Classified into the my_number category.
        assert!(classify("私のマイナンバーは123456789018です")
            .categories
            .contains(&"my_number"));
    }

    #[test]
    fn test_my_number_invalid_checkdigit_not_detected() {
        // A 12-digit run with a wrong check digit must not be flagged.
        assert!(!contains_my_number("id 123456789017 ok"));
    }

    #[test]
    fn test_my_number_wrong_length_not_detected() {
        // 11 digits (too short) and 13 digits (too long) must not match.
        assert!(!contains_my_number("number 12345678901 here"));
        assert!(!contains_my_number("number 1234567890123 here"));
    }

    #[test]
    fn test_my_number_span_position() {
        // The span covers exactly the 12 digits (with separators), excluding
        // trailing separators and surrounding text.
        let text = "no: 1234 5678 9018.";
        let spans = my_number_spans(text);
        assert_eq!(spans.len(), 1, "exactly one span: {spans:?}");
        let (s, e) = spans[0];
        assert_eq!(&text[s..e], "1234 5678 9018");
    }

    // --- Full-width digit + separator normalization (全角, ADR-213/214) ---

    #[test]
    fn test_normalize_for_detection_digits() {
        assert_eq!(
            normalize_for_detection("１２３４５６７８９０"),
            "1234567890"
        );
        // Non-digit characters (kanji, ASCII letters) are unchanged.
        assert_eq!(normalize_for_detection("番号abc１２３"), "番号abc123");
        // Already-ASCII text is unchanged.
        assert_eq!(normalize_for_detection("hello 42"), "hello 42");
    }

    #[test]
    fn test_fullwidth_my_number_detected() {
        // ADR-213: a My Number typed in full-width digits must still be
        // classified sensitive (would otherwise bypass is_ascii_digit checks).
        assert!(classify("マイナンバーは１２３４５６７８９０１８です")
            .categories
            .contains(&"my_number"));
    }

    #[test]
    fn test_fullwidth_credit_card_detected() {
        // A full-width credit card number must be classified sensitive.
        assert!(classify("カード番号４１１１１１１１１１１１１１１１")
            .categories
            .contains(&"credit_card"));
    }

    #[test]
    fn test_fullwidth_ipv4_digits_detected() {
        // Full-width digits with ASCII dot separators normalize and detect.
        assert!(classify("サーバー １９２.１６８.１.１ に接続")
            .categories
            .contains(&"ip"));
    }

    #[test]
    fn test_normalize_fullwidth_separators() {
        // ADR-214: full-width dot, hyphen, dash variants, and ideographic space.
        assert_eq!(
            normalize_for_detection("１９２．１６８．１．１"),
            "192.168.1.1"
        );
        assert_eq!(normalize_for_detection("４１１１－１１１１"), "4111-1111");
        // Unicode dash family (en/em/horizontal bar) → ASCII hyphen.
        assert_eq!(normalize_for_detection("12–34—56―78"), "12-34-56-78");
        // Ideographic space → ASCII space.
        assert_eq!(normalize_for_detection("a　b"), "a b");
    }

    #[test]
    fn test_fullwidth_ipv4_with_fullwidth_dot_detected() {
        // ADR-214: full-width digits AND full-width dot (．) must now detect.
        assert!(classify("サーバー １９２．１６８．１．１ に接続")
            .categories
            .contains(&"ip"));
    }

    #[test]
    fn test_fullwidth_credit_card_with_fullwidth_hyphen_detected() {
        // ADR-214: a full-width card with full-width hyphen separators detects.
        assert!(
            classify("カード ４１１１－１１１１－１１１１－１１１１ です")
                .categories
                .contains(&"credit_card")
        );
    }

    #[test]
    fn test_fullwidth_my_number_with_fullwidth_space_detected() {
        // ADR-214: a full-width My Number grouped with ideographic spaces detects.
        assert!(classify("マイナンバー　１２３４　５６７８　９０１８")
            .categories
            .contains(&"my_number"));
    }

    #[test]
    fn test_phone_detected() {
        assert!(looks_like_phone("+1-415-555-0100"));
        assert!(!looks_like_phone("12345"));
        assert!(!looks_like_phone("+12"));
    }

    #[test]
    fn test_intl_phone_with_spaces_detected() {
        // ADR-207: a space-separated international number spans multiple
        // whitespace tokens, so split_whitespace + looks_like_phone misses it.
        // The whitespace-agnostic phone_spans scan must flag it as sensitive.
        let spaced = "call me at +1 555 123 4567 tomorrow";
        // Regression guard: the per-token path alone does NOT catch it…
        assert!(
            !spaced.split_whitespace().any(looks_like_phone),
            "per-token path is expected to miss the spaced form (that is the bug)"
        );
        // …but classify() now does, via contains_intl_phone.
        assert!(
            classify(spaced).categories.contains(&"phone"),
            "spaced international phone must be classified sensitive"
        );
        assert!(contains_intl_phone(spaced));
        // The span starts at '+' and ends after the last digit.
        let spans = phone_spans(spaced);
        assert_eq!(spans.len(), 1, "exactly one phone span: {spans:?}");
        let (s, e) = spans[0];
        assert_eq!(&spaced[s..e], "+1 555 123 4567");
    }

    #[test]
    fn test_intl_phone_span_excludes_trailing_separator() {
        // A trailing space/paren after the last digit must not be in the span.
        let spans = phone_spans("ph: +44 20 7946 0958 .");
        assert_eq!(spans.len(), 1);
        let (s, e) = spans[0];
        assert_eq!(&"ph: +44 20 7946 0958 ."[s..e], "+44 20 7946 0958");
    }

    #[test]
    fn test_intl_phone_negatives() {
        // Too few digits after '+' (the existing 8..=15 window), and a bare '+'.
        assert!(phone_spans("version +2 release").is_empty());
        assert!(phone_spans("a + b = c").is_empty());
        // 16+ digits exceeds the window (consistent with looks_like_phone).
        assert!(phone_spans("+1234567890123456").is_empty());
    }

    #[test]
    fn test_phone_japanese_domestic() {
        // Mobile (with and without hyphens) and hyphenated landline.
        assert!(looks_like_phone("090-1234-5678"));
        assert!(looks_like_phone("09012345678"));
        assert!(looks_like_phone("080-1111-2222"));
        assert!(looks_like_phone("03-1234-5678"));
        assert!(classify("連絡先は 090-1234-5678 です")
            .categories
            .contains(&"phone"));
        // Negatives: bare non-phone digit runs must not trip.
        assert!(!looks_like_phone("12345678901")); // no leading 0
        assert!(!looks_like_phone("0312345678")); // 10 digits, no hyphen -> ambiguous
        assert!(!looks_like_phone("2026"));
    }

    #[test]
    fn test_jwt_detected() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.dBjftJeZ4CVPmB92K27uhbUJU1p1r";
        assert!(looks_like_jwt(jwt));
        assert!(classify(&format!("token: {jwt}"))
            .categories
            .contains(&"jwt"));
        assert!(!looks_like_jwt("eyJ-not-a-jwt"));
        assert!(!looks_like_jwt("hello.world.foo"));
    }

    #[test]
    fn test_jwt_with_surrounding_punctuation() {
        // ADR-100: a JWT quoted or parenthesised in prose must still be detected
        // (shared trim helper with looks_like_api_key).
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.dBjftJeZ4CVPmB92K27uhbUJU1p1r";
        assert!(looks_like_jwt(&format!("\"{jwt}\"")));
        assert!(looks_like_jwt(&format!("({jwt})")));
        assert!(looks_like_jwt(&format!("`{jwt}`")));
        assert!(classify(&format!("my token is ({jwt})"))
            .categories
            .contains(&"jwt"));
    }

    #[test]
    fn test_embedded_api_key_no_whitespace() {
        // ADR-101: a credential immediately adjacent to other JSON content
        // (no surrounding spaces) must be caught by contains_embedded_api_key.
        assert!(contains_embedded_api_key(
            r#"{"authorization":"sk-abcdefghijklmnop1234"}"#
        ));
        assert!(contains_embedded_api_key(
            r#"token=ghp_abcdefghijklmnopqr123456789012"#
        ));
        // classify() must propagate the detection.
        assert!(classify(r#"{"key":"sk-abcdefghijklmnop1234"}"#)
            .categories
            .contains(&"api_key"));
        // Partial-word false-positive guard: "skiing" must NOT match "sk-".
        assert!(!contains_embedded_api_key("skiing is fun today"));
        // Too-short suffix must not match.
        assert!(!contains_embedded_api_key("prefix:sk-short"));
        // Alphanumeric prefix must not match (e.g. a variable named "mysk-...").
        assert!(!contains_embedded_api_key("mysk-abcdefghijklmnop1234"));
    }

    #[test]
    fn test_api_key_prefix() {
        assert!(looks_like_api_key("sk-abcdefghijklmnop1234"));
        assert!(looks_like_api_key("AKIAABCDEFGH12345678"));
        assert!(looks_like_api_key("glpat-abcdefghijklmnop1234"));
        assert!(!looks_like_api_key("sk-short"));
        assert!(!looks_like_api_key("skiing"));
    }

    #[test]
    fn test_api_key_github_and_aws_sts_variants() {
        // GitHub server/user/refresh tokens are as sensitive as ghp_.
        assert!(looks_like_api_key("ghu_abcdefghijklmnop1234"));
        assert!(looks_like_api_key("ghs_abcdefghijklmnop1234"));
        assert!(looks_like_api_key("ghr_abcdefghijklmnop1234"));
        // AWS STS temporary credentials start with ASIA (vs long-lived AKIA).
        assert!(looks_like_api_key("ASIAABCDEFGH12345678"));
        // Still require sufficient length (no short false positives).
        assert!(!looks_like_api_key("ghu_short"));
    }

    #[test]
    fn test_api_key_slack_xapp_and_huggingface() {
        // Slack App-Level Token (Socket Mode, xapp- prefix, real format is ~70 chars)
        assert!(looks_like_api_key(
            "xapp-1-A01BCDEF234-5678901234-abcdefgh0123456789abcdef0"
        ));
        // Shorter xapp- tokens below the minimum length threshold must not false-positive
        assert!(!looks_like_api_key("xapp-short"));
        // HuggingFace access tokens: hf_ + 37 chars typical
        assert!(looks_like_api_key("hf_abcdefghijklmnopqrstuvwxyz0123456"));
        assert!(!looks_like_api_key("hf_tiny"));
        // classify() propagates both
        assert!(
            classify("use this token: xapp-1-A01BCDEF234-5678901234-abcdef01234567890")
                .categories
                .contains(&"api_key")
        );
        assert!(
            classify("HUGGINGFACE_TOKEN=hf_abcdefghijklmnopqrstuvwxyz0123456")
                .categories
                .contains(&"env_secret")
        );
    }

    #[test]
    fn test_api_key_with_surrounding_punctuation() {
        // ADR-099: a key quoted in JSON or wrapped in prose punctuation must
        // still be detected. Previously a leading quote/paren broke starts_with,
        // letting a quoted credential leak to the cloud undetected.
        assert!(looks_like_api_key("\"sk-abcdefghijklmnop1234\""));
        assert!(looks_like_api_key("(sk-abcdefghijklmnop1234)"));
        assert!(looks_like_api_key("`ghp_abcdefghijklmnop1234`"));
        assert!(looks_like_api_key("<glpat-abcdefghijklmnop1234>"));
        // classify() now flags a key quoted in prose (a whitespace-delimited
        // token whose edges are punctuation).
        assert!(classify(r#"my key is "sk-abcdefghijklmnop1234""#)
            .categories
            .contains(&"api_key"));
        // A `-`/`_` inside a valid prefix is preserved (not trimmed as punctuation).
        assert!(looks_like_api_key("ghp_abcdefghijklmnop1234"));
        // No false positive on ordinary punctuated words.
        assert!(!looks_like_api_key("(hello)"));
    }

    #[test]
    fn test_keyword_detected_en_and_ja() {
        assert!(classify("my password is hunter2")
            .categories
            .contains(&"keyword"));
        assert!(classify("私のパスワードを教える")
            .categories
            .contains(&"keyword"));
    }

    #[test]
    fn test_multiple_categories() {
        let r = classify("send to bob@corp.com, my api key sk-aaaaaaaaaaaaaaaa");
        assert!(r.categories.contains(&"email"));
        assert!(r.categories.contains(&"keyword"));
        assert!(r.categories.contains(&"api_key"));
    }

    #[test]
    fn test_report_no_raw_values_stored() {
        // The report exposes only stable labels, never matched substrings.
        let r = classify("alice@example.com");
        for c in &r.categories {
            assert!(!c.contains('@'), "category must not contain raw PII");
        }
    }

    // --- PEM private key ---

    #[test]
    fn test_pem_rsa_key_detected() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEo...\n-----END RSA PRIVATE KEY-----";
        assert!(contains_pem_key(text));
        assert!(classify(text).categories.contains(&"pem_key"));
    }

    #[test]
    fn test_pem_ec_key_detected() {
        assert!(contains_pem_key(
            "-----BEGIN EC PRIVATE KEY-----\ndata\n-----END EC PRIVATE KEY-----"
        ));
    }

    #[test]
    fn test_pem_openssh_key_detected() {
        assert!(contains_pem_key(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC...\n-----END OPENSSH PRIVATE KEY-----"
        ));
    }

    #[test]
    fn test_pem_public_key_not_flagged() {
        // Public keys are not sensitive in the same way
        assert!(!contains_pem_key(
            "-----BEGIN PUBLIC KEY-----\ndata\n-----END PUBLIC KEY-----"
        ));
    }

    #[test]
    fn test_pem_certificate_not_flagged() {
        assert!(!contains_pem_key(
            "-----BEGIN CERTIFICATE-----\ndata\n-----END CERTIFICATE-----"
        ));
    }

    // --- URL credential ---

    #[test]
    fn test_url_credential_postgres_detected() {
        assert!(contains_url_credential(
            "postgresql://admin:s3cr3t@db.example.com/mydb"
        ));
        assert!(
            classify("connect to postgresql://admin:s3cr3t@db.example.com/mydb")
                .categories
                .contains(&"url_credential")
        );
    }

    #[test]
    fn test_url_credential_ftp_detected() {
        assert!(contains_url_credential("ftp://user:pass@files.example.com"));
    }

    #[test]
    fn test_url_host_port_not_flagged() {
        // host:port is not a credential
        assert!(!contains_url_credential("http://localhost:8080/path"));
        assert!(!contains_url_credential("https://api.example.com/v1"));
    }

    #[test]
    fn test_url_host_port_with_at_not_flagged() {
        // ADR-211: "host.domain.com:8080@attacker.com" could be misparsed as
        // user:password if we don't exclude numeric-only password fields. The
        // ":8080" part (pure digits) is a port, not a password. Must not be
        // flagged as a credential.
        assert!(!contains_url_credential(
            "http://db.example.com:5432@attacker.com/"
        ));
        assert!(!contains_url_credential("http://api.host.com:443@bad.net"));
    }

    #[test]
    fn test_url_credential_empty_password_not_flagged() {
        assert!(!contains_url_credential("ftp://user:@host.com"));
    }

    // --- Environment variable secret ---

    #[test]
    fn test_env_secret_plain_assignment() {
        assert!(contains_env_secret("SECRET_KEY=abc123def456"));
        assert!(classify("SECRET_KEY=abc123def456")
            .categories
            .contains(&"env_secret"));
    }

    #[test]
    fn test_env_secret_export_form() {
        assert!(contains_env_secret("export DATABASE_PASSWORD='hunter2!'"));
    }

    #[test]
    fn test_env_secret_stripe_key() {
        // Constructed at runtime to avoid secret-scanning rules on inert test strings.
        let line = format!("STRIPE_API_KEY={}abcdefghij12345", "sk_live_");
        assert!(contains_env_secret(&line));
    }

    #[test]
    fn test_env_secret_aws() {
        let line = ["AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI", "/K7MDENG"].concat();
        assert!(contains_env_secret(&line));
    }

    #[test]
    fn test_env_secret_trivial_value_not_flagged() {
        assert!(!contains_env_secret("SECRET_KEY="));
        assert!(!contains_env_secret("AUTH_TOKEN=null"));
        assert!(!contains_env_secret("AUTH_TOKEN=true"));
    }

    #[test]
    fn test_env_non_secret_variable_not_flagged() {
        assert!(!contains_env_secret("PORT=3000"));
        assert!(!contains_env_secret("DEBUG=true"));
        assert!(!contains_env_secret("DATABASE_HOST=localhost"));
        assert!(!contains_env_secret("TIMEOUT=30"));
    }

    // --- Expanded key prefixes ---
    // Note: test values are constructed at runtime to avoid triggering
    // repository secret-scanning rules on inert test strings.

    #[test]
    fn test_stripe_live_key_detected() {
        // Assemble at runtime: prefix + filler so scanner sees no literal key.
        let key = ["sk_live_", "51AbcDEFghiJKLmnop", "1234567890"].concat();
        assert!(looks_like_api_key(&key));
        assert!(classify(&format!("my key is {key}"))
            .categories
            .contains(&"api_key"));
    }

    #[test]
    fn test_sendgrid_key_detected() {
        let key = ["SG.", "abcdefghijklmnopqrstuvwxyz", "01234567890ABCDEF"].concat();
        assert!(looks_like_api_key(&key));
    }

    #[test]
    fn test_google_oauth_token_detected() {
        let tok = ["ya29.", "abcdefghijklmnopqrstuvwxyz", "01234"].concat();
        assert!(looks_like_api_key(&tok));
    }

    #[test]
    fn test_npm_token_detected() {
        let tok = ["npm_", "abcdefghijklmnopqrstuvwxyz", "01234567890"].concat();
        assert!(looks_like_api_key(&tok));
    }

    // --- Expanded Japanese keywords ---

    #[test]
    fn test_japanese_dob_detected() {
        assert!(classify("生年月日を教えてください")
            .categories
            .contains(&"keyword"));
    }

    #[test]
    fn test_japanese_bank_account_detected() {
        assert!(classify("口座番号 1234567").categories.contains(&"keyword"));
    }

    #[test]
    fn test_japanese_drivers_license_detected() {
        assert!(classify("運転免許証の番号は")
            .categories
            .contains(&"keyword"));
    }

    #[test]
    fn test_japanese_my_number_detected() {
        assert!(classify("個人番号カードを確認")
            .categories
            .contains(&"keyword"));
    }

    // --- New keyword combinations ---

    #[test]
    fn test_bearer_token_keyword() {
        assert!(classify("Authorization: bearer token eyJ...")
            .categories
            .contains(&"keyword"));
    }

    #[test]
    fn test_iban_keyword() {
        assert!(classify("my IBAN is DE89370400440532013000")
            .categories
            .contains(&"keyword"));
    }

    #[test]
    fn test_date_of_birth_keyword() {
        assert!(classify("my date of birth is 1990-01-15")
            .categories
            .contains(&"keyword"));
    }
}

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

/// Classify a prompt's sensitivity. Returns category labels only.
pub fn classify(text: &str) -> SensitivityReport {
    let mut categories: Vec<&'static str> = Vec::new();
    let lower = text.to_lowercase();

    if KEYWORDS.iter().any(|k| lower.contains(k)) {
        categories.push("keyword");
    }
    if text.split_whitespace().any(looks_like_email) {
        categories.push("email");
    }
    if text.split_whitespace().any(looks_like_ipv4) {
        categories.push("ip");
    }
    if contains_credit_card(text) {
        categories.push("credit_card");
    }
    if text.split_whitespace().any(looks_like_phone) {
        categories.push("phone");
    }
    if text.split_whitespace().any(looks_like_api_key) {
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

/// Detect URL-embedded credentials: `scheme://user:password@host`.
/// Requires a non-empty password part after the colon to avoid matching
/// `http://host:8080/path` (port-only, no user info).
pub fn contains_url_credential(text: &str) -> bool {
    let mut search = text;
    while let Some(pos) = search.find("://") {
        let after = &search[pos + 3..];
        // Bound the search: credentials appear before the first slash in the authority
        let authority = match after.find('/') {
            Some(s) => &after[..s],
            None => after,
        };
        if let Some(at_pos) = authority.find('@') {
            let user_info = &authority[..at_pos];
            if let Some(colon) = user_info.find(':') {
                // Non-empty password part after the colon
                if !user_info[colon + 1..].is_empty() {
                    return true;
                }
            }
        }
        search = &search[pos + 3..];
    }
    false
}

/// Detect environment-variable secret assignments, e.g.:
/// `SECRET_KEY=abc123`, `export DB_PASSWORD='hunter2'`, `API_TOKEN="xyz"`.
/// Matches lines where the variable name contains a secret-sounding substring
/// and the value is non-trivial (length ≥ 4, not a bare boolean/null/empty).
pub fn contains_env_secret(text: &str) -> bool {
    for line in text.lines() {
        let trimmed = line.trim();
        let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
        let Some((key, val)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim();
        // Variable names are ASCII alphanumeric + underscores only
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let key_lower = key.to_lowercase();
        if !ENV_SECRET_SUBSTRINGS.iter().any(|s| key_lower.contains(s)) {
            continue;
        }
        // Strip surrounding quotes and whitespace from the value
        let val = val.trim().trim_matches('"').trim_matches('\'').trim();
        let trivial =
            val.is_empty() || matches!(val, "true" | "false" | "0" | "1" | "none" | "null" | "''");
        if !trivial && val.len() >= 4 {
            return true;
        }
    }
    false
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

/// Heuristic credential token: a known prefix and enough length. Surrounding
/// punctuation is stripped first so a key quoted in JSON or prose
/// (`"sk-…"`, `(sk-…)`, `sk-…,`) is still detected — consistent with
/// `looks_like_jwt`. Stripped characters are the JSON/prose delimiters that
/// never appear inside a real token; `-` and `_` are preserved because they
/// are valid in many prefixes (`ghp_`, `glpat-`, `xoxb-`).
pub fn looks_like_api_key(token: &str) -> bool {
    let t = token.trim_matches(|c: char| {
        matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '<' | '>')
    });
    KEY_PREFIXES
        .iter()
        .any(|p| t.starts_with(p) && t.len() >= p.len() + 12)
}

/// Heuristic JWT / bearer token: header.payload.signature where the header is
/// base64url of `{"...` (begins with `eyJ`). Near-zero false positives.
pub fn looks_like_jwt(token: &str) -> bool {
    let t = token.trim_matches(|c: char| c == '"' || c == ',' || c == ';');
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
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            // Consume a maximal digit/sep run.
            let mut digits: Vec<u8> = Vec::new();
            let mut j = i;
            while j < bytes.len() {
                let cj = bytes[j];
                if cj.is_ascii_digit() {
                    digits.push(cj - b'0');
                    j += 1;
                } else if cj == b' ' || cj == b'-' {
                    j += 1;
                } else {
                    break;
                }
            }
            if (13..=19).contains(&digits.len()) && luhn_valid(&digits) {
                return true;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
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

    #[test]
    fn test_phone_detected() {
        assert!(looks_like_phone("+1-415-555-0100"));
        assert!(!looks_like_phone("12345"));
        assert!(!looks_like_phone("+12"));
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
        assert!(looks_like_api_key("xapp-1-A01BCDEF234-5678901234-abcdefgh0123456789abcdef0"));
        // Shorter xapp- tokens below the minimum length threshold must not false-positive
        assert!(!looks_like_api_key("xapp-short"));
        // HuggingFace access tokens: hf_ + 37 chars typical
        assert!(looks_like_api_key("hf_abcdefghijklmnopqrstuvwxyz0123456"));
        assert!(!looks_like_api_key("hf_tiny"));
        // classify() propagates both
        assert!(classify("use this token: xapp-1-A01BCDEF234-5678901234-abcdef01234567890")
            .categories
            .contains(&"api_key"));
        assert!(classify("HUGGINGFACE_TOKEN=hf_abcdefghijklmnopqrstuvwxyz0123456")
            .categories
            .contains(&"env_secret"));
    }

    #[test]
    fn test_api_key_with_surrounding_punctuation(){
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

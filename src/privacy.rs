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
    "password",
    "passwd",
    "api key",
    "api_key",
    "apikey",
    "secret key",
    "client_secret",
    "access_token",
    "private key",
    "credit card",
    "social security",
    "ssn",
    "passport",
    "パスワード",
    "秘密鍵",
    "マイナンバー",
    "クレジットカード",
];

/// Token prefixes that strongly indicate a leaked credential.
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "glpat-",
    "AKIA",
    "AIza",
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

    SensitivityReport { categories }
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

/// Heuristic credential token: a known prefix and enough length.
pub fn looks_like_api_key(token: &str) -> bool {
    KEY_PREFIXES
        .iter()
        .any(|p| token.starts_with(p) && token.len() >= p.len() + 12)
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
}

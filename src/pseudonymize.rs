//! Reversible PII pseudonymization for cloud requests (IMP-19).
//!
//! When `PASTURE_PSEUDONYMIZE=1`, each cloud-bound message has its detected PII
//! replaced with stable opaque tokens (`<EMAIL_1>`, `<IP_1>`, etc.) before the
//! request leaves the machine, and the cloud response has the tokens replaced
//! back with the original values. This lets users get cloud model quality on
//! prompts that contain PII while keeping actual values off the provider's
//! servers.
//!
//! **Security caveat:** pseudonymization catches the PII patterns Pasture
//! already detects (email, IPv4/IPv6, phone — including `+`-prefixed
//! international numbers that span whitespace, ADR-207 — API-key prefixes,
//! Luhn-valid credit-card numbers — ADR-196, JWTs — ADR-197, URL-embedded
//! credentials and env-var secret values — ADR-203, complete PEM private-key
//! blocks — ADR-204, Japanese My Numbers / マイナンバー with a valid check
//! digit — ADR-212). It is NOT a guarantee that no other sensitive information
//! is sent — use `local_only` mode for hard guarantees.
//!
//! Privacy invariant (I5): the replacement mapping lives only in memory for
//! the duration of the request and is never logged.

use crate::backend::Message;
use crate::privacy;

/// Map from opaque token to original PII value, used to restore responses.
pub type Mapping = Vec<(String, String)>; // (original, token)

/// Replace PII in a slice of messages with opaque tokens.
/// Returns the modified messages and a mapping for `restore()`.
/// Identical values get the same token (stable across messages in one request).
pub fn pseudonymize_messages(messages: &[Message]) -> (Vec<Message>, Mapping) {
    let mut ctx = Ctx::new();
    let result = messages
        .iter()
        .map(|m| Message {
            role: m.role.clone(),
            content: replace_in_text(&m.content, &mut ctx),
            tool_call_id: m.tool_call_id.clone(),
            // ADR-188: tool_calls_json may carry PII in model-generated arguments
            // (e.g. {"email":"alice@example.com"} passed to a send_email tool).
            // The whitespace tokenizer used by replace_in_text cannot find values
            // inside compact JSON strings, so we use a JSON-string-aware walker
            // that decodes each "…" value, pseudonymizes the decoded text (or
            // recursively descends into nested JSON), and re-encodes. The
            // tool_call_id is server-generated and never carries user PII.
            tool_calls_json: m
                .tool_calls_json
                .as_deref()
                .map(|tc| replace_in_json_strings(tc, &mut ctx)),
        })
        .collect();
    (result, ctx.mapping)
}

/// Restore opaque tokens in a response string to the original PII values.
/// Tokens that don't appear in the mapping are left as-is.
///
/// Tokens are replaced longest-first (ADR-205) to prevent the prefix-collision
/// where `<EMAIL_1>` (9 chars) is a prefix of `<EMAIL_10>` (10 chars): if we
/// replaced shorter tokens first, every `<EMAIL_10>` in the text would become
/// `alice@example.com0>` (corrupted). Sorting by descending token length makes
/// `<EMAIL_10>` match before `<EMAIL_1>` so both restore cleanly.
pub fn restore(text: &str, mapping: &[(String, String)]) -> String {
    let mut out = text.to_string();
    let mut sorted: Vec<&(String, String)> = mapping.iter().collect();
    sorted.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    for (original, token) in sorted {
        out = out.replace(token.as_str(), original.as_str());
    }
    out
}

/// Incrementally restores pseudonymized tokens in a streamed (SSE) response.
///
/// Tokens (`<EMAIL_1>`) can be split across two deltas — naively restoring each
/// delta in isolation would miss the boundary case. The restorer buffers a
/// trailing fragment that could be the start of a token (an unterminated `<…`)
/// and only emits it once the token completes or the stream ends, guaranteeing
/// no partial token is ever flushed un-restored.
pub struct StreamRestorer {
    mapping: Mapping,
    pending: String,
    /// Longest token in the mapping (e.g. `<EMAIL_1>` = 9 bytes). A dangling
    /// `<` is held back only until the fragment reaches this length — past it,
    /// no real token can still be completing, so the buffer is bounded (ADR-168).
    max_token_len: usize,
}

impl StreamRestorer {
    pub fn new(mapping: Mapping) -> Self {
        let max_token_len = mapping.iter().map(|(_, tok)| tok.len()).max().unwrap_or(0);
        Self {
            mapping,
            pending: String::new(),
            max_token_len,
        }
    }

    /// Feed the next streamed delta; returns the portion that is now safe to
    /// emit, with any complete tokens restored to their original values.
    pub fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        // Hold back from the last '<' that has no closing '>' yet — it could be
        // the prefix of a token still arriving (or a literal '<' that simply has
        // no '>' yet; either way it flushes at finish()).
        let split = match self.pending.rfind('<') {
            Some(i) if !self.pending[i..].contains('>') => {
                // A real token (`<EMAIL_1>`) closes its '>' within `max_token_len`
                // bytes. Once the dangling fragment reaches that length without a
                // '>', it cannot be a token, so flushing it is both correct and
                // necessary to bound the buffer — otherwise an unterminated '<' in
                // the cloud stream (e.g. `a < b` in code, or an adversarial run with
                // no '>') makes `pending` grow without limit and stalls streaming
                // until finish(). With an empty mapping (max_token_len == 0) nothing
                // is ever held, so the restorer is pure pass-through.
                if self.pending.len() - i >= self.max_token_len {
                    self.pending.len()
                } else {
                    i
                }
            }
            _ => self.pending.len(),
        };
        let flushable = restore(&self.pending[..split], &self.mapping);
        self.pending.drain(..split);
        flushable
    }

    /// Flush any remaining buffered text once the stream has ended.
    pub fn finish(&mut self) -> String {
        let out = restore(&self.pending, &self.mapping);
        self.pending.clear();
        out
    }
}

struct Ctx {
    mapping: Mapping,
    email_n: usize,
    ip_n: usize,
    phone_n: usize,
    key_n: usize,
    card_n: usize,
    jwt_n: usize,
    url_n: usize,      // ADR-203: URL-embedded credentials
    env_n: usize,      // ADR-203: env-var secret values
    pem_n: usize,      // ADR-204: PEM private-key blocks
    mynumber_n: usize, // ADR-212: Japanese My Number (マイナンバー)
    iban_n: usize,     // ADR-240: IBAN bank account numbers
}

impl Ctx {
    fn new() -> Self {
        Self {
            mapping: Vec::new(),
            email_n: 0,
            ip_n: 0,
            phone_n: 0,
            key_n: 0,
            card_n: 0,
            jwt_n: 0,
            url_n: 0,
            env_n: 0,
            pem_n: 0,
            mynumber_n: 0,
            iban_n: 0,
        }
    }

    fn token_for(&mut self, original: &str, category: &str) -> String {
        // Reuse existing token for the same value (stable within one request).
        if let Some((_, tok)) = self.mapping.iter().find(|(v, _)| v == original) {
            return tok.clone();
        }
        let n = match category {
            "EMAIL" => {
                self.email_n += 1;
                self.email_n
            }
            "IP" => {
                self.ip_n += 1;
                self.ip_n
            }
            "PHONE" => {
                self.phone_n += 1;
                self.phone_n
            }
            "CARD" => {
                self.card_n += 1;
                self.card_n
            }
            "JWT" => {
                self.jwt_n += 1;
                self.jwt_n
            }
            "URL" => {
                self.url_n += 1;
                self.url_n
            }
            "ENV" => {
                self.env_n += 1;
                self.env_n
            }
            "PEM" => {
                self.pem_n += 1;
                self.pem_n
            }
            "MYNUMBER" => {
                self.mynumber_n += 1;
                self.mynumber_n
            }
            "IBAN" => {
                self.iban_n += 1;
                self.iban_n
            }
            _ => {
                self.key_n += 1;
                self.key_n
            }
        };
        let tok = format!("<{}_{}>", category, n);
        self.mapping.push((original.to_string(), tok.clone()));
        tok
    }

    fn process_token(&mut self, token: &str) -> Option<String> {
        // Check each PII category in priority order.
        let inner = trim_punct(token);
        if inner.is_empty() {
            return None;
        }
        // trim_matches returns a subslice, so the pointer difference gives the
        // exact byte offset where inner starts within token (ADR-206).  The old
        // formula `token.len() - inner.len()` counted total-stripped chars and
        // wrongly assigned ALL of them to the leading side, making suffix always
        // empty and leaking trailing-punct chars (e.g. "alice@example.com," →
        // prefix="a", suffix="" instead of prefix="", suffix=",").
        let inner_start = inner.as_ptr() as usize - token.as_ptr() as usize;
        let prefix = &token[..inner_start];
        let suffix = &token[inner_start + inner.len()..];

        if privacy::looks_like_email(inner) {
            let tok = self.token_for(inner, "EMAIL");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        if privacy::looks_like_ipv4(inner) {
            let tok = self.token_for(inner, "IP");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        if privacy::looks_like_ipv6(inner) {
            // Same "IP" category as IPv4 — an address is an address (ADR-156).
            // Keeps the pseudonymizer consistent with the sensitivity classifier,
            // which flags IPv6 (ADR-148); otherwise an IPv6 address would reach the
            // cloud raw under allow_sensitive_cloud + pseudonymize.
            let tok = self.token_for(inner, "IP");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        if privacy::looks_like_phone(inner) {
            let tok = self.token_for(inner, "PHONE");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        if privacy::looks_like_api_key(inner) {
            let tok = self.token_for(inner, "KEY");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        if privacy::looks_like_jwt(inner) {
            // A JWT (`eyJ…`) is a bearer secret the classifier flags (ADR-148);
            // mask it so it does not reach the cloud raw under allow_sensitive_cloud
            // + pseudonymize (ADR-197). Single token, so the per-token loop handles
            // it like an API key. Checked after api_key (no prefix overlap: a JWT
            // starts with `eyJ`, not a vendor key prefix).
            let tok = self.token_for(inner, "JWT");
            return Some(format!("{prefix}{tok}{suffix}"));
        }
        None
    }
}

/// Strip leading/trailing punctuation that wraps a token but is not part
/// of the value (e.g. `"user@example.com"` → `user@example.com`).
fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']'
        )
    })
}

/// Walk every JSON string literal in `json`, decode it, pseudonymize the
/// decoded content (recursing into nested JSON-encoded strings such as the
/// tool-call `arguments` field), and re-encode. Non-string characters
/// (braces, colons, numbers, `true`/`false`/`null`) are passed through
/// verbatim. Handles `\"` and `\\` escape sequences; unknown escapes are
/// copied literally.
///
/// This lets the pseudonymizer reach PII inside compact JSON like
/// `{"email":"alice@example.com"}` where the whitespace tokenizer in
/// `replace_in_text` cannot, because there is no whitespace to split on.
fn replace_in_json_strings(json: &str, ctx: &mut Ctx) -> String {
    let chars: Vec<char> = json.chars().collect();
    let len = chars.len();
    let mut out = String::with_capacity(json.len() + 32);
    let mut i = 0;

    while i < len {
        if chars[i] != '"' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Opening quote — scan to the closing unescaped quote, decoding escapes.
        out.push('"');
        i += 1;
        let mut decoded = String::new();
        while i < len {
            match chars[i] {
                '\\' if i + 1 < len => {
                    let esc = chars[i + 1];
                    match esc {
                        '"' => {
                            decoded.push('"');
                            i += 2;
                        }
                        '\\' => {
                            decoded.push('\\');
                            i += 2;
                        }
                        'n' => {
                            decoded.push('\n');
                            i += 2;
                        }
                        't' => {
                            decoded.push('\t');
                            i += 2;
                        }
                        'r' => {
                            decoded.push('\r');
                            i += 2;
                        }
                        _ => {
                            decoded.push('\\');
                            decoded.push(esc);
                            i += 2;
                        }
                    }
                }
                '"' => break,
                c => {
                    decoded.push(c);
                    i += 1;
                }
            }
        }
        // Recurse if the decoded value itself looks like a JSON object/array
        // (e.g. the `arguments` field stores JSON-encoded JSON).  Otherwise
        // use the whitespace tokenizer for plain text values.
        let replaced = if decoded.trim_start().starts_with(['{', '[']) {
            replace_in_json_strings(&decoded, ctx)
        } else {
            replace_in_text(&decoded, ctx)
        };
        out.push_str(&crate::json::escape_string(&replaced));
        if i < len {
            out.push('"'); // closing quote
            i += 1;
        }
    }
    out
}

/// Span-based pre-pass helper: apply `spans` (byte ranges in `text`) replacing
/// each with a token from `ctx.token_for(original, category)`.
fn apply_spans(text: &str, spans: Vec<(usize, usize)>, ctx: &mut Ctx, category: &str) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end) in spans {
        if start >= end || end > text.len() {
            continue;
        }
        out.push_str(&text[last..start]);
        let tok = ctx.token_for(&text[start..end], category);
        out.push_str(&tok);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

/// Mask credit-card numbers with `<CARD_n>` tokens (ADR-196). Cards are handled
/// by a whole-text pre-pass, not the per-token loop, because a card may contain
/// space/hyphen separators (`4111 1111 1111 1111`) and so span multiple
/// whitespace tokens that the tokenizer would never reassemble.
fn mask_credit_cards(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::credit_card_spans(text), ctx, "CARD")
}

/// Mask IBAN bank account numbers with `<IBAN_n>` tokens (ADR-240). Runs before
/// `mask_credit_cards` so a short all-digit IBAN's digit run is claimed here (as
/// part of the whole IBAN, prefix letters included) rather than partially caught
/// by the 13–19-digit card scan. Compact form only; see `privacy::iban_spans`.
fn mask_ibans(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::iban_spans(text), ctx, "IBAN")
}

/// Mask Japanese My Numbers (マイナンバー) with `<MYNUMBER_n>` tokens (ADR-212).
/// A 12-digit My Number may be written with space/hyphen separators
/// (`1234 5678 9018`), spanning multiple whitespace tokens, so it needs a
/// checksum-validated span pre-pass exactly like credit cards.
fn mask_my_numbers(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::my_number_spans(text), ctx, "MYNUMBER")
}

/// Mask international (`+`-prefixed) phone numbers with `<PHONE_n>` tokens
/// (ADR-207). Like credit cards, a number written `+1 555 123 4567` spans
/// several whitespace tokens, so the per-token loop (which calls
/// `looks_like_phone` on each fragment) cannot reassemble it. A whitespace-
/// agnostic span pre-pass closes that gap. Single-token domestic forms
/// (`090-1234-5678`) keep flowing through the per-token loop.
fn mask_phones(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::phone_spans(text), ctx, "PHONE")
}

/// Mask URL-embedded credentials (`user:password` in `scheme://user:pass@host`)
/// with `<URL_n>` tokens (ADR-203). The scheme and host are preserved; only the
/// userinfo part is replaced, so `https://user:hunter2@host` becomes
/// `https://<URL_1>@host`. A pre-pass is necessary because the URL is typically
/// a single whitespace token and the per-token loop cannot split it.
fn mask_url_credentials(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::url_credential_spans(text), ctx, "URL")
}

/// Mask environment-variable secret values (`DB_PASSWORD=hunter2` →
/// `DB_PASSWORD=<ENV_n>`) with `<ENV_n>` tokens (ADR-203). The key name and
/// `=` sign are preserved; only the value is replaced. A line-based pre-pass
/// handles multi-line `.env` blocks without requiring whitespace around `=`.
fn mask_env_secrets(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::env_secret_spans(text), ctx, "ENV")
}

/// Mask complete PEM private-key blocks with `<PEM_n>` tokens (ADR-204).
/// The entire block — header, base64 body, and footer — is replaced so no key
/// material reaches the cloud under `allow_sensitive_cloud` + `pseudonymize`.
/// Only complete, paired blocks (with both a BEGIN header and an END footer) are
/// masked; an unpaired header alone cannot be coherently restored and is
/// flagged by the classifier to stay local before pseudonymization applies.
fn mask_pem_keys(text: &str, ctx: &mut Ctx) -> String {
    apply_spans(text, crate::privacy::pem_key_spans(text), ctx, "PEM")
}

/// Replace PII in `text` token by token, preserving original whitespace.
fn replace_in_text(text: &str, ctx: &mut Ctx) -> String {
    // Pre-passes in order of specificity:
    //  0. IBANs (2 letters + 2 check digits + BBAN, MOD-97, ADR-240) — FIRST, so
    //     a short all-digit IBAN is masked whole before the card scan sees it.
    //  1. Credit-card numbers (13-19 digits, may span whitespace, ADR-196)
    //  2. My Numbers (マイナンバー, exactly 12 digits + check digit, ADR-212)
    //  3. International phone numbers (`+1 555 123 4567`, span whitespace, ADR-207)
    //  4. URL-embedded credentials (ADR-203)
    //  5. Env-var secret values (ADR-203)
    //  6. PEM private-key blocks (multi-line, ADR-204)
    // Each pass chains its output into the next; the per-token loop then handles
    // the remaining single-token PII (email, IP, domestic phone, API key, JWT).
    // IBANs are letter-anchored; cards (13-19 digits) and My Numbers (exactly 12)
    // are length-disjoint and digit-anchored while phones are '+'-anchored, so
    // no pre-pass contends with another for the same bytes.
    let masked = mask_ibans(text, ctx);
    let masked = mask_credit_cards(&masked, ctx);
    let masked = mask_my_numbers(&masked, ctx);
    let masked = mask_phones(&masked, ctx);
    let masked = mask_url_credentials(&masked, ctx);
    let masked = mask_env_secrets(&masked, ctx);
    let masked = mask_pem_keys(&masked, ctx);
    let text = masked.as_str();
    let mut out = String::with_capacity(text.len() + 32);
    let bytes = text.as_bytes();
    let len = text.len();
    let mut pos = 0;

    while pos < len {
        // Copy whitespace verbatim.
        let ws_start = pos;
        while pos < len && is_ws(bytes[pos]) {
            pos += 1;
        }
        out.push_str(&text[ws_start..pos]);
        if pos >= len {
            break;
        }
        // Find the end of the next non-whitespace token.
        let tok_start = pos;
        while pos < len && !is_ws(bytes[pos]) {
            pos += 1;
        }
        let token = &text[tok_start..pos];
        match ctx.process_token(token) {
            Some(replacement) => out.push_str(&replacement),
            None => out.push_str(token),
        }
    }
    out
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(content: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_email_is_pseudonymized() {
        let msgs = vec![msg("contact me at alice@example.com for details")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert!(
            out[0].content.contains("<EMAIL_1>"),
            "email must be replaced: {}",
            out[0].content
        );
        assert!(
            !out[0].content.contains("alice@example.com"),
            "original email must not remain"
        );
        assert!(!mapping.is_empty(), "mapping must be non-empty");
    }

    #[test]
    fn test_ip_is_pseudonymized() {
        let msgs = vec![msg("server IP is 192.168.1.100 please check")];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<IP_1>"), "{}", out[0].content);
        assert!(!out[0].content.contains("192.168.1.100"));
    }

    #[test]
    fn test_ipv6_is_pseudonymized_and_restored() {
        // ADR-156: IPv6 is flagged sensitive by the classifier (ADR-148), so the
        // pseudonymizer must mask it too — otherwise it would reach the cloud raw
        // under allow_sensitive_cloud + pseudonymize. Round-trips back on restore.
        let msgs = vec![msg("connect to 2001:db8::1 then retry")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<IP_1>"), "{}", out[0].content);
        assert!(
            !out[0].content.contains("2001:db8::1"),
            "raw IPv6 must not remain: {}",
            out[0].content
        );
        let restored = restore(&out[0].content, &mapping);
        assert!(
            restored.contains("2001:db8::1"),
            "IPv6 must restore: {restored}"
        );
    }

    #[test]
    fn test_restore_reverses_pseudonymization() {
        let msgs = vec![msg("email alice@example.com and bob@example.com")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        // cloud response echoes back the tokens
        let cloud_resp = out[0].content.clone();
        let restored = restore(&cloud_resp, &mapping);
        assert!(
            restored.contains("alice@example.com"),
            "alice must be restored: {restored}"
        );
        assert!(
            restored.contains("bob@example.com"),
            "bob must be restored: {restored}"
        );
    }

    #[test]
    fn test_same_value_gets_same_token() {
        let msgs = vec![msg("alice@example.com and alice@example.com again")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let count = out[0].content.matches("<EMAIL_1>").count();
        assert_eq!(
            count, 2,
            "same email should use the same token: {}",
            out[0].content
        );
        assert_eq!(
            mapping.len(),
            1,
            "only one mapping entry for the same value"
        );
    }

    #[test]
    fn test_benign_text_unchanged() {
        let msgs = vec![msg("what is the capital of France?")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert_eq!(
            out[0].content, msgs[0].content,
            "benign text must not change"
        );
        assert!(mapping.is_empty(), "no mapping for benign text");
    }

    #[test]
    fn test_whitespace_preserved() {
        let text = "  hello  world  ";
        let msgs = vec![msg(text)];
        let (out, _) = pseudonymize_messages(&msgs);
        assert_eq!(
            out[0].content, text,
            "whitespace must be preserved when no PII"
        );
    }

    #[test]
    fn test_api_key_is_pseudonymized() {
        let msgs = vec![msg("my key is sk-abcdefghijklmnopqrstuvwxyz123456")];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<KEY_1>"), "{}", out[0].content);
    }

    #[test]
    fn test_credit_card_is_pseudonymized_and_restored() {
        // ADR-196: a Luhn-valid card (4111 1111 1111 1111, the standard Visa test
        // number) must be masked before reaching the cloud and restored after.
        // No-separator and space-separated forms both mask.
        for card in [
            "4111111111111111",
            "4111 1111 1111 1111",
            "4111-1111-1111-1111",
        ] {
            let msgs = vec![msg(&format!("charge {card} now"))];
            let (out, mapping) = pseudonymize_messages(&msgs);
            assert!(
                out[0].content.contains("<CARD_1>"),
                "card {card:?} must be masked: {}",
                out[0].content
            );
            assert!(
                !out[0].content.contains(card),
                "raw card {card:?} must not remain: {}",
                out[0].content
            );
            // Surrounding words are preserved and the value round-trips on restore.
            assert!(out[0].content.starts_with("charge "));
            assert!(out[0].content.ends_with(" now"));
            let restored = restore(&out[0].content, &mapping);
            assert!(restored.contains(card), "card must restore: {restored}");
        }
    }

    #[test]
    fn test_non_card_digits_not_masked() {
        // A short or non-Luhn digit run must not be masked as a card.
        let msgs = vec![msg("order 12345 and ref 4111111111111112")]; // 2nd fails Luhn
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(
            !out[0].content.contains("<CARD"),
            "no card token: {}",
            out[0].content
        );
        assert!(out[0].content.contains("12345"), "short number preserved");
    }

    #[test]
    fn test_iban_is_pseudonymized_and_restored() {
        // ADR-240: a checksum-valid IBAN must be masked before the cloud and
        // restored after, with surrounding words preserved.
        let msgs = vec![msg("wire to DE89370400440532013000 today")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert!(
            out[0].content.contains("<IBAN_1>"),
            "IBAN must be masked: {}",
            out[0].content
        );
        assert!(
            !out[0].content.contains("DE89370400440532013000"),
            "raw IBAN must not remain: {}",
            out[0].content
        );
        assert!(out[0].content.starts_with("wire to "));
        assert!(out[0].content.ends_with(" today"));
        let restored = restore(&out[0].content, &mapping);
        assert!(
            restored.contains("DE89370400440532013000"),
            "IBAN must restore: {restored}"
        );
    }

    #[test]
    fn test_short_all_digit_iban_masked_whole_not_as_card() {
        // A 15-char Norwegian IBAN = NO + 13 digits: the digit run alone is in
        // the 13-19 card window, so IBAN masking MUST run first and claim the
        // whole thing (prefix included) as one <IBAN> token, never a <CARD>.
        let msgs = vec![msg("acct NO9386011117947 ok")];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(
            out[0].content.contains("<IBAN_1>"),
            "short IBAN masked whole: {}",
            out[0].content
        );
        assert!(
            !out[0].content.contains("<CARD"),
            "must not be split into a card token: {}",
            out[0].content
        );
    }

    #[test]
    fn test_iban_in_tool_call_arguments_masked() {
        // An IBAN inside tool-call argument JSON is masked too (via replace_in_text).
        let tc =
            r#"[{"function":{"name":"pay","arguments":"{\"iban\":\"DE89370400440532013000\"}"}}]"#;
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc.to_string()),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap_or("");
        assert!(
            tc_out.contains("<IBAN_1>"),
            "IBAN in tool-call args must be masked: {tc_out}"
        );
        assert!(
            !tc_out.contains("DE89370400440532013000"),
            "raw IBAN must not remain in tool-call args: {tc_out}"
        );
    }

    #[test]
    fn test_credit_card_in_tool_call_arguments_masked() {
        // ADR-196 + ADR-188: a card inside tool-call argument JSON is masked too.
        let tc =
            r#"[{"function":{"name":"charge","arguments":"{\"card\":\"4111111111111111\"}"}}]"#;
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc.to_string()),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<CARD_1>"),
            "card in tool args must mask: {tc_out}"
        );
        assert!(
            !tc_out.contains("4111111111111111"),
            "raw card must not remain: {tc_out}"
        );
    }

    #[test]
    fn test_jwt_is_pseudonymized_and_restored() {
        // ADR-197: a JWT bearer token must be masked before reaching the cloud and
        // restored after — it is a single token, handled like an API key.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.dummsignature_abc123";
        let msgs = vec![msg(&format!("Authorization: Bearer {jwt}"))];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert!(
            out[0].content.contains("<JWT_1>"),
            "JWT must be masked: {}",
            out[0].content
        );
        assert!(
            !out[0].content.contains(jwt),
            "raw JWT must not remain: {}",
            out[0].content
        );
        let restored = restore(&out[0].content, &mapping);
        assert!(restored.contains(jwt), "JWT must restore: {restored}");
    }

    #[test]
    fn test_jwt_in_tool_call_arguments_masked() {
        // ADR-197 + ADR-188: a JWT inside tool-call argument JSON is masked too.
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJ1IjoieCJ9.sig_value_1234567";
        let tc =
            format!(r#"[{{"function":{{"name":"call","arguments":"{{\"token\":\"{jwt}\"}}"}}}}]"#);
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<JWT_1>"),
            "JWT in tool args must mask: {tc_out}"
        );
        assert!(!tc_out.contains(jwt), "raw JWT must not remain: {tc_out}");
    }

    #[test]
    fn test_role_preserved() {
        let msgs = vec![Message {
            role: "system".to_string(),
            content: "You help alice@example.com".to_string(),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        assert_eq!(out[0].role, "system");
        assert!(out[0].content.contains("<EMAIL_1>"));
    }

    // ── ADR-188: tool_calls_json pseudonymization ─────────────────────────────

    fn tool_call_msg(content: &str, tc_json: &str) -> Message {
        Message {
            role: "assistant".to_string(),
            content: content.to_string(),
            tool_calls_json: Some(tc_json.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn test_tool_calls_email_in_arguments_pseudonymized() {
        // ADR-188: an email in a tool-call argument (nested JSON-encoded string)
        // must be replaced — the whitespace tokenizer alone cannot find it inside
        // compact `{"email":"alice@example.com"}`.
        let tc = r#"[{"id":"c1","function":{"name":"send","arguments":"{\"email\":\"alice@example.com\"}"}}]"#;
        let msgs = vec![tool_call_msg("", tc)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<EMAIL_1>"),
            "email in tool-call arg must be pseudonymized: {tc_out}"
        );
        assert!(
            !tc_out.contains("alice@example.com"),
            "raw email must not remain in tool_calls_json: {tc_out}"
        );
        // mapping must contain the real value for restore().
        assert!(
            mapping.iter().any(|(v, _)| v == "alice@example.com"),
            "mapping must hold the original email for restore: {mapping:?}"
        );
    }

    #[test]
    fn test_tool_calls_ip_in_arguments_pseudonymized() {
        // IP address in compact JSON arguments must be replaced.
        let tc = r#"[{"function":{"name":"connect","arguments":"{\"host\":\"192.168.1.100\"}"}}]"#;
        let msgs = vec![tool_call_msg("", tc)];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<IP_1>"),
            "IP must be pseudonymized: {tc_out}"
        );
        assert!(
            !tc_out.contains("192.168.1.100"),
            "raw IP must not remain: {tc_out}"
        );
    }

    #[test]
    fn test_tool_calls_token_maps_back_via_restore() {
        // The token placed in tool_calls_json must survive restore() so the
        // cloud response that echoes the token can be de-anonymized.
        let tc = r#"[{"function":{"arguments":"{\"email\":\"alice@example.com\"}"}}]"#;
        let msgs = vec![tool_call_msg("", tc)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let cloud_response = format!("sent to {}", out[0].tool_calls_json.as_deref().unwrap());
        let restored = restore(&cloud_response, &mapping);
        assert!(
            restored.contains("alice@example.com"),
            "restore must recover original from cloud response: {restored}"
        );
    }

    #[test]
    fn test_tool_calls_benign_arguments_unchanged() {
        // A tool call with no PII must pass through without modification.
        let tc = r#"[{"function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]"#;
        let msgs = vec![tool_call_msg("", tc)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        // JSON may be reformatted by re-encode but must not change PII-free values.
        assert!(
            !tc_out.contains("<EMAIL"),
            "no email token for benign args: {tc_out}"
        );
        assert!(mapping.is_empty(), "no mapping entries for benign args");
    }

    #[test]
    fn test_tool_calls_same_email_in_content_and_args_shares_token() {
        // The same PII value appearing in both message content and a tool-call
        // argument must share a stable token across the full request (so restore
        // can use a single mapping entry to fix both).
        let email = "shared@example.com";
        let tc = format!(r#"[{{"function":{{"arguments":"{{\"to\":\"{email}\"}}"}}}}"#);
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: format!("sending to {email}"),
            tool_calls_json: Some(tc),
            ..Default::default()
        }];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert!(
            out[0].content.contains("<EMAIL_1>"),
            "email in content must be replaced: {}",
            out[0].content
        );
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<EMAIL_1>"),
            "same email in tool-call arg must use the same token: {tc_out}"
        );
        assert_eq!(mapping.len(), 1, "one mapping entry for the shared email");
    }

    #[test]
    fn test_restore_no_op_when_mapping_empty() {
        let text = "hello world";
        let result = restore(text, &[]);
        assert_eq!(result, text);
    }

    fn email_mapping() -> Mapping {
        vec![("alice@example.com".to_string(), "<EMAIL_1>".to_string())]
    }

    #[test]
    fn test_stream_restorer_single_delta_complete_token() {
        let mut r = StreamRestorer::new(email_mapping());
        let out = r.push("contact <EMAIL_1> now");
        let tail = r.finish();
        assert_eq!(format!("{out}{tail}"), "contact alice@example.com now");
    }

    #[test]
    fn test_stream_restorer_token_split_across_deltas() {
        // The token <EMAIL_1> arrives in two pieces; it must still restore.
        let mut r = StreamRestorer::new(email_mapping());
        let a = r.push("see <EMA");
        // Nothing past the dangling '<' may be emitted yet.
        assert_eq!(a, "see ");
        let b = r.push("IL_1> ok");
        let tail = r.finish();
        assert_eq!(
            format!("{a}{b}{tail}"),
            "see alice@example.com ok",
            "split token must restore: a={a:?} b={b:?} tail={tail:?}"
        );
    }

    #[test]
    fn test_stream_restorer_literal_angle_bracket_flushes_at_end() {
        // A bare '<' (e.g. code `a < b`) is held until the stream ends, then
        // flushed verbatim — never lost, never mistaken for a token.
        let mut r = StreamRestorer::new(email_mapping());
        let a = r.push("if a < b");
        let tail = r.finish();
        assert_eq!(format!("{a}{tail}"), "if a < b");
    }

    #[test]
    fn test_stream_restorer_passes_through_without_mapping() {
        let mut r = StreamRestorer::new(Vec::new());
        let a = r.push("plain text ");
        let b = r.push("more text");
        let tail = r.finish();
        assert_eq!(format!("{a}{b}{tail}"), "plain text more text");
    }

    #[test]
    fn test_stream_restorer_bounds_buffer_on_unterminated_angle() {
        // ADR-168: a dangling '<' followed by a long run with no '>' must not
        // buffer the whole tail (which would stall streaming and grow memory
        // unbounded). Once the fragment exceeds the longest token, it flushes.
        let mut r = StreamRestorer::new(email_mapping()); // <EMAIL_1> = 9 bytes
        let long_tail = "x".repeat(10_000);
        let out = r.push(&format!("note: a < {long_tail}"));
        // The bulk of the tail must have been emitted, not held in `pending`.
        assert!(
            out.len() >= long_tail.len(),
            "long unterminated-'<' tail must flush, not buffer: emitted {} bytes",
            out.len()
        );
        assert!(
            r.pending.len() < 16,
            "held-back buffer must stay bounded near max_token_len, was {}",
            r.pending.len()
        );
        let tail = r.finish();
        assert!(
            format!("{out}{tail}").contains("a < "),
            "literal '<' preserved"
        );
    }

    #[test]
    fn test_stream_restorer_split_token_still_restores_after_bound() {
        // The bound must not break the legitimate split-token case: a real token
        // arriving in pieces is shorter than max_token_len at each step, so it is
        // still held and restored once complete.
        let mut r = StreamRestorer::new(email_mapping());
        let a = r.push("hi <EMA");
        let b = r.push("IL_1>!");
        let tail = r.finish();
        assert_eq!(format!("{a}{b}{tail}"), "hi alice@example.com!");
    }

    #[test]
    fn test_stream_restorer_empty_mapping_never_buffers() {
        // With no mapping, restore is a no-op, so a '<' need never be held —
        // even a dangling '<' flushes immediately (pure pass-through).
        let mut r = StreamRestorer::new(Vec::new());
        let out = r.push("a < b still flowing");
        assert_eq!(out, "a < b still flowing");
        assert!(
            r.pending.is_empty(),
            "empty mapping must not buffer: {:?}",
            r.pending
        );
    }

    #[test]
    fn test_stream_restorer_token_never_leaks_per_delta() {
        // Feeding the token one byte at a time must never emit the raw token.
        let mut r = StreamRestorer::new(email_mapping());
        let mut out = String::new();
        for ch in "x <EMAIL_1> y".chars() {
            out.push_str(&r.push(&ch.to_string()));
        }
        out.push_str(&r.finish());
        assert_eq!(out, "x alice@example.com y");
        assert!(
            !out.contains("<EMAIL_1>"),
            "raw token must not survive: {out}"
        );
    }

    // ── ADR-203: url_credential + env_secret masking ────────────────────────

    #[test]
    fn test_url_credential_is_masked_and_restored() {
        // The credential (user:pass) must be replaced; scheme and host are kept.
        let msgs = vec![msg("connect to https://admin:hunter2@db.example.com/mydb")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<URL_1>"), "userinfo must be masked: {c}");
        assert!(!c.contains("hunter2"), "raw password must not remain: {c}");
        assert!(c.contains("https://"), "scheme must be preserved: {c}");
        assert!(c.contains("@db.example.com"), "host must be preserved: {c}");
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("admin:hunter2"),
            "userinfo must restore: {restored}"
        );
    }

    #[test]
    fn test_url_without_credential_unchanged() {
        // A port-only URL must not be masked (it has no userinfo).
        let msgs = vec![msg("server at http://localhost:8080/api/v1")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert_eq!(
            out[0].content, msgs[0].content,
            "port-only URL must not change"
        );
        assert!(mapping.is_empty());
    }

    #[test]
    fn test_multiple_url_credentials_masked_independently() {
        let msgs = vec![msg(
            "primary: postgres://alice:secret1@host1/db1 backup: postgres://bob:secret2@host2/db2",
        )];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<URL_1>"), "first cred must mask: {c}");
        assert!(c.contains("<URL_2>"), "second cred must mask: {c}");
        assert!(
            !c.contains("secret1") && !c.contains("secret2"),
            "no raw creds: {c}"
        );
        assert_eq!(mapping.len(), 2);
    }

    #[test]
    fn test_url_credential_in_tool_call_arguments_masked() {
        // ADR-203 + ADR-188: a URL with embedded credentials in tool-call JSON is masked.
        let tc = r#"[{"function":{"name":"connect","arguments":"{\"dsn\":\"mysql://user:p4ss@host/db\"}"}}]"#;
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc.to_string()),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<URL_1>"),
            "URL cred in tool args must mask: {tc_out}"
        );
        assert!(
            !tc_out.contains("p4ss"),
            "raw password must not remain: {tc_out}"
        );
    }

    #[test]
    fn test_env_secret_value_is_masked_and_restored() {
        // The value after `=` is masked; the key name is kept.
        let msgs = vec![msg("DB_PASSWORD=hunter2\nDB_HOST=localhost")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<ENV_1>"), "secret value must be masked: {c}");
        assert!(!c.contains("hunter2"), "raw value must not remain: {c}");
        assert!(c.contains("DB_PASSWORD="), "key must be preserved: {c}");
        assert!(c.contains("DB_HOST=localhost"), "benign var unchanged: {c}");
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("hunter2"),
            "value must restore: {restored}"
        );
    }

    #[test]
    fn test_env_secret_export_form_is_masked() {
        let msgs = vec![msg("export API_TOKEN=\"supersecrettoken123\"")];
        let (out, _) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<ENV_1>"), "export form must be masked: {c}");
        assert!(
            !c.contains("supersecrettoken123"),
            "raw token must not remain: {c}"
        );
        assert!(
            c.contains("export API_TOKEN="),
            "export + key preserved: {c}"
        );
    }

    #[test]
    fn test_env_trivial_value_not_masked() {
        // Short or boolean/null/empty values are not secrets.
        for s in ["DEBUG=true", "RETRY=0", "SECRET=", "DB_PASS=''"] {
            let msgs = vec![msg(s)];
            let (out, _) = pseudonymize_messages(&msgs);
            assert!(
                !out[0].content.contains("<ENV"),
                "trivial env assignment {s:?} must not mask: {}",
                out[0].content
            );
        }
    }

    // ── ADR-204: PEM private-key block masking ────────────────────────────

    // Fake RSA private key for tests — NOT a real key, purely structural.
    const FAKE_RSA_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAtestkey1234567890abcdef\nghijklmnopqrstuvwxyzABCDEFGHIJKLMN\n-----END RSA PRIVATE KEY-----";
    const FAKE_PKCS8_PEM: &str = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBGkqhkiG9w0BAQEFAASCBKcwggtest\n-----END PRIVATE KEY-----";
    const FAKE_EC_PEM: &str = "-----BEGIN EC PRIVATE KEY-----\nMHQCAQEEITestECkeydata1234567890abc\n-----END EC PRIVATE KEY-----";

    #[test]
    fn test_pem_rsa_key_is_masked_and_restored() {
        // The entire PEM block must be replaced; surrounding text is preserved.
        let msgs = vec![msg(&format!("here is my key:\n{FAKE_RSA_PEM}\nend"))];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<PEM_1>"), "PEM block must be masked: {c}");
        assert!(
            !c.contains("-----BEGIN"),
            "BEGIN header must not remain: {c}"
        );
        assert!(
            !c.contains("MIIEpAIBAAKCAQEA"),
            "key material must not remain: {c}"
        );
        assert!(c.starts_with("here is my key:"), "prefix preserved: {c}");
        assert!(c.ends_with("\nend"), "suffix preserved: {c}");
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("-----BEGIN RSA PRIVATE KEY-----"),
            "header restores: {restored}"
        );
        assert!(
            restored.contains("MIIEpAIBAAKCAQEA"),
            "key material restores: {restored}"
        );
    }

    #[test]
    fn test_pem_pkcs8_key_is_masked() {
        let msgs = vec![msg(&format!("key: {FAKE_PKCS8_PEM}"))];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<PEM_1>"), "{}", out[0].content);
        assert!(
            !out[0].content.contains("-----BEGIN PRIVATE KEY"),
            "{}",
            out[0].content
        );
    }

    #[test]
    fn test_pem_ec_key_is_masked() {
        let msgs = vec![msg(FAKE_EC_PEM)];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<PEM_1>"), "{}", out[0].content);
        assert!(
            !out[0].content.contains("-----BEGIN EC"),
            "{}",
            out[0].content
        );
    }

    #[test]
    fn test_multiple_pem_keys_masked_independently() {
        let text = format!("{FAKE_RSA_PEM}\nand\n{FAKE_EC_PEM}");
        let msgs = vec![msg(&text)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<PEM_1>"), "first key masked: {c}");
        assert!(c.contains("<PEM_2>"), "second key masked: {c}");
        assert_eq!(mapping.len(), 2, "two distinct mappings");
    }

    #[test]
    fn test_pem_public_key_not_masked() {
        // A PUBLIC KEY block must NOT be masked (not a private key).
        let pub_key = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9test\n-----END PUBLIC KEY-----";
        let msgs = vec![msg(pub_key)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert_eq!(
            out[0].content, pub_key,
            "public key must not change: {}",
            out[0].content
        );
        assert!(mapping.is_empty());
    }

    #[test]
    fn test_pem_certificate_not_masked() {
        // A CERTIFICATE block must NOT be masked.
        let cert =
            "-----BEGIN CERTIFICATE-----\nMIIDazCCAlOgAwIBAgItest\n-----END CERTIFICATE-----";
        let msgs = vec![msg(cert)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert_eq!(
            out[0].content, cert,
            "certificate must not change: {}",
            out[0].content
        );
        assert!(mapping.is_empty());
    }

    #[test]
    fn test_pem_key_in_tool_call_arguments_masked() {
        // ADR-204 + ADR-188: a PEM key inside tool-call argument JSON is masked.
        let pem_escaped = FAKE_RSA_PEM.replace('\n', "\\n");
        let tc = format!(
            r#"[{{"function":{{"name":"upload_key","arguments":"{{\"key\":\"{pem_escaped}\"}}"}}}}"#
        );
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<PEM_1>"),
            "PEM in tool args must mask: {tc_out}"
        );
        assert!(
            !tc_out.contains("MIIEpAIBAAKCAQEA"),
            "key material must not remain: {tc_out}"
        );
    }

    // ── ADR-205: restore() prefix-collision fix ──────────────────────────────

    #[test]
    fn test_restore_no_prefix_collision_with_10_plus_emails() {
        // ADR-205: tokens <EMAIL_1>…<EMAIL_9> are 9-byte prefixes of
        // <EMAIL_10>…<EMAIL_N>.  Before the fix, restore() iterated in insertion
        // order, so replacing <EMAIL_1> first would turn every <EMAIL_10> in the
        // text into `user1@example.com0>` (corrupted).  Longest-first ordering
        // ensures <EMAIL_10> is matched before <EMAIL_1>.
        let emails: Vec<String> = (1..=12).map(|i| format!("user{i}@example.com")).collect();
        let msgs = vec![msg(&emails.join(" "))];
        let (out, mapping) = pseudonymize_messages(&msgs);
        // Every value must have been tokenized.
        for i in 1..=12 {
            let tok = format!("<EMAIL_{i}>");
            assert!(
                out[0].content.contains(&tok),
                "token {tok} must appear: {}",
                out[0].content
            );
        }
        // restore() must reconstruct every original address without corruption.
        let restored = restore(&out[0].content, &mapping);
        for email in &emails {
            assert!(
                restored.contains(email.as_str()),
                "email {email} must restore without corruption: {restored}"
            );
        }
        // No raw token residue must remain in the restored text.
        assert!(
            !restored.contains("<EMAIL_"),
            "no token residue after restore: {restored}"
        );
    }

    #[test]
    fn test_restore_no_prefix_collision_with_10_plus_ip_addresses() {
        // Same prefix-collision risk for IP category (and any other ≥10 distinct values).
        let ips: Vec<String> = (1..=11).map(|i| format!("10.0.0.{i}")).collect();
        let msgs = vec![msg(&ips.join(" "))];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let restored = restore(&out[0].content, &mapping);
        for ip in &ips {
            assert!(
                restored.contains(ip.as_str()),
                "IP {ip} must restore: {restored}"
            );
        }
        assert!(!restored.contains("<IP_"), "no token residue: {restored}");
    }

    // ── ADR-206: trim_punct prefix/suffix offset fix ──────────────────────────

    #[test]
    fn test_email_with_trailing_comma_fully_masked() {
        // ADR-206: "alice@example.com," has a trailing comma stripped by trim_punct.
        // The old formula (token.len() - inner.len()) counted total stripped chars
        // and attributed them all to the prefix, so prefix="a" and the comma was
        // lost.  The fix uses pointer arithmetic so prefix="" and suffix=",".
        let msgs_comma = vec![msg("contact alice@example.com, for help")];
        let (out, mapping) = pseudonymize_messages(&msgs_comma);
        let c = &out[0].content;
        // Full email must not appear, including no partial leak of leading chars.
        assert!(
            !c.contains("alice"),
            "no raw email chars must remain (ADR-206): {c}"
        );
        assert!(c.contains("<EMAIL_1>"), "email must be tokenized: {c}");
        // The trailing comma must be preserved in the output (not swallowed).
        assert!(
            c.contains("<EMAIL_1>,"),
            "trailing comma must survive after token (ADR-206): {c}"
        );
        // Round-trip restore must recover the original.
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("alice@example.com,"),
            "email + comma must restore: {restored}"
        );
    }

    #[test]
    fn test_email_with_leading_punct_fully_masked() {
        // Leading quote before email: '"alice@example.com' — prefix should be '"'.
        let msgs = vec![msg(r#"address "alice@example.com" is valid"#)];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(!c.contains("alice"), "no raw email chars: {c}");
        assert!(c.contains("<EMAIL_1>"), "email tokenized: {c}");
        // Surrounding quotes preserved.
        assert!(c.contains('"'), "double-quote preserved: {c}");
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("alice@example.com"),
            "email restores: {restored}"
        );
    }

    #[test]
    fn test_email_with_trailing_semicolon_fully_masked() {
        // Semicolon at end of list: "alice@example.com;"
        let msgs = vec![msg("recipients: alice@example.com;")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(!c.contains("alice"), "no leading char leak: {c}");
        assert!(
            c.contains("<EMAIL_1>;") || c.ends_with("<EMAIL_1>"),
            "semicolon preserved or token at end: {c}"
        );
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("alice@example.com"),
            "restores: {restored}"
        );
    }

    #[test]
    fn test_ip_with_trailing_comma_fully_masked() {
        // Same prefix bug applies to IP addresses, not just emails.
        let msgs = vec![msg("servers 192.168.1.1, and 10.0.0.1,")];
        let (out, _) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        // No partial IP octets should leak.
        assert!(
            !c.contains("192.168"),
            "raw IP must not remain after comma (ADR-206): {c}"
        );
        assert!(c.contains("<IP_1>"), "first IP tokenized: {c}");
        assert!(c.contains("<IP_2>"), "second IP tokenized: {c}");
    }

    // ── ADR-207: international phone numbers spanning whitespace ───────────────

    #[test]
    fn test_intl_phone_with_spaces_masked_and_restored() {
        // ADR-207: `+1 555 123 4567` spans four whitespace tokens, so the
        // per-token loop alone cannot mask it. The phone_spans pre-pass must.
        let msgs = vec![msg("reach me at +1 555 123 4567 anytime")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<PHONE_1>"), "spaced phone must be masked: {c}");
        assert!(
            !c.contains("555 123"),
            "raw phone digits must not remain: {c}"
        );
        assert!(c.starts_with("reach me at "), "prefix preserved: {c}");
        assert!(c.ends_with(" anytime"), "suffix preserved: {c}");
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("+1 555 123 4567"),
            "phone must restore intact: {restored}"
        );
    }

    #[test]
    fn test_domestic_phone_still_masked_by_per_token_loop() {
        // The single-token domestic form must keep working (per-token path),
        // and must NOT be double-counted by the phone pre-pass (no '+').
        let msgs = vec![msg("携帯は 090-1234-5678 です")];
        let (out, _) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<PHONE_1>"), "domestic phone masked: {c}");
        assert!(!c.contains("090-1234-5678"), "raw domestic phone gone: {c}");
    }

    #[test]
    fn test_intl_phone_in_tool_call_arguments_masked() {
        // ADR-207 + ADR-188: a spaced international phone inside tool-call JSON
        // is masked too (the JSON walker hands the decoded value to the same
        // pre-pass chain).
        let tc = r#"[{"function":{"name":"sms","arguments":"{\"to\":\"+1 555 123 4567\"}"}}]"#;
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc.to_string()),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<PHONE_1>"),
            "phone in tool args must mask: {tc_out}"
        );
        assert!(
            !tc_out.contains("555 123"),
            "raw phone must not remain: {tc_out}"
        );
    }

    #[test]
    fn test_card_and_intl_phone_coexist() {
        // A card (digit-anchored) and a phone (+-anchored) in the same text are
        // masked independently by their disjoint pre-passes.
        let msgs = vec![msg("card 4111 1111 1111 1111 phone +1 555 123 4567")];
        let (out, _) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<CARD_1>"), "card masked: {c}");
        assert!(c.contains("<PHONE_1>"), "phone masked: {c}");
        assert!(!c.contains("4111"), "raw card gone: {c}");
        assert!(!c.contains("555 123"), "raw phone gone: {c}");
    }

    // ── ADR-212: My Number (マイナンバー) masking ──────────────────────────────

    #[test]
    fn test_my_number_masked_and_restored() {
        // ADR-212: a valid 12-digit My Number is masked and round-trips.
        let msgs = vec![msg("私のマイナンバーは 1234 5678 9018 です")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<MYNUMBER_1>"), "My Number must be masked: {c}");
        assert!(!c.contains("5678 9018"), "raw digits must not remain: {c}");
        assert!(
            c.contains("マイナンバーは "),
            "surrounding text preserved: {c}"
        );
        let restored = restore(c, &mapping);
        assert!(
            restored.contains("1234 5678 9018"),
            "My Number must restore: {restored}"
        );
    }

    #[test]
    fn test_my_number_and_card_coexist_disjoint() {
        // A My Number (12 digits) and a credit card (16 digits) coexist; the
        // length-disjoint pre-passes mask each with its own token.
        let msgs = vec![msg("mynumber 123456789018 card 4111111111111111")];
        let (out, _) = pseudonymize_messages(&msgs);
        let c = &out[0].content;
        assert!(c.contains("<MYNUMBER_1>"), "My Number masked: {c}");
        assert!(c.contains("<CARD_1>"), "card masked: {c}");
        assert!(!c.contains("123456789018"), "raw My Number gone: {c}");
        assert!(!c.contains("4111"), "raw card gone: {c}");
    }

    #[test]
    fn test_my_number_in_tool_call_arguments_masked() {
        // ADR-212 + ADR-188: a My Number inside tool-call JSON is masked too.
        let tc =
            r#"[{"function":{"name":"register","arguments":"{\"mynumber\":\"123456789018\"}"}}]"#;
        let msgs = vec![Message {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls_json: Some(tc.to_string()),
            ..Default::default()
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        let tc_out = out[0].tool_calls_json.as_deref().unwrap();
        assert!(
            tc_out.contains("<MYNUMBER_1>"),
            "My Number in tool args must mask: {tc_out}"
        );
        assert!(
            !tc_out.contains("123456789018"),
            "raw My Number must not remain: {tc_out}"
        );
    }
}

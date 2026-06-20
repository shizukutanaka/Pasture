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
//! already detects (email, IPv4, phone, API-key prefixes). It is NOT a
//! guarantee that no other sensitive information is sent — use `local_only`
//! mode for hard guarantees.
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
pub fn restore(text: &str, mapping: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (original, token) in mapping {
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
}

impl Ctx {
    fn new() -> Self {
        Self {
            mapping: Vec::new(),
            email_n: 0,
            ip_n: 0,
            phone_n: 0,
            key_n: 0,
        }
    }

    fn token_for(&mut self, original: &str, category: &str) -> String {
        // Reuse existing token for the same value (stable within one request).
        if let Some((_, tok)) = self.mapping.iter().find(|(v, _)| v == original) {
            return tok.clone();
        }
        let n = match category {
            "EMAIL" => { self.email_n += 1; self.email_n }
            "IP" => { self.ip_n += 1; self.ip_n }
            "PHONE" => { self.phone_n += 1; self.phone_n }
            _ => { self.key_n += 1; self.key_n }
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
        let prefix = &token[..token.len() - inner.len()];
        let suffix = &token[prefix.len() + inner.len()..];

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
        None
    }
}

/// Strip leading/trailing punctuation that wraps a token but is not part
/// of the value (e.g. `"user@example.com"` → `user@example.com`).
fn trim_punct(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>' | '[' | ']'))
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
                        '"'  => { decoded.push('"');  i += 2; }
                        '\\' => { decoded.push('\\'); i += 2; }
                        'n'  => { decoded.push('\n'); i += 2; }
                        't'  => { decoded.push('\t'); i += 2; }
                        'r'  => { decoded.push('\r'); i += 2; }
                        _    => { decoded.push('\\'); decoded.push(esc); i += 2; }
                    }
                }
                '"' => break,
                c   => { decoded.push(c); i += 1; }
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

/// Replace PII in `text` token by token, preserving original whitespace.
fn replace_in_text(text: &str, ctx: &mut Ctx) -> String {
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
        assert_eq!(count, 2, "same email should use the same token: {}", out[0].content);
        assert_eq!(mapping.len(), 1, "only one mapping entry for the same value");
    }

    #[test]
    fn test_benign_text_unchanged() {
        let msgs = vec![msg("what is the capital of France?")];
        let (out, mapping) = pseudonymize_messages(&msgs);
        assert_eq!(out[0].content, msgs[0].content, "benign text must not change");
        assert!(mapping.is_empty(), "no mapping for benign text");
    }

    #[test]
    fn test_whitespace_preserved() {
        let text = "  hello  world  ";
        let msgs = vec![msg(text)];
        let (out, _) = pseudonymize_messages(&msgs);
        assert_eq!(out[0].content, text, "whitespace must be preserved when no PII");
    }

    #[test]
    fn test_api_key_is_pseudonymized() {
        let msgs = vec![msg("my key is sk-abcdefghijklmnopqrstuvwxyz123456")];
        let (out, _) = pseudonymize_messages(&msgs);
        assert!(out[0].content.contains("<KEY_1>"), "{}", out[0].content);
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
        assert!(tc_out.contains("<IP_1>"), "IP must be pseudonymized: {tc_out}");
        assert!(!tc_out.contains("192.168.1.100"), "raw IP must not remain: {tc_out}");
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
        assert!(!tc_out.contains("<EMAIL"), "no email token for benign args: {tc_out}");
        assert!(mapping.is_empty(), "no mapping entries for benign args");
    }

    #[test]
    fn test_tool_calls_same_email_in_content_and_args_shares_token() {
        // The same PII value appearing in both message content and a tool-call
        // argument must share a stable token across the full request (so restore
        // can use a single mapping entry to fix both).
        let email = "shared@example.com";
        let tc = format!(
            r#"[{{"function":{{"arguments":"{{\"to\":\"{email}\"}}"}}}}"#
        );
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
        assert!(format!("{out}{tail}").contains("a < "), "literal '<' preserved");
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
        assert!(r.pending.is_empty(), "empty mapping must not buffer: {:?}", r.pending);
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
        assert!(!out.contains("<EMAIL_1>"), "raw token must not survive: {out}");
    }
}

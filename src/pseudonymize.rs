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
        }];
        let (out, _) = pseudonymize_messages(&msgs);
        assert_eq!(out[0].role, "system");
        assert!(out[0].content.contains("<EMAIL_1>"));
    }

    #[test]
    fn test_restore_no_op_when_mapping_empty() {
        let text = "hello world";
        let result = restore(text, &[]);
        assert_eq!(result, text);
    }
}

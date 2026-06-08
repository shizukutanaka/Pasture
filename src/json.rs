//! A minimal, dependency-free JSON parser and serializer.
//!
//! Scope is deliberately small: enough to parse OpenAI-style chat requests and
//! build OpenAI-style responses. It is exercised heavily by tests because
//! hand-written parsers are error-prone (I7).

use std::collections::BTreeMap;

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            JsonValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Look up a key on an object value.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            JsonValue::Object(m) => m.get(key),
            _ => None,
        }
    }

    /// Serialize back to a compact JSON string (the inverse of `parse`). Object
    /// keys are emitted in sorted order because the backing store is a `BTreeMap`,
    /// so the output is deterministic — useful for hashing/caching. Non-finite
    /// numbers serialize as `null` (JSON has no NaN/Infinity).
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            JsonValue::Number(n) => out.push_str(&fmt_json_number(*n)),
            JsonValue::Str(s) => {
                out.push('"');
                out.push_str(&escape_string(s));
                out.push('"');
            }
            JsonValue::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            JsonValue::Object(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape_string(k));
                    out.push_str("\":");
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// Format a JSON number: whole values within i64 range as integers (so a parsed
/// `2` round-trips as `2`, not `2.0`), others via the default float formatter,
/// and non-finite as `null`.
fn fmt_json_number(n: f64) -> String {
    if !n.is_finite() {
        return "null".to_string();
    }
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// A parse error with the byte position at which it occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON parse error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for ParseError {}

/// Parse a complete JSON document.
/// Maximum object/array nesting depth accepted by the parser.
const MAX_DEPTH: usize = 128;

pub fn parse(input: &str) -> Result<JsonValue, ParseError> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.parse_value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after value"));
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err(&self, msg: &str) -> ParseError {
        ParseError {
            pos: self.pos,
            msg: msg.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        // Bound recursion so adversarial deeply-nested JSON cannot overflow the
        // stack and abort the process (the proxy parses untrusted request bodies).
        if depth > MAX_DEPTH {
            return Err(self.err("maximum nesting depth exceeded"));
        }
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(JsonValue::Str(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(self.err("unexpected token")),
        }
    }

    fn expect(&mut self, c: u8) -> Result<(), ParseError> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err("expected a specific character"))
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(map));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.parse_value(depth + 1)?;
            map.insert(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
        Ok(JsonValue::Object(map))
    }

    fn parse_array(&mut self, depth: usize) -> Result<JsonValue, ParseError> {
        self.expect(b'[')?;
        let mut arr = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(arr));
        }
        loop {
            self.skip_ws();
            arr.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
        Ok(JsonValue::Array(arr))
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let esc = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            let cp = self.parse_unicode_escape()?;
                            out.push(cp);
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                _ => {
                    // Re-decode this byte as part of a UTF-8 sequence.
                    let start = self.pos - 1;
                    let len = utf8_len(c);
                    let end = start + len;
                    if end > self.bytes.len() {
                        return Err(self.err("invalid UTF-8 in string"));
                    }
                    let slice = &self.bytes[start..end];
                    match std::str::from_utf8(slice) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(self.err("invalid UTF-8 in string")),
                    }
                    self.pos = end;
                }
            }
        }
        Ok(out)
    }

    fn parse_unicode_escape(&mut self) -> Result<char, ParseError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| self.err("invalid \\u escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.err("invalid \\u hex"))?;
        self.pos += 4;
        char::from_u32(code).ok_or_else(|| self.err("invalid unicode code point"))
    }

    fn parse_bool(&mut self) -> Result<JsonValue, ParseError> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue, ParseError> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == b'-' || c == b'+' || c == b'.' || c == b'e' || c == b'E' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| ParseError {
                pos: start,
                msg: "invalid number".to_string(),
            })
    }
}

fn utf8_len(first_byte: u8) -> usize {
    if first_byte < 0x80 {
        1
    } else if first_byte >> 5 == 0b110 {
        2
    } else if first_byte >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// Escape a string for embedding inside a JSON document (no surrounding quotes).
pub fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_json_string_scalars() {
        assert_eq!(JsonValue::Null.to_json_string(), "null");
        assert_eq!(JsonValue::Bool(true).to_json_string(), "true");
        assert_eq!(JsonValue::Number(2.0).to_json_string(), "2"); // whole -> integer
        assert_eq!(JsonValue::Number(0.5).to_json_string(), "0.5");
        assert_eq!(
            JsonValue::Str("a\"b".to_string()).to_json_string(),
            "\"a\\\"b\""
        );
    }

    #[test]
    fn test_to_json_string_roundtrip_object() {
        let src = r#"{"a":[1,2,3],"b":{"c":true,"d":"x"},"e":null}"#;
        let v = parse(src).unwrap();
        // Keys are sorted (BTreeMap); this input is already in sorted order.
        let out = v.to_json_string();
        assert_eq!(out, src);
        // Re-parsing yields the same value (true round-trip).
        assert_eq!(parse(&out).unwrap(), v);
    }

    #[test]
    fn test_to_json_string_sorts_keys() {
        let v = parse(r#"{"z":1,"a":2}"#).unwrap();
        assert_eq!(v.to_json_string(), r#"{"a":2,"z":1}"#);
    }

    #[test]
    fn test_parse_object_with_string() {
        let v = parse(r#"{"role":"user","content":"hi"}"#).unwrap();
        assert_eq!(v.get("role").and_then(|x| x.as_str()), Some("user"));
        assert_eq!(v.get("content").and_then(|x| x.as_str()), Some("hi"));
    }

    #[test]
    fn test_parse_nested_array_of_objects() {
        let v = parse(r#"{"messages":[{"role":"user","content":"a"}]}"#).unwrap();
        let arr = v.get("messages").and_then(|m| m.as_array()).unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0].get("content").and_then(|c| c.as_str()), Some("a"));
    }

    #[test]
    fn test_parse_bool_and_null_and_number() {
        assert_eq!(parse("true").unwrap(), JsonValue::Bool(true));
        assert_eq!(parse("false").unwrap(), JsonValue::Bool(false));
        assert_eq!(parse("null").unwrap(), JsonValue::Null);
        assert_eq!(parse("-12.5").unwrap(), JsonValue::Number(-12.5));
    }

    #[test]
    fn test_parse_string_escapes() {
        let v = parse(r#""line\nbreak \"q\" end""#).unwrap();
        assert_eq!(v.as_str(), Some("line\nbreak \"q\" end"));
    }

    #[test]
    fn test_parse_unicode_escape() {
        let v = parse(r#""\u0041""#).unwrap();
        assert_eq!(v.as_str(), Some("A"));
    }

    #[test]
    fn test_parse_multibyte_utf8_passthrough() {
        let v = parse("\"日本語\"").unwrap();
        assert_eq!(v.as_str(), Some("日本語"));
    }

    #[test]
    fn test_parse_empty_object_and_array() {
        assert_eq!(parse("{}").unwrap(), JsonValue::Object(BTreeMap::new()));
        assert_eq!(parse("[]").unwrap(), JsonValue::Array(vec![]));
    }

    #[test]
    fn test_parse_rejects_trailing_garbage() {
        assert!(parse("{} x").is_err());
    }

    #[test]
    fn test_parse_rejects_unterminated_string() {
        assert!(parse("\"abc").is_err());
    }

    #[test]
    fn test_escape_string_roundtrip() {
        let original = "tab\tnewline\n\"quote\"\\back";
        let escaped = escape_string(original);
        let doc = format!("\"{escaped}\"");
        assert_eq!(parse(&doc).unwrap().as_str(), Some(original));
    }

    #[test]
    fn test_deeply_nested_is_rejected_not_overflow() {
        // 100k-deep input must error gracefully, never overflow the stack.
        assert!(parse(&"[".repeat(100_000)).is_err());
        let n = MAX_DEPTH + 50;
        let deep = format!("{}1{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn test_moderate_nesting_ok() {
        let n = 50; // comfortably within MAX_DEPTH
        let doc = format!("{}1{}", "[".repeat(n), "]".repeat(n));
        assert!(parse(&doc).is_ok());
    }
}

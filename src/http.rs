//! Minimal blocking HTTP/1.1 wire layer for the proxy (ADR-276, IMP-49).
//!
//! Reads one request off a `TcpStream` (keep-alive pipelining, body-size and
//! header-count DoS guards, read-timeout detection), checks bearer tokens in
//! constant time, and writes JSON / Prometheus-text / HTML / SSE response
//! heads. No routing, no `Proxy` state: moved verbatim out of `proxy.rs`, which
//! now keeps only request orchestration. Response *bodies* are built in
//! `response.rs` (ADR-275).

use crate::json::escape_string;
use std::io::{Read, Write};

pub(crate) fn write_sse_headers(
    stream: &mut std::net::TcpStream,
    cors: &str,
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n{cors}\r\n"
    );
    stream.write_all(headers.as_bytes())
}
/// Default maximum request body the proxy will read (SPEC §7, IMP-21).
/// Overridden at runtime via `PASTURE_MAX_BODY_BYTES` / `Proxy::with_max_body_bytes`.
pub(crate) const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Outcome of reading one HTTP request off the socket.
pub(crate) enum ReadOutcome {
    Request {
        method: String,
        path: String,
        body: String,
        /// The `Authorization` header value, if present (used for auth, IMP-15).
        auth: Option<String>,
        /// The `Origin` header value, if present (used for CORS, IMP-cors).
        origin: Option<String>,
        /// True when the client explicitly requested `Connection: close`, or when
        /// the request is HTTP/1.0 without an explicit `Connection: keep-alive`
        /// (HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close).
        connection_close: bool,
        /// The `X-Request-ID` header value, if present; echoed on all responses
        /// for request tracing.
        request_id: Option<String>,
        /// The `Content-Type` header value (lowercased), if present. Used to
        /// return 415 Unsupported Media Type for non-JSON POST bodies.
        content_type: Option<String>,
    },
    /// The declared or actual body exceeded `MAX_BODY_BYTES` → 413. Always
    /// raised after the header block was parsed, so its head is known.
    TooLarge { head: RejectedHead },
    /// A socket read timed out before the request completed → 408 (IMP-timeout).
    /// `head` is `None` when the timeout hit before the headers were complete.
    TimedOut { head: Option<RejectedHead> },
    /// Connection closed early or headers were malformed/oversized → 400.
    Closed,
}

/// What was already parsed when a request was rejected before dispatch
/// (ADR-277), so the 413/408 can carry the same trace id, CORS headers and
/// access-log line as every other response instead of being anonymous.
pub(crate) struct RejectedHead {
    pub method: String,
    pub path: String,
    pub origin: Option<String>,
    pub request_id: Option<String>,
}

/// True for an I/O error that means "no data within the read timeout window".
pub(crate) fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Read one HTTP/1.x request from `stream`, using `conn_buf` as a persistent
/// accumulation buffer. Any bytes read past the end of this request remain in
/// `conn_buf` for the next call (keep-alive pipelining support).
pub(crate) fn read_request(
    stream: &mut std::net::TcpStream,
    conn_buf: &mut Vec<u8>,
    max_body_bytes: usize,
) -> std::io::Result<ReadOutcome> {
    let mut chunk = [0u8; 1024];
    // Accumulate bytes until the header terminator is found.
    let header_end = loop {
        if let Some(pos) = find_subslice(conn_buf, b"\r\n\r\n") {
            break pos;
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut { head: None }),
            Err(e) => return Err(e),
        };
        if n == 0 {
            // EOF with no headers seen → clean connection close.
            return Ok(ReadOutcome::Closed);
        }
        conn_buf.extend_from_slice(&chunk[..n]);
        if conn_buf.len() > 1_048_576 {
            return Ok(ReadOutcome::Closed); // 1 MiB header guard
        }
    };

    let header_text = String::from_utf8_lossy(&conn_buf[..header_end]).into_owned();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close.
    let http11 = request_line.contains("HTTP/1.1");
    let mut content_length = 0usize;
    let mut auth: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut connection_close = !http11; // HTTP/1.0 default = close
    let mut request_id: Option<String> = None;
    let mut content_type: Option<String> = None;
    for (header_idx, line) in lines.enumerate() {
        if header_idx >= 1000 {
            // Too many header fields — reject as malformed (DoS guard, ADR-097).
            return Ok(ReadOutcome::Closed);
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("content-type:") {
            content_type = Some(v.trim().to_string());
        } else if lower.starts_with("authorization:") {
            if let Some((_, v)) = line.split_once(':') {
                auth = Some(v.trim().to_string());
            }
        } else if lower.starts_with("origin:") {
            if let Some((_, v)) = line.split_once(':') {
                origin = Some(v.trim().to_string());
            }
        } else if let Some(v) = lower.strip_prefix("connection:") {
            let val = v.trim();
            connection_close = val == "close";
            // HTTP/1.0 + "Connection: keep-alive" → keep alive
            if !http11 && val == "keep-alive" {
                connection_close = false;
            }
        } else if lower.starts_with("x-request-id:") {
            if let Some((_, v)) = line.split_once(':') {
                // Strip CR/LF to guard against header injection.
                let clean: String = v
                    .trim()
                    .chars()
                    .filter(|&c| c != '\r' && c != '\n')
                    .collect();
                if !clean.is_empty() {
                    request_id = Some(clean);
                }
            }
        }
    }

    let head = || RejectedHead {
        method: method.clone(),
        path: path.clone(),
        origin: origin.clone(),
        request_id: request_id.clone(),
    };

    // Reject oversized bodies before reading them (DoS guard, SPEC §7).
    if content_length > max_body_bytes {
        return Ok(ReadOutcome::TooLarge { head: head() });
    }

    // The body starts right after the \r\n\r\n. bytes already in conn_buf
    // past that offset are part of the body (or a pipelined next request).
    let body_start = header_end + 4;
    let body_end = body_start + content_length;

    // Read more bytes until we have the full body.
    while conn_buf.len() < body_end {
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) if is_timeout(&e) => return Ok(ReadOutcome::TimedOut { head: Some(head()) }),
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        conn_buf.extend_from_slice(&chunk[..n]);
        if conn_buf.len() > body_end + max_body_bytes {
            return Ok(ReadOutcome::TooLarge { head: head() });
        }
    }

    // Extract exactly content_length body bytes.
    let body_bytes = conn_buf[body_start..body_end.min(conn_buf.len())].to_vec();
    // Drain the consumed request bytes; any remainder belongs to the next request.
    conn_buf.drain(..body_end.min(conn_buf.len()));

    Ok(ReadOutcome::Request {
        method,
        path,
        body: String::from_utf8_lossy(&body_bytes).into_owned(),
        auth,
        origin,
        connection_close,
        request_id,
        content_type,
    })
}

/// Validate a bearer token from an `Authorization` header against the expected
/// value, in constant time (no early return on first mismatch).
pub(crate) fn auth_ok(header: Option<&str>, expected: &str) -> bool {
    let Some(h) = header else {
        return false;
    };
    let token = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
        .unwrap_or(h)
        .trim();
    constant_time_eq(token.as_bytes(), expected.as_bytes())
}

/// Length-checked, constant-time byte comparison (avoids leaking token length
/// match timing beyond the unavoidable length check).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Write a HEAD-only response (headers identical to the GET equivalent, no body).
/// `content_length` is the body size the equivalent GET would return (RFC 7231 §4.3.2).
pub(crate) fn write_head_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    content_length: usize,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {content_length}\r\nConnection: {conn}\r\n{extra}\r\n"
    );
    stream.write_all(response.as_bytes())
}

/// Write an HTTP/1.1 response. `extra` is a block of additional header lines
/// (each already terminated with `\r\n`, e.g. CORS headers) or empty.
/// `keep_alive` controls the `Connection:` header value.
pub(crate) fn write_response(
    stream: &mut std::net::TcpStream,
    status: u16,
    body: &str,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {conn}\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

/// Write an HTTP/1.1 response with `Content-Type: text/plain` (for `/metrics`).
pub(crate) fn write_plain_response(
    stream: &mut std::net::TcpStream,
    body: &str,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: {conn}\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

/// Write an HTTP/1.1 `200 OK` with `Content-Type: text/html` (for the embedded
/// dashboard at `GET /dashboard`). Kept separate from `write_response` (which
/// hardcodes `application/json`) and `write_plain_response` (Prometheus text)
/// so each endpoint family advertises the correct media type. The body is
/// compile-time constant HTML, so `body.len()` is the exact byte length.
pub(crate) fn write_html_response(
    stream: &mut std::net::TcpStream,
    body: &str,
    extra: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let conn = if keep_alive { "keep-alive" } else { "close" };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: {conn}\r\n{extra}\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

/// Append one access-log record (JSONL, no PII) to the configured file.
/// Fields: ts (Unix epoch ms), method, path (no query string), status, ms, request_id?.
/// Silently ignores write errors (access log is best-effort; never drops the request).
pub(crate) fn append_access_log(
    path: &str,
    method: &str,
    norm_path: &str,
    status: u16,
    ms: u128,
    request_id: Option<&str>,
) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let req_id_field = match request_id {
        Some(id) => format!(",\"request_id\":\"{}\"", escape_string(id)),
        None => String::new(),
    };
    let line = format!(
        "{{\"ts\":{ts},\"method\":\"{}\",\"path\":\"{}\",\"status\":{status},\"ms\":{ms}{req_id_field}}}\n",
        escape_string(method),
        escape_string(norm_path),
    );
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

pub(crate) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

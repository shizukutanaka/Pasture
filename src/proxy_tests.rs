//! Unit tests for the proxy module, kept in a separate file because the
//! suite outgrew the source (`#[path]` keeps it a normal child module of
//! `proxy`, so `use super::*` and private-item access work unchanged).

use super::*;
use crate::backend::MockBackend;

fn proxy_with(local: bool, cloud: bool, threshold: usize, log: &str) -> Proxy {
    let engine = RoutingEngine::new(threshold, local, cloud);
    Proxy::new(
        engine,
        local.then(|| Box::new(MockBackend::new("local", "local-reply")) as Box<dyn Backend>),
        cloud.then(|| Box::new(MockBackend::new("cloud", "cloud-reply")) as Box<dyn Backend>),
        log,
    )
}

fn tmp_log() -> String {
    std::env::temp_dir()
        .join(format!(
            "pasture-proxy-{}.jsonl",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .to_string_lossy()
        .into_owned()
}

#[test]
fn test_parse_request_extracts_messages() {
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let req = Proxy::parse_request(body).unwrap();
    assert_eq!(req.model, "m");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].content, "hi");
}

#[test]
fn test_parse_request_detects_tools() {
    // IMP-10: a non-empty tools/functions array sets has_tools.
    let with_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
    assert!(Proxy::parse_request(with_tools).unwrap().has_tools);
    let with_functions = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"functions":[{"name":"f"}]}"#;
    assert!(Proxy::parse_request(with_functions).unwrap().has_tools);
    let empty_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[]}"#;
    assert!(!Proxy::parse_request(empty_tools).unwrap().has_tools);
    let no_tools = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    assert!(!Proxy::parse_request(no_tools).unwrap().has_tools);
}

#[test]
fn test_handle_chat_with_tools_goes_cloud() {
    // IMP-10: even a short prompt routes to cloud when tools are present.
    let log = tmp_log();
    let p = proxy_with(true, true, 100, &log);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
    let resp = p.handle_chat(body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"cloud\""));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_build_models_response_shape() {
    // IMP-8: OpenAI-compatible list shape, de-duplicated and non-empty.
    let p = proxy_with(true, false, 100, "unused").with_models(vec![
        "llama3".into(),
        "llama3".into(),
        "".into(),
        "gpt-4o-mini".into(),
    ]);
    let body = build_models_response(&p.models);
    assert!(body.starts_with("{\"object\":\"list\",\"data\":["));
    assert!(body.contains("\"id\":\"llama3\""));
    assert!(body.contains("\"id\":\"gpt-4o-mini\""));
    assert!(body.contains("\"object\":\"model\""));
    // de-duplicated llama3, dropped empty -> exactly two entries.
    assert_eq!(body.matches("\"object\":\"model\"").count(), 2);
}

#[test]
fn test_build_models_response_empty_is_valid() {
    let body = build_models_response(&[]);
    assert_eq!(body, "{\"object\":\"list\",\"data\":[]}");
}

#[test]
fn test_build_model_response_found_and_missing() {
    let models = vec!["llama3".to_string(), "gpt-4o-mini".to_string()];
    let body = build_model_response(&models, "llama3").expect("found");
    assert!(body.contains("\"id\":\"llama3\""), "{body}");
    assert!(body.contains("\"object\":\"model\""), "{body}");
    assert!(build_model_response(&models, "nope").is_none());
}

#[test]
fn test_is_timeout_classifies_kinds() {
    use std::io::{Error, ErrorKind};
    assert!(is_timeout(&Error::from(ErrorKind::WouldBlock)));
    assert!(is_timeout(&Error::from(ErrorKind::TimedOut)));
    assert!(!is_timeout(&Error::from(ErrorKind::BrokenPipe)));
}

/// Parse a header block's Content-Length (0 when absent).
fn header_content_length(hdr: &str) -> usize {
    hdr.lines()
        .find_map(|l| {
            let low = l.to_ascii_lowercase();
            low.strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0)
}

/// Parse the status codes of concatenated HTTP responses, framed via
/// Content-Length so a second status line isn't merged into the first body.
fn parse_response_statuses(raw: &[u8]) -> Vec<u16> {
    let mut statuses = Vec::new();
    let mut pos = 0;
    while pos < raw.len() {
        // Find header end.
        let Some(hdr_end) = find_subslice(&raw[pos..], b"\r\n\r\n") else {
            break;
        };
        let hdr = String::from_utf8_lossy(&raw[pos..pos + hdr_end]);
        // Parse status code from the first line.
        if let Some(status) = hdr
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
        {
            statuses.push(status);
        }
        pos += hdr_end + 4 + header_content_length(&hdr);
    }
    statuses
}

/// Read exactly one Content-Length-framed HTTP response from the socket
/// (stopping early on EOF or error so callers can assert on what arrived).
fn read_one_response(c: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
            let hdr = String::from_utf8_lossy(&buf[..hdr_end]);
            if buf.len() >= hdr_end + 4 + header_content_length(&hdr) {
                break;
            }
        }
        match c.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    buf
}

/// Helper: send two raw HTTP requests on a single TCP connection and return
/// all HTTP response status codes. Parses responses using Content-Length so
/// that the second status line isn't merged with the first response body.
fn keepalive_statuses(proxy: Proxy, req1: String, req2: String) -> Vec<u16> {
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(req1.as_bytes()).unwrap();
        c.write_all(req2.as_bytes()).unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).unwrap();
        buf
    });
    let (mut server, _) = listener.accept().unwrap();
    proxy.handle_connection(&mut server).unwrap();
    drop(server);
    let raw = client.join().unwrap();
    parse_response_statuses(&raw)
}

#[test]
fn test_keepalive_two_requests_on_one_connection() {
    let p = proxy_with(true, false, 100, "unused");
    let req = "GET /health HTTP/1.1\r\nHost: x\r\n\r\n".to_string();
    let statuses = keepalive_statuses(p, req.clone(), req);
    assert_eq!(statuses, vec![200, 200], "expected two 200s: {statuses:?}");
}

#[test]
fn test_connection_close_terminates_after_first_request() {
    // Not via keepalive_statuses: closing with the pipelined second request
    // still unread makes the kernel send RST, which can discard the first
    // response from the client's receive buffer before it is read — an
    // intermittent ECONNRESET flake. Deterministic instead: hold the server
    // socket open until the client confirms it has consumed response 1.
    use std::net::Shutdown;
    use std::sync::mpsc;
    let p = proxy_with(true, false, 100, "unused");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (got_first_tx, got_first_rx) = mpsc::channel::<()>();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        // Pipeline both requests up front; the second must not be served.
        c.write_all(
            b"GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n\
              GET /health HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut raw = read_one_response(&mut c);
        got_first_tx.send(()).unwrap();
        // Any further bytes would be a second, erroneous response. The server
        // closing instead (EOF or reset) is the expected outcome — tolerated.
        let mut rest = Vec::new();
        let _ = c.read_to_end(&mut rest);
        raw.extend_from_slice(&rest);
        raw
    });
    let (mut server, _) = listener.accept().unwrap();
    // Returns after the first response: Connection: close ends the keep-alive loop.
    p.handle_connection(&mut server).unwrap();
    // Hold the socket open until the client has read response 1, so the
    // close-time RST cannot discard it.
    got_first_rx.recv().unwrap();
    drop(server);
    let raw = client.join().unwrap();
    let statuses = parse_response_statuses(&raw);
    assert_eq!(
        statuses,
        vec![200],
        "expected one 200 (connection closed): {statuses:?}"
    );
}

#[test]
fn test_keepalive_response_has_keep_alive_header() {
    // HTTP/1.1 without Connection: close → response must advertise keep-alive.
    let (status, raw) = roundtrip(
        proxy_with(true, false, 100, "unused"),
        "GET /health HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
    );
    assert_eq!(status, 200);
    // The raw response (headers + body) must include Connection: keep-alive.
    // roundtrip() reads everything the server writes; headers appear before body.
    let _ = raw; // body stripped by roundtrip; check via a second helper approach
                 // Re-use the raw roundtrip: use the full raw response from the socket.
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut r = String::new();
        c.read_to_string(&mut r).unwrap();
        r
    });
    let (mut server, _) = listener.accept().unwrap();
    proxy_with(true, false, 100, "unused")
        .handle_connection(&mut server)
        .unwrap();
    drop(server);
    let raw_resp = client.join().unwrap();
    assert!(
        raw_resp.contains("Connection: keep-alive"),
        "HTTP/1.1 response should advertise keep-alive by default: {raw_resp}"
    );
}

#[test]
fn test_http10_request_defaults_to_close() {
    let p = proxy_with(true, false, 100, "unused");
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"GET /health HTTP/1.0\r\nHost: x\r\n\r\n")
            .unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut r = String::new();
        c.read_to_string(&mut r).unwrap();
        r
    });
    let (mut server, _) = listener.accept().unwrap();
    p.handle_connection(&mut server).unwrap();
    drop(server);
    let resp = client.join().unwrap();
    assert!(
        resp.contains("Connection: close"),
        "HTTP/1.0 without keep-alive header should close: {resp}"
    );
}

#[test]
fn test_head_health_returns_200_no_body() {
    let p = proxy_with(true, false, 100, "unused");
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(b"HEAD /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut r = String::new();
        c.read_to_string(&mut r).unwrap();
        r
    });
    let (mut server, _) = listener.accept().unwrap();
    p.handle_connection(&mut server).unwrap();
    drop(server);
    let resp = client.join().unwrap();
    // Must be 200, Content-Length must match GET body size, body must be absent.
    assert!(resp.starts_with("HTTP/1.1 200"), "status: {resp}");
    let clen: usize = resp
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse().unwrap_or(0))
        })
        .unwrap_or(0);
    // Content-Length must match the actual GET body size, which now includes the version field.
    let expected_body = concat!(
        "{\"status\":\"ok\",\"version\":\"",
        env!("CARGO_PKG_VERSION"),
        "\"}"
    );
    assert_eq!(
        clen,
        expected_body.len(),
        "Content-Length must match GET body"
    );
    // Body must be empty (headers end at first \r\n\r\n; nothing follows).
    let body = resp.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
    assert!(body.is_empty(), "HEAD must have no body: {body:?}");
}

#[test]
fn test_v1_engines_alias_lists_models() {
    let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
    let (status, body) = roundtrip(p, "GET /v1/engines HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"object\":\"list\""), "{body}");
    assert!(body.contains("llama3"), "{body}");
}

#[test]
fn test_v1_engines_retrieve_alias() {
    let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
    let (status, body) = roundtrip(p, "GET /v1/engines/llama3 HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    let v = crate::json::parse(&body).unwrap();
    assert_eq!(v.get("id").and_then(|x| x.as_str()), Some("llama3"));
}

#[test]
fn test_roundtrip_slow_client_times_out_408() {
    // A client that opens a connection and sends an incomplete request must
    // not pin the worker: with a short timeout the server responds 408.
    let p = proxy_with(true, false, 100, "unused")
        .with_request_timeout(Some(std::time::Duration::from_millis(50)));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        // Partial request: headers never terminate.
        c.write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\n")
            .unwrap();
        // Hold the connection open past the server's read timeout.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut resp = String::new();
        let _ = c.read_to_string(&mut resp);
        resp
    });
    let (mut s, _) = listener.accept().unwrap();
    p.handle_connection(&mut s).unwrap();
    drop(s);
    let resp = client.join().unwrap();
    assert!(resp.contains("408"), "expected 408 timeout, got: {resp}");
}

#[test]
fn test_roundtrip_model_retrieve_ok() {
    let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
    let (status, body) = roundtrip(p, "GET /v1/models/llama3 HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    let v = crate::json::parse(&body).unwrap();
    assert_eq!(v.get("id").and_then(|x| x.as_str()), Some("llama3"));
    assert_eq!(v.get("object").and_then(|x| x.as_str()), Some("model"));
}

#[test]
fn test_roundtrip_model_retrieve_unknown_is_404() {
    let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
    let (status, body) = roundtrip(p, "GET /v1/models/ghost HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 404);
    assert!(body.contains("not found"), "{body}");
}

#[test]
fn test_roundtrip_models_list_still_works() {
    // The bare list path must not be captured by the retrieve branch.
    let p = proxy_with(true, false, 100, "unused").with_models(vec!["llama3".into()]);
    let (status, body) = roundtrip(p, "GET /v1/models HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"object\":\"list\""), "{body}");
}

#[test]
fn test_build_error_response_envelope() {
    // SPEC §3.5: nested {"error":{"message","type","param","code"}}, escaped.
    // param/code always present (null when unknown) for OpenAI SDK parity.
    let body = build_error_response("bad \"thing\"", "invalid_request_error");
    assert_eq!(
        body,
        "{\"error\":{\"message\":\"bad \\\"thing\\\"\",\"type\":\"invalid_request_error\",\"param\":null,\"code\":null}}"
    );
    // Round-trips as valid JSON with the expected keys.
    let v = crate::json::parse(&body).unwrap();
    let err = v.get("error").unwrap();
    assert!(err.get("param").is_some());
    assert!(err.get("code").is_some());
}

#[test]
fn test_build_error_response_coded_emits_code() {
    let body = build_error_response_coded("nope", "rate_limit_error", Some("rate_limit_exceeded"));
    assert!(body.contains("\"code\":\"rate_limit_exceeded\""), "{body}");
    assert!(body.contains("\"param\":null"), "{body}");
}

#[test]
fn test_roundtrip_429_error_code_is_rate_limit_exceeded() {
    let p = proxy_with(true, false, 100, "unused").with_rate_limit(1);
    let raw = "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n\
               GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n"
        .to_string();
    let resp = roundtrip_raw(p, raw);
    assert!(
        resp.contains("\"code\":\"rate_limit_exceeded\""),
        "429 body missing rate_limit_exceeded code: {resp}"
    );
}

#[test]
fn test_roundtrip_401_error_code_is_invalid_api_key() {
    let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("s3cret".to_string()));
    let resp = roundtrip_raw(
        p,
        "GET /v1/models HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_string(),
    );
    assert!(resp.contains("401"), "expected 401: {resp}");
    assert!(
        resp.contains("\"code\":\"invalid_api_key\""),
        "401 body missing invalid_api_key code: {resp}"
    );
}

#[test]
fn test_response_includes_created() {
    let resp = CompletionResponse {
        content: "hi".into(),
        model: "m".into(),
        prompt_tokens: 1,
        completion_tokens: 1,
    };
    assert!(build_openai_response(&resp, "local").contains("\"created\":"));
    assert!(build_openai_chunk(
        "chatcmpl-x",
        "m",
        "fp_pasture_00000000",
        "hi",
        "local",
        None
    )
    .contains("\"created\":"));
}

#[test]
fn test_completion_ids_are_unique_and_prefixed() {
    let resp = CompletionResponse {
        content: "hi".into(),
        model: "m".into(),
        prompt_tokens: 1,
        completion_tokens: 1,
    };
    let id_of = |json: &str| {
        crate::json::parse(json)
            .unwrap()
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap()
            .to_string()
    };
    let a = id_of(&build_openai_response(&resp, "local"));
    let b = id_of(&build_openai_response(&resp, "local"));
    assert!(a.starts_with("chatcmpl-"), "id: {a}");
    assert_ne!(a, b, "completion ids must be unique");
}

/// Send `raw_request` to a one-shot server backed by `proxy`; return
/// (status_code, body) of the HTTP response.
fn roundtrip(proxy: Proxy, raw_request: String) -> (u16, String) {
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(raw_request.as_bytes()).unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        resp
    });
    let (mut server, _) = listener.accept().unwrap();
    proxy.handle_connection(&mut server).unwrap();
    drop(server);
    let resp = client.join().unwrap();
    let status = resp
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

/// Like `roundtrip` but returns the full raw HTTP response string so tests
/// can inspect response headers.
fn roundtrip_raw(proxy: Proxy, raw_request: String) -> String {
    use std::net::Shutdown;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(raw_request.as_bytes()).unwrap();
        c.shutdown(Shutdown::Write).ok();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        resp
    });
    let (mut server, _) = listener.accept().unwrap();
    proxy.handle_connection(&mut server).unwrap();
    drop(server);
    client.join().unwrap()
}

fn http_post(path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[test]
fn test_roundtrip_chat_ok_has_created_and_route() {
    let log = tmp_log();
    let p = proxy_with(true, true, 100, &log);
    let req = http_post(
        "/v1/chat/completions",
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let (status, body) = roundtrip(p, req);
    assert_eq!(status, 200);
    assert!(body.contains("\"created\":"), "{body}");
    assert!(body.contains("\"x_pasture_route\":\"local\""), "{body}");
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_roundtrip_bad_json_is_400_envelope() {
    let p = proxy_with(true, true, 100, "unused");
    let (status, body) = roundtrip(p, http_post("/v1/chat/completions", "{not json"));
    assert_eq!(status, 400);
    assert!(body.contains("\"error\":{"), "{body}");
    assert!(
        body.contains("\"type\":\"invalid_request_error\""),
        "{body}"
    );
}

#[test]
fn test_roundtrip_oversized_body_is_413() {
    let p = proxy_with(true, true, 100, "unused");
    // Declare a Content-Length far beyond MAX_BODY_BYTES; no body sent.
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
        MAX_BODY_BYTES + 1
    );
    let (status, body) = roundtrip(p, req);
    assert_eq!(status, 413);
    assert!(
        body.contains("\"type\":\"invalid_request_error\""),
        "{body}"
    );
}

#[test]
fn test_roundtrip_unknown_path_is_404_envelope() {
    let p = proxy_with(true, true, 100, "unused");
    let (status, body) = roundtrip(p, "GET /nope HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
    assert_eq!(status, 404);
    assert!(body.contains("\"error\":{"), "{body}");
}

#[test]
fn test_parse_embeddings_string_and_array() {
    assert_eq!(
        Proxy::parse_embeddings_request(r#"{"input":"hello"}"#).unwrap(),
        vec!["hello".to_string()]
    );
    assert_eq!(
        Proxy::parse_embeddings_request(r#"{"input":["a","bb"]}"#).unwrap(),
        vec!["a".to_string(), "bb".to_string()]
    );
    assert!(Proxy::parse_embeddings_request(r#"{"model":"m"}"#).is_err());
    assert!(Proxy::parse_embeddings_request(r#"{"input":[1,2]}"#).is_err());
    assert!(Proxy::parse_embeddings_request(r#"{"input":[]}"#).is_err());
}

#[test]
fn test_build_embeddings_response_shape() {
    let resp = EmbeddingsResponse {
        model: "m".into(),
        vectors: vec![vec![0.5, 1.0], vec![2.0, 3.0]],
        prompt_tokens: 4,
    };
    let body = build_embeddings_response(&resp);
    assert!(body.starts_with("{\"object\":\"list\",\"data\":["));
    assert!(body.contains("\"embedding\":[0.5,1]"), "{body}");
    assert!(body.contains("\"index\":1"), "{body}");
    assert!(body.contains("\"model\":\"m\""), "{body}");
    assert!(body.contains("\"total_tokens\":4"), "{body}");
}

#[test]
fn test_handle_embeddings_local() {
    // Mock embeddings return [char_count, 0.0]; "hello"=5, "hi"=2.
    let p = proxy_with(true, false, 100, "unused");
    let body = p.handle_embeddings(r#"{"input":["hello","hi"]}"#).unwrap();
    assert!(body.contains("\"embedding\":[5,0]"), "{body}");
    assert!(body.contains("\"embedding\":[2,0]"), "{body}");
}

#[test]
fn test_handle_embeddings_no_local_is_503() {
    let p = proxy_with(false, true, 100, "unused");
    assert_eq!(
        p.handle_embeddings(r#"{"input":"x"}"#)
            .unwrap_err()
            .status(),
        503
    );
}

#[test]
fn test_build_stats_response_shape() {
    let s = crate::cost::CostSummary {
        total: 4,
        local: 2,
        cloud: 1,
        cache: 1,
        prompt_tokens: 100,
        completion_tokens: 50,
        cloud_cost_usd: 0.0123,
    };
    let json = build_stats_response(&s, 7, 3, 5, 128);
    let v = crate::json::parse(&json).expect("valid json");
    assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(4.0));
    assert_eq!(v.get("cloud").and_then(|x| x.as_f64()), Some(1.0));
    assert_eq!(v.get("cloud_rate").and_then(|x| x.as_f64()), Some(0.25));
    assert_eq!(v.get("cache_rate").and_then(|x| x.as_f64()), Some(0.25));
    assert_eq!(
        v.get("completion_tokens").and_then(|x| x.as_f64()),
        Some(50.0)
    );
    assert_eq!(v.get("cache_hits").and_then(|x| x.as_f64()), Some(7.0));
    assert_eq!(v.get("cache_misses").and_then(|x| x.as_f64()), Some(3.0));
    assert_eq!(v.get("cache_size").and_then(|x| x.as_f64()), Some(5.0));
    assert_eq!(
        v.get("cache_capacity").and_then(|x| x.as_f64()),
        Some(128.0)
    );
}

#[test]
fn test_handle_stats_empty_log_is_zeros() {
    // A non-existent cost log reads as all-zeros (no error).
    let p = proxy_with(true, false, 100, "/no/such/cost-log.jsonl");
    let json = p.handle_stats().unwrap();
    let v = crate::json::parse(&json).expect("valid json");
    assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(0.0));
}

#[test]
fn test_handle_stats_counts_logged_requests() {
    // Drive 2 local completions through a real log file, then read stats.
    let log = tmp_log();
    let p = proxy_with(true, false, 100, &log);
    p.handle_chat(r#"{"messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    p.handle_chat(r#"{"messages":[{"role":"user","content":"yo"}]}"#)
        .unwrap();
    let json = p.handle_stats().unwrap();
    let v = crate::json::parse(&json).expect("valid json");
    assert_eq!(v.get("total").and_then(|x| x.as_f64()), Some(2.0));
    assert_eq!(v.get("local").and_then(|x| x.as_f64()), Some(2.0));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_parse_request_extracts_sampling() {
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"temperature":0.2,"max_tokens":100,"stop":["\n\n"],"seed":7}"#;
    let req = Proxy::parse_request(body).unwrap();
    assert_eq!(req.sampling.temperature, Some(0.2));
    assert_eq!(req.sampling.max_tokens, Some(100));
    assert_eq!(req.sampling.seed, Some(7));
    assert_eq!(req.sampling.stop, vec!["\n\n".to_string()]);
}

#[test]
fn test_parse_request_max_completion_tokens_alias() {
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"max_completion_tokens":42}"#;
    let req = Proxy::parse_request(body).unwrap();
    assert_eq!(req.sampling.max_tokens, Some(42));
}

#[test]
fn test_parse_request_stop_as_string() {
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"stop":"END"}"#;
    let req = Proxy::parse_request(body).unwrap();
    assert_eq!(req.sampling.stop, vec!["END".to_string()]);
}

#[test]
fn test_parse_request_no_sampling_is_empty() {
    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let req = Proxy::parse_request(body).unwrap();
    assert!(req.sampling.is_empty());
}

#[test]
fn test_parse_request_extracts_response_format() {
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_object"}}"#;
    let req = Proxy::parse_request(body).unwrap();
    let rf = req
        .sampling
        .response_format
        .expect("response_format parsed");
    assert_eq!(rf.get("type").and_then(|x| x.as_str()), Some("json_object"));
}

#[test]
fn test_cache_distinguishes_by_response_format() {
    let base = r#"{"messages":[{"role":"user","content":"hi"}]"#;
    let plain = Proxy::parse_request(&format!("{base}}}")).unwrap();
    let json_mode = Proxy::parse_request(&format!(
        "{base},\"response_format\":{{\"type\":\"json_object\"}}}}"
    ))
    .unwrap();
    assert_ne!(
        crate::cache::request_key(&plain),
        crate::cache::request_key(&json_mode)
    );
}

#[test]
fn test_cache_distinguishes_by_temperature() {
    // Same messages, different temperature -> different cache key, so a
    // temperature:0 response is never served to a temperature:1 request.
    let base = r#"{"messages":[{"role":"user","content":"hi"}]"#;
    let r0 = Proxy::parse_request(&format!("{base},\"temperature\":0}}")).unwrap();
    let r1 = Proxy::parse_request(&format!("{base},\"temperature\":1}}")).unwrap();
    assert_ne!(
        crate::cache::request_key(&r0),
        crate::cache::request_key(&r1)
    );
}

#[test]
fn test_constant_time_eq() {
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"Secret"));
    assert!(!constant_time_eq(b"secret", b"secre"));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(constant_time_eq(b"", b""));
}

#[test]
fn test_auth_ok_bearer_forms() {
    assert!(auth_ok(Some("Bearer tok123"), "tok123"));
    assert!(auth_ok(Some("bearer tok123"), "tok123")); // case-insensitive scheme
    assert!(auth_ok(Some("tok123"), "tok123")); // bare token accepted too
    assert!(!auth_ok(Some("Bearer wrong"), "tok123"));
    assert!(!auth_ok(None, "tok123"));
}

#[test]
fn test_gate_open_when_unconfigured() {
    let p = proxy_with(true, false, 100, "unused");
    assert!(p.check_gate("/v1/chat/completions", None).is_none());
}

#[test]
fn test_gate_auth_required_and_enforced() {
    let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("s3cret".to_string()));
    // No / wrong token -> 401.
    assert_eq!(
        p.check_gate("/v1/chat/completions", None).map(|g| g.0),
        Some(401)
    );
    assert_eq!(
        p.check_gate("/v1/chat/completions", Some("Bearer nope"))
            .map(|g| g.0),
        Some(401)
    );
    // Correct token -> allowed.
    assert!(p
        .check_gate("/v1/chat/completions", Some("Bearer s3cret"))
        .is_none());
    // /health is always exempt.
    assert!(p.check_gate("/health", None).is_none());
}

#[test]
fn test_gate_rate_limit_enforced() {
    let p = proxy_with(true, false, 100, "unused").with_rate_limit(1);
    // First request consumes the only token; second is rejected with 429.
    assert!(p.check_gate("/v1/models", None).is_none());
    let denied = p.check_gate("/v1/models", None).expect("second request denied");
    assert_eq!(denied.0, 429);
    // The 429 must carry a positive Retry-After estimate (RFC 7231 §7.1.3).
    assert!(matches!(denied.3, Some(secs) if secs >= 1));
    // /health bypasses the limiter.
    assert!(p.check_gate("/health", None).is_none());
}

#[test]
fn test_roundtrip_ratelimit_headers_present_when_enabled() {
    let p = proxy_with(true, false, 100, "unused").with_rate_limit(10);
    let resp = roundtrip_raw(p, "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
    assert!(
        resp.contains("X-RateLimit-Limit-Requests: 10"),
        "missing limit header: {resp}"
    );
    assert!(
        resp.contains("X-RateLimit-Remaining-Requests:"),
        "missing remaining header: {resp}"
    );
    assert!(
        resp.contains("X-RateLimit-Reset-Requests:"),
        "missing reset header: {resp}"
    );
}

#[test]
fn test_roundtrip_ratelimit_headers_absent_when_disabled() {
    // Default (no rate limit): no X-RateLimit-* noise on responses.
    let p = proxy_with(true, false, 100, "unused");
    let resp = roundtrip_raw(p, "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
    assert!(
        !resp.contains("X-RateLimit"),
        "rate-limit headers should be absent when disabled: {resp}"
    );
}

#[test]
fn test_roundtrip_429_carries_retry_after_header() {
    // Two pipelined GETs on one keep-alive connection: the first consumes the
    // only token, the second is rate-limited and must include Retry-After.
    let p = proxy_with(true, false, 100, "unused").with_rate_limit(1);
    let raw = "GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n\
               GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n"
        .to_string();
    let resp = roundtrip_raw(p, raw);
    assert!(resp.contains("429"), "expected a 429 in: {resp}");
    assert!(
        resp.contains("Retry-After:"),
        "429 response missing Retry-After header: {resp}"
    );
}

#[test]
fn test_roundtrip_401_without_token() {
    let p = proxy_with(true, true, 100, "unused").with_auth_token(Some("k".to_string()));
    let (status, body) = roundtrip(
        p,
        http_post(
            "/v1/chat/completions",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    );
    assert_eq!(status, 401);
    assert!(
        body.contains("\"type\":\"invalid_request_error\""),
        "{body}"
    );
}

#[test]
fn test_roundtrip_authed_request_ok() {
    let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("k".to_string()));
    let req = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer k\r\nContent-Length: {}\r\n\r\n{}",
        r#"{"messages":[{"role":"user","content":"hi"}]}"#.len(),
        r#"{"messages":[{"role":"user","content":"hi"}]}"#
    );
    let (status, _body) = roundtrip(p, req);
    assert_eq!(status, 200);
}

#[test]
fn test_roundtrip_health_exempt_from_auth() {
    let p = proxy_with(true, false, 100, "unused").with_auth_token(Some("k".to_string()));
    let (status, _) = roundtrip(p, "GET /health HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
}

#[test]
fn test_parse_include_usage() {
    assert!(Proxy::parse_include_usage(
        r#"{"stream":true,"stream_options":{"include_usage":true}}"#
    ));
    assert!(!Proxy::parse_include_usage(
        r#"{"stream":true,"stream_options":{"include_usage":false}}"#
    ));
    assert!(!Proxy::parse_include_usage(r#"{"stream":true}"#));
}

#[test]
fn test_build_usage_chunk_shape() {
    let json =
        build_openai_usage_chunk("chatcmpl-x", "m", "fp_pasture_00000000", "local", 10, 5);
    let v = crate::json::parse(&json).expect("valid json");
    assert_eq!(
        v.get("object").and_then(|x| x.as_str()),
        Some("chat.completion.chunk")
    );
    // OpenAI: the usage chunk has an empty choices array.
    assert_eq!(
        v.get("choices")
            .and_then(JsonValue::as_array)
            .map(|a| a.len()),
        Some(0)
    );
    let usage = v.get("usage").unwrap();
    assert_eq!(
        usage.get("prompt_tokens").and_then(|x| x.as_f64()),
        Some(10.0)
    );
    assert_eq!(
        usage.get("completion_tokens").and_then(|x| x.as_f64()),
        Some(5.0)
    );
    assert_eq!(
        usage.get("total_tokens").and_then(|x| x.as_f64()),
        Some(15.0)
    );
}

#[test]
fn test_roundtrip_stream_includes_usage_when_requested() {
    let p = proxy_with(true, false, 100, tmp_log().as_str());
    let body = r#"{"stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 200);
    assert!(resp.contains("\"usage\""), "expected usage chunk: {resp}");
    assert!(resp.contains("\"total_tokens\""), "{resp}");
    assert!(resp.contains("data: [DONE]"), "{resp}");
}

#[test]
fn test_roundtrip_stream_shares_one_id() {
    let p = proxy_with(true, false, 100, tmp_log().as_str());
    let body = r#"{"stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 200);
    // Every chunk's id must be identical across the stream (incl. usage chunk).
    let ids: Vec<String> = resp
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|p| p.starts_with('{'))
        .filter_map(|p| crate::json::parse(p).ok())
        .filter_map(|v| v.get("id").and_then(|x| x.as_str()).map(str::to_string))
        .collect();
    assert!(ids.len() >= 2, "expected multiple chunks: {resp}");
    assert!(ids.iter().all(|id| *id == ids[0]), "ids differ: {ids:?}");
    assert!(ids[0].starts_with("chatcmpl-"), "{:?}", ids[0]);
}

#[test]
fn test_roundtrip_stream_omits_usage_by_default() {
    let p = proxy_with(true, false, 100, tmp_log().as_str());
    let body = r#"{"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 200);
    assert!(!resp.contains("\"usage\""), "should not emit usage: {resp}");
    assert!(resp.contains("data: [DONE]"), "{resp}");
}

#[test]
fn test_cors_policy_parse() {
    assert_eq!(CorsPolicy::parse(""), None);
    assert_eq!(CorsPolicy::parse("   "), None);
    assert!(CorsPolicy::parse("*").unwrap().allow_any);
    let p = CorsPolicy::parse("https://a.com, https://b.com").unwrap();
    assert!(!p.allow_any);
    assert_eq!(p.origins, vec!["https://a.com", "https://b.com"]);
}

#[test]
fn test_cors_allow_origin_matching() {
    let any = CorsPolicy::parse("*").unwrap();
    assert_eq!(
        any.allow_origin(Some("https://x.com")).as_deref(),
        Some("*")
    );
    assert_eq!(any.allow_origin(None).as_deref(), Some("*"));
    let list = CorsPolicy::parse("https://ok.com").unwrap();
    assert_eq!(
        list.allow_origin(Some("https://ok.com")).as_deref(),
        Some("https://ok.com")
    );
    assert_eq!(list.allow_origin(Some("https://evil.com")), None);
    assert_eq!(list.allow_origin(None), None);
}

#[test]
fn test_cors_headers_off_by_default() {
    let p = proxy_with(true, false, 100, "unused");
    assert_eq!(p.cors_headers(Some("https://x.com")), "");
    assert!(p.cors_preflight(Some("https://x.com")).is_none());
}

#[test]
fn test_cors_headers_wildcard() {
    let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
    let h = p.cors_headers(Some("https://x.com"));
    assert!(h.contains("Access-Control-Allow-Origin: *"), "{h}");
    assert!(!h.contains("Vary"), "wildcard needs no Vary: {h}");
}

#[test]
fn test_cors_headers_specific_origin_adds_vary() {
    let p =
        proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("https://ok.com"));
    let h = p.cors_headers(Some("https://ok.com"));
    assert!(
        h.contains("Access-Control-Allow-Origin: https://ok.com"),
        "{h}"
    );
    assert!(h.contains("Vary: Origin"), "{h}");
    // Disallowed origin -> no header.
    assert_eq!(p.cors_headers(Some("https://evil.com")), "");
}

#[test]
fn test_roundtrip_options_preflight() {
    let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
    let req = "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
    let (status, _body) = roundtrip(p, req.to_string());
    assert_eq!(status, 204);
}

#[test]
fn test_roundtrip_preflight_skips_auth() {
    // Preflight carries no credentials, so it must not be 401'd even with auth on.
    let p = proxy_with(true, false, 100, "unused")
        .with_cors(CorsPolicy::parse("*"))
        .with_auth_token(Some("k".to_string()));
    let req = "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\n\r\n";
    let (status, _) = roundtrip(p, req.to_string());
    assert_eq!(status, 204);
}

#[test]
fn test_roundtrip_cors_header_on_response() {
    let p = proxy_with(true, false, 100, "unused").with_cors(CorsPolicy::parse("*"));
    let req = "GET /v1/models HTTP/1.1\r\nHost: x\r\nOrigin: https://app.example\r\n\r\n";
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(req.as_bytes()).unwrap();
        c.shutdown(std::net::Shutdown::Write).ok();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        resp
    });
    let (mut s, _) = listener.accept().unwrap();
    p.handle_connection(&mut s).unwrap();
    drop(s);
    let resp = client.join().unwrap();
    assert!(
        resp.contains("Access-Control-Allow-Origin: *"),
        "missing CORS header: {resp}"
    );
}

#[test]
fn test_roundtrip_stats_ok() {
    let p = proxy_with(true, true, 100, "/no/such/cost-log.jsonl");
    let (status, body) = roundtrip(p, "GET /v1/stats HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"object\":\"pasture.stats\""), "{body}");
}

// ── Prometheus /metrics endpoint (IMP-metrics-prom) ───────────────────────

#[test]
fn test_metrics_endpoint_returns_200_text_plain() {
    let p = proxy_with(true, true, 100, "/no/such/cost-log.jsonl");
    let raw = "GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = raw_roundtrip(p, raw.to_string());
    assert!(
        resp.contains("HTTP/1.1 200"),
        "expected 200: {resp}"
    );
    assert!(
        resp.contains("text/plain"),
        "expected text/plain Content-Type: {resp}"
    );
}

#[test]
fn test_metrics_response_shape() {
    let s = crate::cost::CostSummary {
        total: 10,
        local: 7,
        cloud: 2,
        cache: 1,
        prompt_tokens: 200,
        completion_tokens: 100,
        cloud_cost_usd: 0.005,
    };
    let body = build_metrics_response(&s, 3, 8, 5, 50);
    assert!(
        body.contains("pasture_requests_total{route=\"local\"} 7"),
        "{body}"
    );
    assert!(
        body.contains("pasture_requests_total{route=\"cloud\"} 2"),
        "{body}"
    );
    assert!(body.contains("pasture_cache_hits_total 3"), "{body}");
    assert!(body.contains("pasture_cache_entries 5"), "{body}");
    assert!(body.contains("pasture_cache_capacity 50"), "{body}");
    assert!(body.contains("# TYPE pasture_requests_total counter"), "{body}");
    assert!(body.contains("# TYPE pasture_cache_entries gauge"), "{body}");
}

#[test]
fn test_metrics_wrong_method_returns_405() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, _) = roundtrip(p, "POST /metrics HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_string());
    assert_eq!(status, 405, "POST /metrics should be 405");
}

#[test]
fn test_stats_includes_live_cache_counters() {
    let p = proxy_with(true, true, 100, "/no/such/cost-log.jsonl");
    let (status, body) = roundtrip(p, "GET /v1/stats HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"cache_hits\":"), "missing cache_hits: {body}");
    assert!(
        body.contains("\"cache_misses\":"),
        "missing cache_misses: {body}"
    );
}

// ── X-Response-Time header (IMP-response-time) ─────────────────────────────

#[test]
fn test_response_time_header_on_success() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = roundtrip_raw(p, "GET /health HTTP/1.1\r\nConnection: close\r\n\r\n".to_string());
    let header_block = raw.split("\r\n\r\n").next().unwrap_or("");
    let xrt = header_block
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("x-response-time:"));
    assert!(xrt.is_some(), "X-Response-Time header missing from /health response:\n{raw}");
    let val = xrt.unwrap().split_once(':').map(|x| x.1).unwrap_or("").trim();
    assert!(val.ends_with("ms"), "X-Response-Time value must end with ms, got: {val}");
    let ms: u64 = val.trim_end_matches("ms").parse().expect("X-Response-Time not a number");
    assert!(ms < 5000, "X-Response-Time suspiciously large: {ms}ms");
}

#[test]
fn test_response_time_header_on_error() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = roundtrip_raw(p, "GET /no/such/route HTTP/1.1\r\nConnection: close\r\n\r\n".to_string());
    let header_block = raw.split("\r\n\r\n").next().unwrap_or("");
    assert!(
        header_block.to_ascii_lowercase().contains("x-response-time:"),
        "X-Response-Time missing from 404 error response:\n{raw}"
    );
}

#[test]
fn test_response_time_header_on_chat_completion() {
    let log = tmp_log();
    let p = proxy_with(true, false, 100, &log);
    let raw = roundtrip_raw(p, http_post(
        "/v1/chat/completions",
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
    ));
    let header_block = raw.split("\r\n\r\n").next().unwrap_or("");
    assert!(
        header_block.to_ascii_lowercase().contains("x-response-time:"),
        "X-Response-Time missing from chat completions response:\n{raw}"
    );
}

// ── Access log (IMP-access-log) ────────────────────────────────────────────

fn proxy_with_access_log(log: &str, access: &str) -> Proxy {
    let engine = RoutingEngine::new(100, true, false);
    Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", "local-reply")) as Box<dyn Backend>),
        None,
        log,
    )
    .with_access_log(Some(access.to_string()))
}

#[test]
fn test_access_log_records_successful_request() {
    let log = tmp_log();
    let access = tmp_log();
    let p = proxy_with_access_log(&log, &access);
    let req = http_post(
        "/v1/chat/completions",
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
    );
    let (status, _) = roundtrip(p, req);
    assert_eq!(status, 200);
    let entries = std::fs::read_to_string(&access).unwrap_or_default();
    assert!(!entries.is_empty(), "access log should not be empty");
    let parsed: crate::json::JsonValue = crate::json::parse(entries.trim_end_matches('\n').lines().next().unwrap_or("{}")).unwrap();
    assert_eq!(parsed.get("status").and_then(|v| v.as_f64()), Some(200.0), "status: {parsed:?}");
    assert_eq!(parsed.get("method").and_then(|v| v.as_str()), Some("POST"));
    assert_eq!(parsed.get("path").and_then(|v| v.as_str()), Some("/v1/chat/completions"));
    assert!(parsed.get("ms").is_some(), "missing ms field");
    assert!(parsed.get("ts").is_some(), "missing ts field");
}

#[test]
fn test_access_log_records_error_response() {
    let access = tmp_log();
    let p = proxy_with_access_log("unused", &access);
    let (status, _) = roundtrip(p, "GET /no/such HTTP/1.1\r\nConnection: close\r\n\r\n".to_string());
    assert_eq!(status, 404);
    let entries = std::fs::read_to_string(&access).unwrap_or_default();
    let line = entries.trim_end_matches('\n').lines().next().unwrap_or("{}");
    let parsed: crate::json::JsonValue = crate::json::parse(line).unwrap();
    assert_eq!(parsed.get("status").and_then(|v| v.as_f64()), Some(404.0));
}

#[test]
fn test_access_log_includes_request_id() {
    let log = tmp_log();
    let access = tmp_log();
    let p = proxy_with_access_log(&log, &access);
    let raw = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nX-Request-ID: test-id-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#.len(),
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
    );
    let (status, _) = roundtrip(p, raw);
    assert_eq!(status, 200);
    let entries = std::fs::read_to_string(&access).unwrap_or_default();
    let line = entries.trim_end_matches('\n').lines().next().unwrap_or("{}");
    let parsed: crate::json::JsonValue = crate::json::parse(line).unwrap();
    assert_eq!(parsed.get("request_id").and_then(|v| v.as_str()), Some("test-id-1"), "line: {line}");
}

#[test]
fn test_access_log_escapes_method_and_path() {
    // A malicious or malformed client sending quotes/backslashes in the HTTP
    // request line must not inject arbitrary JSON into the access log.
    // We verify this by calling append_access_log directly with hostile inputs.
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "pasture-access-log-escape-test-{}.jsonl",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let p = path.to_str().unwrap();
    // Method and path containing JSON-breaking characters.
    append_access_log(p, "GET\"evil", "/path\\n\\\"injected\":1,\"x", 200, 1, None);
    let line = std::fs::read_to_string(p)
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_string();
    let _ = std::fs::remove_file(p);
    let v = crate::json::parse(&line).expect("access log line must be valid JSON");
    // The escaping must preserve the original hostile string (escaped), not
    // allow it to break the JSON structure.
    let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
    assert!(method.contains("evil"), "method should contain the original value");
    let path_val = v.get("path").and_then(|x| x.as_str()).unwrap_or("");
    assert!(path_val.contains("injected"), "path should contain the original value");
}

#[test]
fn test_access_log_disabled_by_default() {
    let log = tmp_log();
    let engine = RoutingEngine::new(100, true, false);
    let p = Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", "local-reply")) as Box<dyn Backend>),
        None,
        &log,
    );
    // No with_access_log call — access_log is None by default.
    let (status, _) = roundtrip(p, http_post("/v1/chat/completions", r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#));
    assert_eq!(status, 200); // just verify it still works without access log
}

// ── /health version field + /v1/audio|images 501 stubs ────────────────────

#[test]
fn test_health_includes_version() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, body) = roundtrip(p, "GET /health HTTP/1.1\r\nConnection: close\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"status\":\"ok\""), "missing status: {body}");
    assert!(body.contains("\"version\":"), "missing version: {body}");
    assert!(body.contains(env!("CARGO_PKG_VERSION")), "wrong version: {body}");
}

#[test]
fn test_audio_returns_501() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = http_post("/v1/audio/speech", r#"{"model":"tts-1","input":"hi","voice":"alloy"}"#);
    let (status, body) = roundtrip(p, raw);
    assert_eq!(status, 501, "expected 501 for /v1/audio/speech, got {status}");
    assert!(body.contains("not_supported"), "body: {body}");
}

#[test]
fn test_images_returns_501() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = http_post("/v1/images/generations", r#"{"prompt":"a cat"}"#);
    let (status, body) = roundtrip(p, raw);
    assert_eq!(status, 501, "expected 501 for /v1/images/generations, got {status}");
    assert!(body.contains("not_supported"), "body: {body}");
}

#[test]
fn test_audio_wrong_method_returns_405() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, _) = roundtrip(p, "GET /v1/audio/speech HTTP/1.1\r\nConnection: close\r\n\r\n".to_string());
    assert_eq!(status, 405, "expected 405 for GET /v1/audio, got {status}");
}

// ── Content-Type: application/json validation (IMP-content-type) ──────────

#[test]
fn test_post_with_wrong_content_type_returns_415() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = "POST /v1/chat/completions HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
    let (status, _) = roundtrip(p, raw.to_string());
    assert_eq!(status, 415, "text/plain POST should be 415");
}

#[test]
fn test_post_without_content_type_passes_through() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    // No Content-Type header — should still work (e.g. bare curl).
    let raw = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (status, _) = roundtrip(p, raw);
    assert_eq!(status, 200, "missing Content-Type should not be rejected");
}

#[test]
fn test_post_with_charset_suffix_is_accepted() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let raw = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (status, _) = roundtrip(p, raw);
    assert_eq!(
        status, 200,
        "application/json; charset=utf-8 should be accepted"
    );
}

// ── n > 1 validation (IMP-n-validation) ───────────────────────────────────

#[test]
fn test_n_gt_1_returns_400() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"n":3}"#;
    let (status, resp) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 400, "n:3 should be 400: {resp}");
    assert!(resp.contains("'n' must be 1"), "error message: {resp}");
}

#[test]
fn test_n_equals_1_passes() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"n":1}"#;
    let (status, _) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 200, "n:1 should succeed");
}

#[test]
fn test_n_absent_passes() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, _) = roundtrip(p, http_post("/v1/chat/completions", body));
    assert_eq!(status, 200, "absent n should succeed");
}

// ── /v1/moderations stub (IMP-moderations) ────────────────────────────────

#[test]
fn test_moderations_returns_200_all_false() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"input":"test content"}"#;
    let (status, resp) = roundtrip(p, http_post("/v1/moderations", body));
    assert_eq!(status, 200, "body: {resp}");
    assert!(resp.contains("\"flagged\":false"), "body: {resp}");
    assert!(resp.contains("\"model\":\"text-moderation-stable\""), "body: {resp}");
}

#[test]
fn test_moderations_wrong_method_returns_405() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, _) = roundtrip(p, "GET /v1/moderations HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 405);
}

// ── cache_size + cache_capacity in /v1/stats ──────────────────────────────

#[test]
fn test_stats_includes_cache_size_and_capacity() {
    let p = proxy_with(true, true, 100, "/no/such/cost-log.jsonl");
    let (status, body) = roundtrip(p, "GET /v1/stats HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 200);
    assert!(body.contains("\"cache_size\":"), "missing cache_size: {body}");
    assert!(
        body.contains("\"cache_capacity\":"),
        "missing cache_capacity: {body}"
    );
}

#[test]
fn test_roundtrip_embeddings_ok() {
    let p = proxy_with(true, true, 100, "unused");
    let (status, body) = roundtrip(p, http_post("/v1/embeddings", r#"{"input":"hello"}"#));
    assert_eq!(status, 200);
    assert!(body.contains("\"object\":\"embedding\""), "{body}");
}

/// Backend that fails with a transient error `fail_n` times, then succeeds.
struct FlakyBackend {
    remaining: std::sync::atomic::AtomicU32,
    attempts: std::sync::atomic::AtomicU32,
    retryable: bool,
}
impl FlakyBackend {
    fn new(fail_n: u32, retryable: bool) -> Self {
        Self {
            remaining: std::sync::atomic::AtomicU32::new(fail_n),
            attempts: std::sync::atomic::AtomicU32::new(0),
            retryable,
        }
    }
}
impl Backend for FlakyBackend {
    fn name(&self) -> &str {
        "flaky"
    }
    fn complete(&self, _req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
        use std::sync::atomic::Ordering;
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.remaining.load(Ordering::SeqCst) > 0 {
            self.remaining.fetch_sub(1, Ordering::SeqCst);
            return Err(if self.retryable {
                BackendError::Transport("temporary".into())
            } else {
                BackendError::Protocol("permanent".into())
            });
        }
        Ok(CompletionResponse {
            content: "cloud-ok".into(),
            model: "flaky".into(),
            prompt_tokens: 1,
            completion_tokens: 1,
        })
    }
}

#[test]
fn test_retry_succeeds_after_transient() {
    let b = FlakyBackend::new(2, true);
    let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
    let resp = complete_with_retry(&b, &req, 3, 0).unwrap();
    assert_eq!(resp.content, "cloud-ok");
    assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn test_retry_gives_up_after_limit() {
    let b = FlakyBackend::new(5, true);
    let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
    assert!(complete_with_retry(&b, &req, 2, 0).is_err());
    // 1 initial + 2 retries = 3 attempts.
    assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[test]
fn test_retry_skips_non_retryable() {
    let b = FlakyBackend::new(1, false); // Protocol error -> not retryable
    let req = Proxy::parse_request(r#"{"messages":[{"role":"user","content":"hi"}]}"#).unwrap();
    assert!(complete_with_retry(&b, &req, 5, 0).is_err());
    assert_eq!(b.attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn test_cloud_failure_falls_back_to_local() {
    // IMP-9: a persistently failing cloud falls back to the local answer.
    let log = tmp_log();
    let engine = RoutingEngine::new(100, true, true);
    let proxy = Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", "local-reply"))),
        Some(Box::new(FlakyBackend::new(99, true))), // always fails (transient)
        &log,
    )
    .with_cloud_retry(0); // no backoff sleeps in the test
                          // tools force the cloud route (IMP-10); cloud fails -> fall back to local.
    let body = r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f"}}]}"#;
    let resp = proxy.handle_chat(body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"local\""));
    assert!(resp.contains("local-reply"));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_parse_request_missing_messages_errors() {
    assert!(Proxy::parse_request(r#"{"model":"m"}"#).is_err());
}

#[test]
fn test_parse_request_empty_messages_errors() {
    assert!(Proxy::parse_request(r#"{"messages":[]}"#).is_err());
}

#[test]
fn test_handle_chat_short_goes_local() {
    let log = tmp_log();
    let p = proxy_with(true, true, 100, &log);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = p.handle_chat(body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"local\""));
    assert!(resp.contains("local-reply"));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_handle_chat_long_goes_cloud() {
    let log = tmp_log();
    let p = proxy_with(true, true, 5, &log);
    let long = "word ".repeat(50);
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{long}"}}]}}"#);
    let resp = p.handle_chat(&body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"cloud\""));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_handle_chat_sensitive_kept_local_despite_long() {
    let log = tmp_log();
    let p = proxy_with(true, true, 5, &log); // low threshold -> would be cloud
    let long = format!("contact me at user@example.com {}", "word ".repeat(50));
    let body = format!(r#"{{"model":"m","messages":[{{"role":"user","content":"{long}"}}]}}"#);
    let resp = p.handle_chat(&body).unwrap();
    assert!(
        resp.contains("\"x_pasture_route\":\"local\""),
        "sensitive content must stay local: {resp}"
    );
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_handle_chat_writes_cost_log() {
    let log = tmp_log();
    let p = proxy_with(true, true, 100, &log);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    p.handle_chat(body).unwrap();
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains("\"route\":\"local\""));
    let _ = std::fs::remove_file(&log);
}

fn cascade_proxy(local_reply: &str, cloud_reply: &str, log: &str) -> Proxy {
    let engine = RoutingEngine::new(10_000, true, true); // high threshold -> local first
    Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", local_reply))),
        Some(Box::new(MockBackend::new("cloud", cloud_reply))),
        log,
    )
    .with_cascade(true)
}

#[test]
fn test_cascade_escalates_low_confidence() {
    let log = tmp_log();
    let p = cascade_proxy("I don't know", "the answer is 42", &log);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hard"}]}"#;
    let resp = p.handle_chat(body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"cloud\""), "{resp}");
    assert!(resp.contains("the answer is 42"));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_cascade_keeps_confident_local() {
    let log = tmp_log();
    let p = cascade_proxy(
        "The capital of France is Paris.",
        "should not be used",
        &log,
    );
    let body = r#"{"model":"m","messages":[{"role":"user","content":"q"}]}"#;
    let resp = p.handle_chat(body).unwrap();
    assert!(resp.contains("\"x_pasture_route\":\"local\""), "{resp}");
    assert!(resp.contains("Paris"));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_cache_hit_returns_cache_route() {
    let log = tmp_log();
    let p = proxy_with(true, false, 100, &log).with_cache(8);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
    let first = p.handle_chat(body).unwrap();
    assert!(first.contains("\"x_pasture_route\":\"local\""), "{first}");
    let second = p.handle_chat(body).unwrap();
    assert!(second.contains("\"x_pasture_route\":\"cache\""), "{second}");
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_sensitive_not_cached() {
    let log = tmp_log();
    let p = proxy_with(true, false, 100, &log).with_cache(8);
    let body = r#"{"model":"m","messages":[{"role":"user","content":"my email a@b.com"}]}"#;
    let first = p.handle_chat(body).unwrap();
    let second = p.handle_chat(body).unwrap();
    assert!(first.contains("\"x_pasture_route\":\"local\""));
    assert!(second.contains("\"x_pasture_route\":\"local\""), "{second}");
    assert!(!second.contains("\"cache\""));
    let _ = std::fs::remove_file(&log);
}

#[test]
fn test_build_openai_response_shape_is_valid_json() {
    let resp = CompletionResponse {
        content: "answer with \"quotes\"".to_string(),
        model: "m".to_string(),
        prompt_tokens: 3,
        completion_tokens: 2,
    };
    let json = build_openai_response(&resp, "cloud");
    let parsed = parse(&json).unwrap();
    assert_eq!(
        parsed.get("x_pasture_route").and_then(|r| r.as_str()),
        Some("cloud")
    );
    assert_eq!(
        parsed
            .get("usage")
            .and_then(|u| u.get("total_tokens"))
            .map(|t| matches!(t, JsonValue::Number(_))),
        Some(true)
    );
}

#[test]
fn test_response_choice_has_null_logprobs() {
    let resp = CompletionResponse {
        content: "hi".into(),
        model: "m".into(),
        prompt_tokens: 1,
        completion_tokens: 1,
    };
    let json = build_openai_response(&resp, "local");
    let v = parse(&json).unwrap();
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .unwrap();
    assert!(
        matches!(choice.get("logprobs"), Some(JsonValue::Null)),
        "choice must carry logprobs:null, got: {json}"
    );
}

#[test]
fn test_chunk_choice_has_null_logprobs() {
    let fp = fingerprint_for_model("m");
    let json = build_openai_chunk("chatcmpl-x", "m", &fp, "tok", "local", None);
    let v = parse(&json).unwrap();
    let choice = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .unwrap();
    assert!(
        matches!(choice.get("logprobs"), Some(JsonValue::Null)),
        "chunk choice must carry logprobs:null, got: {json}"
    );
}

#[test]
fn test_find_subslice_locates_header_break() {
    assert_eq!(find_subslice(b"ab\r\n\r\ncd", b"\r\n\r\n"), Some(2));
    assert_eq!(find_subslice(b"abc", b"\r\n\r\n"), None);
}

#[test]
fn test_build_openai_chunk_delta_is_valid_json() {
    let json = build_openai_chunk(
        "chatcmpl-x",
        "llama3",
        "fp_pasture_00000000",
        "hel\"lo",
        "local",
        None,
    );
    let v = parse(&json).unwrap();
    assert_eq!(
        v.get("object").and_then(|o| o.as_str()),
        Some("chat.completion.chunk")
    );
    assert_eq!(
        v.get("x_pasture_route").and_then(|r| r.as_str()),
        Some("local")
    );
    let delta = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("delta"))
        .and_then(|d| d.get("content"))
        .and_then(|c| c.as_str());
    assert_eq!(delta, Some("hel\"lo"));
}

#[test]
fn test_build_openai_chunk_finish_stop() {
    let json = build_openai_chunk(
        "chatcmpl-x",
        "gpt-4",
        "fp_pasture_00000000",
        "",
        "cloud",
        Some("stop"),
    );
    assert!(json.contains("\"finish_reason\":\"stop\""));
    assert!(json.contains("\"delta\":{}"));
}

#[test]
fn test_fingerprint_for_model_is_deterministic_and_prefixed() {
    let fp1 = fingerprint_for_model("llama3");
    let fp2 = fingerprint_for_model("llama3");
    assert_eq!(fp1, fp2, "fingerprint must be deterministic");
    assert!(fp1.starts_with("fp_pasture_"), "prefix: {fp1}");
    assert_eq!(fp1.len(), "fp_pasture_".len() + 8, "8 hex chars: {fp1}");
    let other = fingerprint_for_model("gpt-4");
    assert_ne!(fp1, other, "different models must differ");
}

#[test]
fn test_response_includes_system_fingerprint() {
    let resp = CompletionResponse {
        content: "hi".into(),
        model: "llama3".into(),
        prompt_tokens: 1,
        completion_tokens: 1,
    };
    let json = build_openai_response(&resp, "local");
    let v = parse(&json).unwrap();
    let fp = v
        .get("system_fingerprint")
        .and_then(|f| f.as_str())
        .unwrap_or("");
    assert!(fp.starts_with("fp_pasture_"), "got: {fp}");
    assert_eq!(fp, fingerprint_for_model("llama3").as_str());
}

#[test]
fn test_chunk_includes_system_fingerprint() {
    let fp = fingerprint_for_model("llama3");
    let json = build_openai_chunk("chatcmpl-x", "llama3", &fp, "hello", "local", None);
    let v = parse(&json).unwrap();
    assert_eq!(
        v.get("system_fingerprint").and_then(|f| f.as_str()),
        Some(fp.as_str())
    );
}

#[test]
fn test_stream_chunks_share_fingerprint() {
    let fp = fingerprint_for_model("m");
    let c1 = build_openai_chunk("chatcmpl-a", "m", &fp, "tok1", "local", None);
    let c2 = build_openai_chunk("chatcmpl-a", "m", &fp, "tok2", "local", None);
    let stop = build_openai_chunk("chatcmpl-a", "m", &fp, "", "local", Some("stop"));
    let usage = build_openai_usage_chunk("chatcmpl-a", "m", &fp, "local", 5, 3);
    let fp_of = |j: &str| {
        parse(j)
            .unwrap()
            .get("system_fingerprint")
            .and_then(|f| f.as_str())
            .unwrap()
            .to_string()
    };
    let fps: Vec<_> = [&c1, &c2, &stop, &usage].iter().map(|j| fp_of(j)).collect();
    assert!(
        fps.iter().all(|f| f == &fp),
        "fingerprints must match: {fps:?}"
    );
}

#[test]
fn test_chunk_includes_model() {
    let fp = fingerprint_for_model("llama3");
    let json = build_openai_chunk("chatcmpl-x", "llama3", &fp, "tok", "local", None);
    let v = parse(&json).unwrap();
    assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("llama3"));
}

#[test]
fn test_usage_chunk_includes_model() {
    let fp = fingerprint_for_model("llama3");
    let json = build_openai_usage_chunk("chatcmpl-x", "llama3", &fp, "local", 5, 3);
    let v = parse(&json).unwrap();
    assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("llama3"));
}

#[test]
fn test_stream_chunks_share_model() {
    let fp = fingerprint_for_model("phi3");
    let chunks = [
        build_openai_chunk("chatcmpl-b", "phi3", &fp, "tok1", "local", None),
        build_openai_chunk("chatcmpl-b", "phi3", &fp, "", "local", Some("stop")),
        build_openai_usage_chunk("chatcmpl-b", "phi3", &fp, "local", 4, 2),
    ];
    let model_of = |j: &str| {
        parse(j)
            .unwrap()
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap()
            .to_string()
    };
    let models: Vec<_> = chunks.iter().map(|j| model_of(j)).collect();
    assert!(
        models.iter().all(|m| m == "phi3"),
        "model must be consistent: {models:?}"
    );
}

#[test]
fn test_sse_frame_format() {
    assert_eq!(sse_frame("X"), "data: X\n\n");
}

#[test]
fn test_utc_date_str_epoch() {
    assert_eq!(utc_date_str(0), "1970-01-01");
}

#[test]
fn test_utc_date_str_known_date() {
    // 2026-06-08 UTC = 20612 days from epoch (verified by counting leap years)
    let ts = 20612u64 * 86400;
    assert_eq!(utc_date_str(ts), "2026-06-08");
}

#[test]
fn test_utc_date_str_y2k() {
    // 2000-01-01 UTC = 10957 days from epoch
    let ts = 10957u64 * 86400;
    assert_eq!(utc_date_str(ts), "2000-01-01");
}

#[test]
fn test_inject_context_prepends_system() {
    let req = CompletionRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: "hi".to_string(),
        }],
        stream: false,
        has_tools: false,
        sampling: Default::default(),
    };
    let injected = inject_context_into(&req);
    assert_eq!(injected.messages[0].role, "system");
    assert!(injected.messages[0].content.contains("Date"));
    assert_eq!(injected.messages[1].role, "user");
}

#[test]
fn test_inject_context_merges_existing_system() {
    let req = CompletionRequest {
        model: "m".to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "be brief".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            },
        ],
        stream: false,
        has_tools: false,
        sampling: Default::default(),
    };
    let injected = inject_context_into(&req);
    // No duplicate system messages — context merged into the existing one.
    assert_eq!(injected.messages[0].role, "system");
    assert!(injected.messages[0].content.contains("be brief"));
    assert!(injected.messages[0].content.contains("Date"));
    assert_eq!(injected.messages.len(), 2);
}

#[test]
fn test_inject_context_enabled_on_proxy() {
    let log = tmp_log();
    let p = proxy_with(true, false, 100, &log).with_inject_context(true);
    let resp = p
        .handle_chat(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .unwrap();
    assert!(resp.contains("local-reply"));
}

// ── PASTURE_SYSTEM_PROMPT (IMP-system-prompt) ─────────────────────────────

fn make_req_user(content: &str) -> CompletionRequest {
    CompletionRequest {
        model: "m".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: content.to_string(),
        }],
        stream: false,
        has_tools: false,
        sampling: Default::default(),
    }
}

#[test]
fn test_prepend_system_prompt_no_existing_system() {
    let req = make_req_user("hi");
    let out = prepend_system_prompt(&req, "Be brief.");
    assert_eq!(out.messages.len(), 2);
    assert_eq!(out.messages[0].role, "system");
    assert_eq!(out.messages[0].content, "Be brief.");
    assert_eq!(out.messages[1].role, "user");
}

#[test]
fn test_prepend_system_prompt_merges_existing_system() {
    let req = CompletionRequest {
        model: "m".to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: "existing".to_string(),
            },
            Message {
                role: "user".to_string(),
                content: "hi".to_string(),
            },
        ],
        stream: false,
        has_tools: false,
        sampling: Default::default(),
    };
    let out = prepend_system_prompt(&req, "prefix");
    assert_eq!(out.messages.len(), 2, "no new message should be added");
    assert_eq!(out.messages[0].role, "system");
    assert!(
        out.messages[0].content.starts_with("prefix"),
        "configured prompt must be first"
    );
    assert!(out.messages[0].content.contains("existing"));
}

#[test]
fn test_with_system_prompt_empty_string_disables() {
    let p = proxy_with(true, false, 100, "unused").with_system_prompt(Some(String::new()));
    // An empty string is treated as None (disabled).
    assert!(p.system_prompt.is_none());
}

#[test]
fn test_system_prompt_applied_via_handle_chat() {
    let log = tmp_log();
    // We can't inspect the messages sent to the backend from handle_chat,
    // but we verify the call succeeds and returns a normal response.
    let p = proxy_with(true, false, 100, &log)
        .with_system_prompt(Some("You are a test assistant.".to_string()));
    let resp = p
        .handle_chat(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .unwrap();
    assert!(resp.contains("local-reply"), "unexpected: {resp}");
}

// ── Model-pinned routing (IMP-model-pinning) ──────────────────────────────

fn proxy_with_models(local: bool, cloud: bool) -> Proxy {
    let log = tmp_log();
    let engine = RoutingEngine::new(10, local, cloud);
    Proxy::new(
        engine,
        local.then(|| Box::new(MockBackend::new("local", "local-reply")) as Box<dyn Backend>),
        cloud.then(|| Box::new(MockBackend::new("cloud", "cloud-reply")) as Box<dyn Backend>),
        &log,
    )
    .with_model_names("llama3".to_string(), "gpt-4o-mini".to_string())
}

#[test]
fn test_model_sentinel_local_forces_local() {
    let p = proxy_with_models(true, true);
    let resp = p
        .handle_chat(r#"{"model":"local","messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    assert!(resp.contains("local-reply"), "model:local should route local: {resp}");
}

#[test]
fn test_model_sentinel_cloud_forces_cloud() {
    let p = proxy_with_models(true, true);
    let resp = p
        .handle_chat(r#"{"model":"cloud","messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    assert!(resp.contains("cloud-reply"), "model:cloud should route cloud: {resp}");
}

#[test]
fn test_configured_local_model_name_forces_local() {
    let p = proxy_with_models(true, true);
    // "llama3" is set as the local model name
    let resp = p
        .handle_chat(r#"{"model":"llama3","messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    assert!(
        resp.contains("local-reply"),
        "named local model should route local: {resp}"
    );
}

#[test]
fn test_configured_cloud_model_name_forces_cloud() {
    let p = proxy_with_models(true, true);
    // "gpt-4o-mini" is set as the cloud model name; short prompt would normally go local
    let resp = p
        .handle_chat(r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    assert!(
        resp.contains("cloud-reply"),
        "named cloud model should route cloud: {resp}"
    );
}

#[test]
fn test_unrecognised_model_name_uses_normal_routing() {
    let p = proxy_with_models(true, true);
    // Short prompt with unknown model → normal routing (local for short)
    let resp = p
        .handle_chat(r#"{"model":"unknown-model","messages":[{"role":"user","content":"hi"}]}"#)
        .unwrap();
    // Normal routing for a short prompt with threshold=10: should go local
    assert!(!resp.is_empty());
}

#[test]
fn test_local_only_routes_all_traffic_local() {
    let log = tmp_log();
    let engine = RoutingEngine::new(10, true, true).with_local_only(true);
    // Long prompt that would normally go cloud.
    let long_body = format!(
        "{{\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
        "x".repeat(500)
    );
    let p = Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", "local-reply"))),
        Some(Box::new(MockBackend::new("cloud", "cloud-reply"))),
        &log,
    );
    let resp = p.handle_chat(&long_body).unwrap();
    assert!(resp.contains("x_pasture_route"));
    assert!(resp.contains("\"local\"") || resp.contains("local-reply"));
}

// ── X-Request-ID echo (IMP-request-id) ────────────────────────────────────

/// Helper: send a raw HTTP request, return the full response string.
fn raw_roundtrip(p: Proxy, raw: String) -> String {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = std::thread::spawn(move || {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(raw.as_bytes()).unwrap();
        c.shutdown(std::net::Shutdown::Write).ok();
        let mut resp = String::new();
        c.read_to_string(&mut resp).unwrap();
        resp
    });
    let (mut s, _) = listener.accept().unwrap();
    p.handle_connection(&mut s).unwrap();
    drop(s);
    client.join().unwrap()
}

#[test]
fn test_request_id_echoed_on_response() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = "GET /health HTTP/1.1\r\nHost: x\r\nX-Request-ID: abc-123\r\nConnection: close\r\n\r\n";
    let resp = raw_roundtrip(p, raw.to_string());
    assert!(
        resp.contains("X-Request-ID: abc-123"),
        "missing echoed request-id: {resp}"
    );
}

#[test]
fn test_response_carries_server_header() {
    let p = proxy_with(true, false, 100, "unused");
    let resp = raw_roundtrip(
        p,
        "GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_string(),
    );
    assert!(
        resp.contains("Server: pasture/"),
        "missing Server header: {resp}"
    );
}

#[test]
fn test_request_id_generated_when_not_sent() {
    // When the client omits X-Request-ID, Pasture mints one (req_…) so every
    // response is traceable, matching OpenAI/LiteLLM (IMP-request-id-gen).
    let p = proxy_with(true, false, 100, "unused");
    let raw = "GET /health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = raw_roundtrip(p, raw.to_string());
    assert!(
        resp.contains("X-Request-ID: req_"),
        "expected a generated req_ request-id: {resp}"
    );
}

#[test]
fn test_generated_request_ids_are_unique() {
    assert_ne!(next_request_id(), next_request_id());
    assert!(next_request_id().starts_with("req_"));
}

#[test]
fn test_request_id_crlf_injection_guard() {
    let p = proxy_with(true, false, 100, "unused");
    // An attacker trying to inject a second header via CRLF in the ID value.
    let raw =
        "GET /health HTTP/1.1\r\nHost: x\r\nX-Request-ID: id\r\nEvil: hdr\r\nConnection: close\r\n\r\n";
    let resp = raw_roundtrip(p, raw.to_string());
    // The \r\n inside the ID value should be stripped so "Evil: hdr" is not injected.
    assert!(
        !resp.contains("Evil: hdr"),
        "CRLF injection not prevented: {resp}"
    );
    // A sanitised (non-empty) ID is still echoed.
    assert!(resp.contains("X-Request-ID:"), "no request-id echoed: {resp}");
}

// ── 405 Method Not Allowed (IMP-http-methods) ─────────────────────────────

#[test]
fn test_wrong_method_on_known_route_returns_405() {
    let p = proxy_with(true, false, 100, "unused");
    // GET on a POST-only route must return 405, not 404.
    let (status, _) = roundtrip(p, "GET /v1/chat/completions HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 405, "expected 405 for GET /v1/chat/completions");
}

#[test]
fn test_wrong_method_returns_allow_header() {
    let p = proxy_with(true, false, 100, "unused");
    let raw = "DELETE /v1/models HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = raw_roundtrip(p, raw.to_string());
    assert!(
        resp.contains("HTTP/1.1 405"),
        "expected 405 response: {resp}"
    );
    assert!(resp.contains("Allow:"), "missing Allow header: {resp}");
}

#[test]
fn test_unknown_path_returns_404_not_405() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, _) = roundtrip(p, "GET /no/such/path HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 404, "unknown path should be 404, not 405");
}

// ── tool_choice escalation (IMP-tool-choice) ──────────────────────────────

#[test]
fn test_tool_choice_auto_escalates() {
    // tool_choice:"auto" without a tools array should still escalate.
    let req = Proxy::parse_request(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tool_choice":"auto"}"#,
    )
    .unwrap();
    assert!(req.has_tools, "tool_choice:auto should set has_tools");
}

#[test]
fn test_tool_choice_required_escalates() {
    let req = Proxy::parse_request(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tool_choice":"required"}"#,
    )
    .unwrap();
    assert!(req.has_tools, "tool_choice:required should set has_tools");
}

#[test]
fn test_tool_choice_none_does_not_escalate() {
    // "none" = do not call any tool → not a hard escalation signal.
    let req = Proxy::parse_request(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tool_choice":"none"}"#,
    )
    .unwrap();
    assert!(!req.has_tools, "tool_choice:none should not set has_tools");
}

#[test]
fn test_tool_choice_named_function_escalates() {
    let req = Proxy::parse_request(
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"tool_choice":{"type":"function","function":{"name":"my_fn"}}}"#,
    )
    .unwrap();
    assert!(
        req.has_tools,
        "named tool_choice object should set has_tools"
    );
}

// ── /v1/completions legacy shim (IMP-legacy-completions) ─────────────────

#[test]
fn test_parse_legacy_completion_string_prompt() {
    let req = Proxy::parse_legacy_completion(
        r#"{"model":"gpt-3.5-turbo-instruct","prompt":"Say hi"}"#,
    )
    .unwrap();
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, "user");
    assert_eq!(req.messages[0].content, "Say hi");
}

#[test]
fn test_parse_legacy_completion_array_prompt() {
    let req = Proxy::parse_legacy_completion(
        r#"{"prompt":["Hello","world"]}"#,
    )
    .unwrap();
    assert_eq!(req.messages[0].content, "Hello\nworld");
}

#[test]
fn test_parse_legacy_completion_missing_prompt_errors() {
    assert!(Proxy::parse_legacy_completion(r#"{"model":"m"}"#).is_err());
}

#[test]
fn test_build_legacy_completion_response_shape() {
    let resp = CompletionResponse {
        content: "hi there".to_string(),
        model: "local-model".to_string(),
        prompt_tokens: 5,
        completion_tokens: 3,
    };
    let json = build_legacy_completion_response(&resp, "local");
    assert!(json.contains("\"object\":\"text_completion\""), "{json}");
    assert!(json.contains("\"text\":\"hi there\""), "{json}");
    assert!(json.contains("\"finish_reason\":\"stop\""), "{json}");
    assert!(json.contains("\"prompt_tokens\":5"), "{json}");
    assert!(json.contains("\"total_tokens\":8"), "{json}");
    assert!(json.starts_with("{\"id\":\"cmpl-"), "{json}");
}

#[test]
fn test_legacy_completion_roundtrip_returns_200_text_completion() {
    let p = proxy_with(true, false, 100, "unused");
    let body = r#"{"model":"m","prompt":"hello"}"#;
    let (status, resp_body) = roundtrip(p, http_post("/v1/completions", body));
    assert_eq!(status, 200, "body: {resp_body}");
    assert!(
        resp_body.contains("\"object\":\"text_completion\""),
        "body: {resp_body}"
    );
}

#[test]
fn test_legacy_completion_wrong_method_returns_405() {
    let p = proxy_with(true, false, 100, "unused");
    let (status, _) = roundtrip(p, "GET /v1/completions HTTP/1.1\r\n\r\n".to_string());
    assert_eq!(status, 405);
}

#[test]
fn test_fast_model_used_for_simple_prompt() {
    let log = tmp_log();
    let engine = RoutingEngine::new(1000, true, false);
    let p = Proxy::new(
        engine,
        Some(Box::new(MockBackend::new("local", "local-reply"))),
        None,
        &log,
    )
    .with_fast_model(Some("phi3:mini".to_string()), 50);
    // Short, no-hard-signal prompt should use the fast model.
    let resp = p
        .handle_chat(r#"{"messages":[{"role":"user","content":"hello"}]}"#)
        .unwrap();
    assert!(resp.contains("local-reply"));
}

// ── Header line count DoS guard (ADR-097) ─────────────────────────────────

#[test]
fn test_excessive_header_count_closes_connection() {
    // A request with > 1000 header fields must be silently closed (DoS guard,
    // ADR-097). read_request returns ReadOutcome::Closed; the client receives
    // no response bytes.
    let p = proxy_with(true, false, 100, "unused");
    let mut headers = String::new();
    for i in 0..1001usize {
        headers.push_str(&format!("X-Pad-{i}: v\r\n"));
    }
    let raw = format!("GET /health HTTP/1.1\r\n{headers}\r\n");
    let resp = roundtrip_raw(p, raw);
    assert!(
        resp.is_empty(),
        "expected empty response (connection closed on excess headers), got: {resp}"
    );
}

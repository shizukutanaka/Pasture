//! Cloud backends (ADR-005). The request/response shaping is pure and always
//! compiled & tested. The HTTPS transport is gated behind the `cloud` feature
//! because TLS requires a dependency (native-tls / system OpenSSL), which the
//! default zero-dependency build deliberately omits.
//!
//! BYOK: API keys come from environment variables and are never logged (I5):
//!   OpenAI    -> PASTURE_OPENAI_API_KEY
//!   Anthropic -> PASTURE_ANTHROPIC_API_KEY

use crate::backend::{BackendError, CompletionRequest};
use crate::json::{escape_string, parse, JsonValue};

/// Supported cloud providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
}

impl Provider {
    pub fn from_name(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "openai" => Some(Provider::OpenAI),
            "anthropic" | "claude" => Some(Provider::Anthropic),
            _ => None,
        }
    }

    pub fn host(self) -> &'static str {
        match self {
            Provider::OpenAI => "api.openai.com",
            Provider::Anthropic => "api.anthropic.com",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Provider::OpenAI => "/v1/chat/completions",
            Provider::Anthropic => "/v1/messages",
        }
    }

    pub fn env_key(self) -> &'static str {
        match self {
            Provider::OpenAI => "PASTURE_OPENAI_API_KEY",
            Provider::Anthropic => "PASTURE_ANTHROPIC_API_KEY",
        }
    }

    /// Build the JSON request body for this provider.
    pub fn build_body(self, req: &CompletionRequest) -> String {
        self.build_body_opts(req, false)
    }

    /// Like `build_body` but, when `cache_control` is true, injects an Anthropic
    /// prompt-caching hint (`cache_control: {"type": "ephemeral"}`) into the system
    /// content block (IMP-18). For OpenAI the flag is a no-op (OAI supports prefix
    /// caching automatically; no explicit annotation is required).
    ///
    /// For Anthropic the system message(s) are also separated from the messages
    /// array into the top-level `"system"` field, which is the canonical Anthropic
    /// Messages API form and enables provider-side prefix caching.
    pub fn build_body_opts(self, req: &CompletionRequest, cache_control: bool) -> String {
        match self {
            Provider::OpenAI => {
                let msgs: Vec<String> = req
                    .messages
                    .iter()
                    .map(|m| {
                        // For role:"tool" result messages, tool_call_id is required by
                        // the OpenAI API (ADR-182). Emit it as a top-level field when present.
                        if let Some(tid) = &m.tool_call_id {
                            format!(
                                "{{\"role\":\"{}\",\"tool_call_id\":\"{}\",\"content\":\"{}\"}}",
                                escape_string(&m.role),
                                escape_string(tid),
                                escape_string(&m.content)
                            )
                        } else {
                            format!(
                                "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                                escape_string(&m.role),
                                escape_string(&m.content)
                            )
                        }
                    })
                    .collect();
                format!(
                    "{{\"model\":\"{}\",\"messages\":[{}]{}}}",
                    escape_string(&req.model),
                    msgs.join(","),
                    req.sampling.openai_fields()
                )
            }
            Provider::Anthropic => {
                // Separate system messages from the conversation turns (IMP-18).
                // The Anthropic Messages API takes system as a top-level field;
                // putting it there (rather than as role="system" in the array)
                // is the recommended form and is required for prompt caching.
                let mut system_parts: Vec<&str> = Vec::new();
                let mut conv_msgs: Vec<String> = Vec::new();
                for m in &req.messages {
                    if m.role == "system" {
                        system_parts.push(m.content.as_str());
                    } else if m.role == "tool" {
                        // Translate OpenAI role:"tool" result messages to Anthropic's
                        // tool_result format (ADR-182). Anthropic requires these to be
                        // wrapped as role:"user" with a tool_result content block.
                        let tool_use_id = m
                            .tool_call_id
                            .as_deref()
                            .unwrap_or(""); // id required; omitting is better than a 400
                        conv_msgs.push(format!(
                            "{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"{}\",\"content\":\"{}\"}}]}}",
                            escape_string(tool_use_id),
                            escape_string(&m.content)
                        ));
                    } else {
                        conv_msgs.push(format!(
                            "{{\"role\":\"{}\",\"content\":\"{}\"}}",
                            escape_string(&m.role),
                            escape_string(&m.content)
                        ));
                    }
                }

                // Build the "system" JSON fragment (omitted when there are no
                // system messages so the schema stays minimal).
                let system_json = if !system_parts.is_empty() {
                    let combined = system_parts.join("\n\n");
                    if cache_control {
                        // Structured content block with cache hint (IMP-18).
                        format!(
                            ",\"system\":[{{\"type\":\"text\",\"text\":\"{}\",\"cache_control\":{{\"type\":\"ephemeral\"}}}}]",
                            escape_string(&combined)
                        )
                    } else {
                        // Plain string form (Anthropic accepts both).
                        format!(",\"system\":\"{}\"", escape_string(&combined))
                    }
                } else {
                    String::new()
                };

                // Anthropic requires max_tokens; honour the client's value or
                // fall back to 1024. temperature/top_p/stop_sequences map across.
                let max_tokens = req.sampling.max_tokens.unwrap_or(1024);
                let mut extra = String::new();
                if let Some(t) = req.sampling.temperature {
                    extra.push_str(&format!(",\"temperature\":{t}"));
                }
                if let Some(p) = req.sampling.top_p {
                    extra.push_str(&format!(",\"top_p\":{p}"));
                }
                if !req.sampling.stop.is_empty() {
                    let items: Vec<String> = req
                        .sampling
                        .stop
                        .iter()
                        .map(|s| format!("\"{}\"", escape_string(s)))
                        .collect();
                    extra.push_str(&format!(",\"stop_sequences\":[{}]", items.join(",")));
                }
                // Translate OpenAI tool definitions to Anthropic format (ADR-179).
                if let Some(tools) = &req.sampling.tools {
                    extra.push_str(&translate_tools_to_anthropic(tools));
                }
                // Translate OpenAI tool_choice to Anthropic's enum (ADR-180).
                // Only emitted when tools are present (Anthropic requires both or neither).
                if req.sampling.tools.is_some() {
                    if let Some(tc) = &req.sampling.tool_choice {
                        extra.push_str(&translate_tool_choice_to_anthropic(tc));
                    }
                }
                format!(
                    "{{\"model\":\"{}\",\"max_tokens\":{max_tokens}{},\"messages\":[{}]{}}}",
                    escape_string(&req.model),
                    system_json,
                    conv_msgs.join(","),
                    extra
                )
            }
        }
    }

    /// Like `build_body` but with `"stream": true` for SSE streaming.
    /// For OpenAI, also requests a final usage chunk via `stream_options.include_usage`
    /// (ADR-173). For Anthropic, that field is invalid — usage arrives naturally in
    /// `message_delta`; adding it would cause a 400 error (ADR-179).
    pub fn build_body_stream(self, req: &CompletionRequest) -> String {
        let mut s = self.build_body(req);
        debug_assert!(s.ends_with('}'));
        s.pop();
        match self {
            Provider::OpenAI => {
                s.push_str(",\"stream\":true,\"stream_options\":{\"include_usage\":true}}");
            }
            Provider::Anthropic => {
                s.push_str(",\"stream\":true}");
            }
        }
        s
    }

    /// Like `build_body` but requesting per-token log-probabilities (OpenAI
    /// `logprobs`), used to score local-answer confidence for the cascade.
    pub fn build_body_logprobs(self, req: &CompletionRequest) -> String {
        let mut s = self.build_body(req);
        debug_assert!(s.ends_with('}'));
        s.pop();
        s.push_str(",\"logprobs\":true}");
        s
    }

    /// HTTP headers (name, value) for an authenticated request.
    pub fn headers(self, api_key: &str) -> Vec<(String, String)> {
        match self {
            Provider::OpenAI => vec![("Authorization".into(), format!("Bearer {api_key}"))],
            Provider::Anthropic => vec![
                ("x-api-key".into(), api_key.to_string()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ],
        }
    }

    /// Extract (content, tool_calls, prompt_tokens, completion_tokens) from a
    /// success body. `tool_calls` is the raw JSON array string when the model
    /// chose to call a tool (ADR-177); `None` for an ordinary text completion.
    pub fn parse_response(
        self,
        body: &str,
    ) -> Result<(String, Option<String>, u64, u64), BackendError> {
        let v = parse(body).map_err(|e| BackendError::Protocol(e.to_string()))?;
        // Surface server-reported errors (e.g. LM Studio returns an error object
        // with a 500 when the model id does not match a loaded model).
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err.as_str())
                .unwrap_or("unknown error");
            return Err(BackendError::Protocol(format!("server error: {msg}")));
        }
        match self {
            Provider::OpenAI => {
                let message = v
                    .get("choices")
                    .and_then(JsonValue::as_array)
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"));
                // A tool-call response has `content: null` and a `tool_calls`
                // array, so content alone cannot be required (ADR-177).
                let tool_calls = message
                    .and_then(|m| m.get("tool_calls"))
                    .filter(|tc| matches!(tc, JsonValue::Array(a) if !a.is_empty()))
                    .map(|tc| tc.to_json_string());
                let content = message
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                // Reject only when there is neither text nor a tool call — a
                // genuinely malformed response.
                if content.is_empty() && tool_calls.is_none() {
                    return Err(BackendError::Protocol(
                        "missing choices[0].message.content and tool_calls".into(),
                    ));
                }
                let (p, c) = usage(&v, "prompt_tokens", "completion_tokens");
                Ok((content, tool_calls, p, c))
            }
            Provider::Anthropic => {
                // Walk the content array; collect text blocks and tool_use blocks
                // separately (ADR-179). A tool-use response may have no text block.
                let content_arr = v
                    .get("content")
                    .and_then(JsonValue::as_array)
                    .ok_or_else(|| BackendError::Protocol("missing content array".into()))?;
                let mut text_parts: Vec<&str> = Vec::new();
                let mut tool_items: Vec<String> = Vec::new();
                for block in content_arr {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(t);
                            }
                        }
                        Some("tool_use") => {
                            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                            // Anthropic input is a parsed object; re-serialise to a
                            // JSON string for OpenAI's arguments field (ADR-179).
                            let args = block
                                .get("input")
                                .map(|i| i.to_json_string())
                                .unwrap_or_else(|| "{}".to_string());
                            tool_items.push(format!(
                                "{{\"id\":\"{}\",\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"arguments\":\"{}\"}}}}",
                                escape_string(id),
                                escape_string(name),
                                escape_string(&args)
                            ));
                        }
                        _ => {}
                    }
                }
                let content = text_parts.join("");
                let tool_calls = if tool_items.is_empty() {
                    None
                } else {
                    Some(format!("[{}]", tool_items.join(",")))
                };
                if content.is_empty() && tool_calls.is_none() {
                    return Err(BackendError::Protocol(
                        "missing content[0].text and no tool_use".into(),
                    ));
                }
                let (p, c) = usage(&v, "input_tokens", "output_tokens");
                Ok((content, tool_calls, p, c))
            }
        }
    }
}

/// A parsed SSE line from an OpenAI-style streaming response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenAiStreamEvent {
    Delta(String),
    Done,
    /// Token usage from the final usage chunk (ADR-173).
    /// Present only when `stream_options.include_usage: true` was requested
    /// and the backend sent a usage-only chunk (`choices: []`).
    Usage(u64, u64),
    /// A `delta.tool_calls` fragment (raw JSON array string), streamed when the
    /// model is making a tool call (ADR-178). Accumulated across chunks by the
    /// caller into a complete `tool_calls` array.
    ToolCallDelta(String),
}

/// Mean per-token log-probability from an OpenAI chat-completions response
/// (`choices[0].logprobs.content[].logprob`). `None` if absent or empty.
/// A higher (closer to 0) value means the model was more confident.
pub fn mean_logprob_from_openai(body: &str) -> Option<f64> {
    let v = parse(body).ok()?;
    let tokens = v
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("logprobs"))
        .and_then(|l| l.get("content"))
        .and_then(JsonValue::as_array)?;
    let lps: Vec<f64> = tokens
        .iter()
        .filter_map(|t| t.get("logprob").and_then(|x| x.as_f64()))
        .collect();
    if lps.is_empty() {
        return None;
    }
    Some(lps.iter().sum::<f64>() / lps.len() as f64)
}

/// Parse one SSE `data:` line from an OpenAI-compatible streaming response.
pub fn parse_openai_stream_line(line: &str) -> Option<OpenAiStreamEvent> {
    let data = line.trim().strip_prefix("data:")?.trim();
    if data == "[DONE]" {
        return Some(OpenAiStreamEvent::Done);
    }
    let v = parse(data).ok()?;
    // Usage-only chunk: `{"choices":[],"usage":{"prompt_tokens":N,"completion_tokens":M}}`
    // Sent by OpenAI-compatible backends when stream_options.include_usage is true (ADR-173).
    if let Some(choices) = v.get("choices").and_then(JsonValue::as_array) {
        if choices.is_empty() {
            let (p, c) = usage(&v, "prompt_tokens", "completion_tokens");
            if p > 0 || c > 0 {
                return Some(OpenAiStreamEvent::Usage(p, c));
            }
            return None;
        }
    }
    let delta = v
        .get("choices")
        .and_then(JsonValue::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("delta"));
    // A tool-call fragment: `delta.tool_calls` is a (partial) array (ADR-178).
    if let Some(tc) = delta
        .and_then(|d| d.get("tool_calls"))
        .filter(|tc| matches!(tc, JsonValue::Array(a) if !a.is_empty()))
    {
        return Some(OpenAiStreamEvent::ToolCallDelta(tc.to_json_string()));
    }
    let content = delta.and_then(|d| d.get("content")).and_then(|c| c.as_str());
    match content {
        Some(c) if !c.is_empty() => Some(OpenAiStreamEvent::Delta(c.to_string())),
        _ => None,
    }
}

/// Accumulates OpenAI streamed `tool_calls` delta fragments into a complete
/// `tool_calls` array (ADR-178). Fragments are merged by `index`: `id`, `type`,
/// and `function.name` are set from the first fragment that carries them;
/// `function.arguments` strings are concatenated across fragments.
#[derive(Default)]
pub struct ToolCallAccumulator {
    slots: Vec<ToolCallSlot>,
}

#[derive(Default)]
struct ToolCallSlot {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    /// Ingest one `delta.tool_calls` array (the raw JSON string from a
    /// `ToolCallDelta` event).
    pub fn push(&mut self, delta_json: &str) {
        let Ok(arr) = parse(delta_json) else { return };
        let Some(items) = arr.as_array() else { return };
        for (pos, item) in items.iter().enumerate() {
            let idx = item
                .get("index")
                .and_then(|v| match v {
                    JsonValue::Number(n) => Some(*n as usize),
                    _ => None,
                })
                .unwrap_or(pos);
            while self.slots.len() <= idx {
                self.slots.push(ToolCallSlot::default());
            }
            let slot = &mut self.slots[idx];
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    slot.id = id.to_string();
                }
            }
            if let Some(func) = item.get("function") {
                if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                    if !name.is_empty() {
                        slot.name = name.to_string();
                    }
                }
                if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                    slot.arguments.push_str(args);
                }
            }
        }
    }

    /// Build the complete `tool_calls` array JSON, or `None` if nothing was
    /// accumulated. Each call is `{"id","type":"function","function":{"name","arguments"}}`.
    pub fn finish(self) -> Option<String> {
        if self.slots.is_empty() {
            return None;
        }
        let calls: Vec<String> = self
            .slots
            .into_iter()
            .map(|s| {
                format!(
                    "{{\"id\":\"{}\",\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"arguments\":\"{}\"}}}}",
                    escape_string(&s.id),
                    escape_string(&s.name),
                    escape_string(&s.arguments)
                )
            })
            .collect();
        Some(format!("[{}]", calls.join(",")))
    }
}

/// Parse one SSE `data:` line from an Anthropic Messages streaming response.
/// Text arrives as `content_block_delta` events with `delta.type="text_delta"`;
/// `message_stop` signals the end. Token usage is split across two events
/// (ADR-175): `message_start` carries `message.usage.input_tokens`, and the
/// final `message_delta` carries `usage.output_tokens`.
///
/// Tool-use streaming (ADR-179): `content_block_start` with
/// `content_block.type="tool_use"` carries the call id and name; subsequent
/// `content_block_delta` events with `delta.type="input_json_delta"` carry
/// `partial_json` fragments. Both are emitted as `ToolCallDelta` events
/// (OpenAI-shaped index-keyed delta arrays) so `ToolCallAccumulator` can
/// assemble them provider-agnostically.
pub fn parse_anthropic_stream_line(line: &str) -> Option<OpenAiStreamEvent> {
    let data = line.trim().strip_prefix("data:")?.trim();
    let v = parse(data).ok()?;
    match v.get("type").and_then(|t| t.as_str()) {
        // content_block_start with tool_use → emit ToolCallDelta with id + name
        // so ToolCallAccumulator seeds the slot (ADR-179).
        Some("content_block_start") => {
            let block = v.get("content_block")?;
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                return None; // text blocks have no delta-level signal here
            }
            let idx = v.get("index").and_then(json_u64).unwrap_or(0);
            let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let frag = format!(
                "[{{\"index\":{idx},\"id\":\"{}\",\"type\":\"function\",\"function\":{{\"name\":\"{}\",\"arguments\":\"\"}}}}]",
                escape_string(id),
                escape_string(name)
            );
            Some(OpenAiStreamEvent::ToolCallDelta(frag))
        }
        Some("content_block_delta") => {
            let delta = v.get("delta")?;
            let idx = v.get("index").and_then(json_u64).unwrap_or(0);
            match delta.get("type").and_then(|t| t.as_str()) {
                Some("text_delta") => delta
                    .get("text")
                    .and_then(|t| t.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| OpenAiStreamEvent::Delta(s.to_string())),
                // input_json_delta: partial_json is a raw JSON fragment; escape it
                // into an arguments string delta for ToolCallAccumulator (ADR-179).
                Some("input_json_delta") => {
                    let partial = delta.get("partial_json").and_then(|v| v.as_str()).unwrap_or("");
                    let frag = format!(
                        "[{{\"index\":{idx},\"function\":{{\"arguments\":\"{}\"}}}}]",
                        escape_string(partial)
                    );
                    Some(OpenAiStreamEvent::ToolCallDelta(frag))
                }
                _ => None,
            }
        }
        // message_start.message.usage.input_tokens — prompt tokens (output is a
        // placeholder here, filled by the later message_delta).
        Some("message_start") => {
            let input = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(json_u64);
            input.map(|p| OpenAiStreamEvent::Usage(p, 0))
        }
        // message_delta.usage.output_tokens — final cumulative completion tokens.
        Some("message_delta") => {
            let output = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(json_u64);
            output.map(|c| OpenAiStreamEvent::Usage(0, c))
        }
        Some("message_stop") => Some(OpenAiStreamEvent::Done),
        _ => None,
    }
}

/// Read a JSON number as u64 (truncating), or None for non-numbers.
fn json_u64(v: &JsonValue) -> Option<u64> {
    match v {
        JsonValue::Number(f) => Some(*f as u64),
        _ => None,
    }
}

impl Provider {
    /// Parse an SSE line in this provider's streaming dialect.
    pub fn parse_stream_line(self, line: &str) -> Option<OpenAiStreamEvent> {
        match self {
            Provider::OpenAI => parse_openai_stream_line(line),
            Provider::Anthropic => parse_anthropic_stream_line(line),
        }
    }
}

/// Map a non-2xx HTTP status to a backend error. A 5xx is a server-side,
/// typically transient failure → `Transport` (retryable, IMP-9); other codes
/// (e.g. 4xx auth/bad-request) are not retryable → `Protocol`.
pub fn http_status_error(status: u16, message: String) -> BackendError {
    if (500..600).contains(&status) {
        BackendError::Transport(message)
    } else {
        BackendError::Protocol(message)
    }
}

/// Read an HTTP SSE response from `reader`, invoking `on_delta` for each text
/// delta as it arrives, and return the assembled content. Provider-agnostic and
/// transport-agnostic (works over any `Read`, so the same logic is unit-tested
/// offline and reused over the real TLS stream). Chunked-transfer size lines are
/// ignored because they never start with `data:`; this assumes one SSE event per
/// chunk, which OpenAI and Anthropic both honour.
/// The assembled content, optional usage, and any accumulated `tool_calls`
/// array from a streamed SSE response (ADR-173/178).
#[derive(Debug)]
pub struct SseStreamResult {
    pub content: String,
    pub usage: Option<(u64, u64)>,
    pub tool_calls: Option<String>,
}

/// Read an HTTP SSE response from `reader`, invoking `on_delta` for each text
/// delta as it arrives, and return the assembled content, any usage reported in
/// the final usage chunk (ADR-173), and any streamed `tool_calls` (ADR-178).
/// Provider-agnostic and transport-agnostic.
pub fn read_sse_body<R: std::io::Read>(
    reader: &mut R,
    provider: Provider,
    on_delta: &mut dyn FnMut(&str),
) -> Result<SseStreamResult, BackendError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 1024];
    // Read until the headers terminator.
    let header_end = loop {
        if let Some(p) = find_crlf2(&buf) {
            break p;
        }
        let n = reader
            .read(&mut chunk)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if n == 0 {
            return Err(BackendError::Protocol("stream ended before headers".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 65_536 {
            return Err(BackendError::Protocol("oversized response headers".into()));
        }
    };
    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let status = header_text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    let pending = buf[header_end + 4..].to_vec();
    if !(200..300).contains(&status) {
        let mut rest = pending;
        let _ = reader.read_to_end(&mut rest);
        let snippet: String = String::from_utf8_lossy(&rest).chars().take(200).collect();
        return Err(http_status_error(
            status,
            format!("HTTP {status}: {snippet}"),
        ));
    }

    let mut content = String::new();
    let mut line_buf: Vec<u8> = Vec::new();
    let mut stream_usage: Option<(u64, u64)> = None;
    let mut tool_acc = ToolCallAccumulator::default();
    emit_sse_lines(provider, &mut line_buf, &pending, &mut content, on_delta, &mut stream_usage, &mut tool_acc);
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        emit_sse_lines(provider, &mut line_buf, &chunk[..n], &mut content, on_delta, &mut stream_usage, &mut tool_acc);
    }
    let tool_calls = tool_acc.finish();
    // A pure tool-call stream has empty content but a tool_calls array, so the
    // emptiness check must consider both (ADR-178).
    if content.is_empty() && tool_calls.is_none() {
        return Err(BackendError::Protocol("empty stream".into()));
    }
    Ok(SseStreamResult {
        content,
        usage: stream_usage,
        tool_calls,
    })
}

fn emit_sse_lines(
    provider: Provider,
    line_buf: &mut Vec<u8>,
    incoming: &[u8],
    content: &mut String,
    on_delta: &mut dyn FnMut(&str),
    stream_usage: &mut Option<(u64, u64)>,
    tool_acc: &mut ToolCallAccumulator,
) {
    line_buf.extend_from_slice(incoming);
    while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = line_buf.drain(..=pos).collect();
        let s = String::from_utf8_lossy(&line);
        match provider.parse_stream_line(s.trim()) {
            Some(OpenAiStreamEvent::Delta(d)) => {
                content.push_str(&d);
                on_delta(&d);
            }
            Some(OpenAiStreamEvent::Usage(p, c)) => {
                // Merge partial usage: a non-zero field overwrites. OpenAI sends
                // both counts in one event; Anthropic splits them across
                // message_start (input) and message_delta (output) (ADR-175).
                let (ep, ec) = stream_usage.unwrap_or((0, 0));
                *stream_usage = Some((if p > 0 { p } else { ep }, if c > 0 { c } else { ec }));
            }
            Some(OpenAiStreamEvent::ToolCallDelta(frag)) => {
                tool_acc.push(&frag); // accumulate streamed tool_calls (ADR-178)
            }
            _ => {}
        }
    }
}

fn find_crlf2(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Translate an OpenAI `tool_choice` value to Anthropic's `tool_choice` object (ADR-180).
/// OpenAI → Anthropic:
///   `"auto"`                                         → `{"type":"auto"}`
///   `"required"`                                     → `{"type":"any"}`
///   `{"type":"function","function":{"name":"f"}}`   → `{"type":"tool","name":"f"}`
///   `"none"` and `null` are handled upstream (not forwarded); absent value → omit field.
/// Returns a comma-prefixed `,"tool_choice":{...}` fragment or empty string.
fn translate_tool_choice_to_anthropic(tc: &JsonValue) -> String {
    match tc {
        JsonValue::Str(s) => match s.as_str() {
            "auto" => ",\"tool_choice\":{\"type\":\"auto\"}".to_string(),
            "required" => ",\"tool_choice\":{\"type\":\"any\"}".to_string(),
            _ => String::new(), // unknown string → omit (default auto)
        },
        JsonValue::Object(_) => {
            // OpenAI named-function: {"type":"function","function":{"name":"f"}}
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if name.is_empty() {
                return String::new();
            }
            format!(",\"tool_choice\":{{\"type\":\"tool\",\"name\":\"{}\"}}", escape_string(name))
        }
        _ => String::new(),
    }
}

/// Translate an OpenAI-format `tools` array to Anthropic's schema (ADR-179).
/// OpenAI: `[{"type":"function","function":{"name","description","parameters":{...}}}]`
/// Anthropic: `[{"name","description","input_schema":{...}}]`
/// Returns a comma-prefixed `,"tools":[...]` fragment or empty if no valid tools.
fn translate_tools_to_anthropic(tools: &JsonValue) -> String {
    let Some(arr) = tools.as_array() else {
        return String::new();
    };
    let items: Vec<String> = arr
        .iter()
        .filter_map(|t| {
            let func = t.get("function")?;
            let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = func.get("description").and_then(|v| v.as_str()).unwrap_or("");
            // OpenAI `parameters` → Anthropic `input_schema` (same JSON Schema object).
            let schema_json = func
                .get("parameters")
                .map(|s| s.to_json_string())
                .unwrap_or_else(|| "{\"type\":\"object\"}".to_string());
            Some(format!(
                "{{\"name\":\"{}\",\"description\":\"{}\",\"input_schema\":{}}}",
                escape_string(name),
                escape_string(desc),
                schema_json
            ))
        })
        .collect();
    if items.is_empty() {
        return String::new();
    }
    format!(",\"tools\":[{}]", items.join(","))
}

fn usage(v: &JsonValue, pk: &str, ck: &str) -> (u64, u64) {
    let u = v.get("usage");
    let get = |key: &str| {
        u.and_then(|x| x.get(key))
            .and_then(|n| match n {
                JsonValue::Number(f) => Some(*f as u64),
                _ => None,
            })
            .unwrap_or(0)
    };
    (get(pk), get(ck))
}

/// Parse a raw HTTP response into (status, body), handling Content-Length and
/// chunked transfer encoding. Pure and unit-tested.
pub fn parse_http_response(raw: &str) -> Result<(u16, String), BackendError> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| BackendError::Protocol("no header separator".into()))?;
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| BackendError::Protocol("bad status line".into()))?;
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(body)?
    } else {
        body.to_string()
    };
    Ok((status, body))
}

/// Decode a chunked transfer-encoding body (RFC 7230 §4.1).
/// Returns Err if a declared chunk size exceeds available bytes — that means
/// the response was truncated in transit and the data would be silently wrong.
fn dechunk(body: &str) -> Result<String, BackendError> {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size_line, after)) = rest.split_once("\r\n") else {
            break;
        };
        let size =
            usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0").trim(), 16)
                .unwrap_or(0);
        if size == 0 {
            break;
        }
        // ADR-107: truncated chunk → protocol error, not silent data loss.
        if after.len() < size {
            return Err(BackendError::Protocol(format!(
                "chunked body truncated: declared {size} bytes, got {}",
                after.len()
            )));
        }
        out.push_str(&after[..size]);
        // Skip the chunk data and its trailing CRLF.
        rest = after.get(size + 2..).unwrap_or("");
    }
    Ok(out)
}

/// Resolve the API key for a provider from the environment. The value is
/// trimmed so keys set via `export KEY=$(cat file)` or shell substitution
/// (which often append a trailing newline) still work correctly.
pub fn api_key_from_env(provider: Provider) -> Option<String> {
    std::env::var(provider.env_key())
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

#[cfg(feature = "cloud")]
pub use transport::HttpsCloudBackend;

#[cfg(feature = "cloud")]
mod transport {
    use super::*;
    use crate::backend::{Backend, CompletionResponse};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    /// Cloud backend that performs real HTTPS requests (feature = "cloud").
    pub struct HttpsCloudBackend {
        provider: Provider,
        api_key: String,
        model: String,
        /// When true, inject Anthropic prompt-cache hints (IMP-18).
        cache_control: bool,
    }

    impl HttpsCloudBackend {
        pub fn new(provider: Provider, api_key: &str, model: &str) -> Self {
            Self {
                provider,
                api_key: api_key.to_string(),
                model: model.to_string(),
                cache_control: false,
            }
        }

        /// Build from environment; returns None if the API key is unset.
        pub fn from_env(provider: Provider, model: &str) -> Option<Self> {
            api_key_from_env(provider).map(|k| Self::new(provider, &k, model))
        }

        /// Enable Anthropic prompt-cache hints (IMP-18).
        pub fn with_cache_control(mut self, enabled: bool) -> Self {
            self.cache_control = enabled;
            self
        }
    }

    impl Backend for HttpsCloudBackend {
        fn name(&self) -> &str {
            "cloud"
        }

        fn complete(&self, req: &CompletionRequest) -> Result<CompletionResponse, BackendError> {
            let host = self.provider.host();
            let path = self.provider.path();
            let body = self.provider.build_body_opts(req, self.cache_control);
            let mut header_lines = String::new();
            for (k, v) in self.provider.headers(&self.api_key) {
                header_lines.push_str(&format!("{k}: {v}\r\n"));
            }
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{header_lines}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let raw = https_exchange(host, request.as_bytes())?;
            let (status, resp_body) = parse_http_response(&raw)?;
            if !(200..300).contains(&status) {
                let snippet: String = resp_body.chars().take(200).collect();
                return Err(http_status_error(
                    status,
                    format!("HTTP {status}: {snippet}"),
                ));
            }
            let (content, tool_calls, prompt_tokens, completion_tokens) =
                self.provider.parse_response(&resp_body)?;
            Ok(CompletionResponse {
                content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
                tool_calls,
            })
        }

        fn stream_complete(
            &self,
            req: &CompletionRequest,
            on_delta: &mut dyn FnMut(&str),
        ) -> Result<CompletionResponse, BackendError> {
            let host = self.provider.host();
            let path = self.provider.path();
            // build_body_opts + stream flag; OpenAI also gets stream_options so
            // the backend sends a final usage chunk (ADR-173); Anthropic already
            // sends usage in message_delta and rejects stream_options (ADR-179).
            // cache_control hints apply here too (IMP-18).
            let mut body = self.provider.build_body_opts(req, self.cache_control);
            debug_assert!(body.ends_with('}'));
            body.pop();
            match self.provider {
                Provider::OpenAI => {
                    body.push_str(",\"stream\":true,\"stream_options\":{\"include_usage\":true}}");
                }
                Provider::Anthropic => {
                    body.push_str(",\"stream\":true}");
                }
            }
            let body = body;
            let mut header_lines = String::new();
            for (k, v) in self.provider.headers(&self.api_key) {
                header_lines.push_str(&format!("{k}: {v}\r\n"));
            }
            let request = format!(
                "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{header_lines}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = tls_send(host, request.as_bytes())?;
            let result = read_sse_body(&mut stream, self.provider, on_delta)?;
            // Use actual usage from the stream; fall back to estimate only if
            // the backend did not send a usage chunk (ADR-173).
            let (prompt_tokens, completion_tokens) = result.usage.unwrap_or_else(|| (
                crate::routing::estimate_tokens(&req.routing_text()) as u64,
                crate::routing::estimate_tokens(&result.content) as u64,
            ));
            Ok(CompletionResponse {
                content: result.content,
                model: self.model.clone(),
                prompt_tokens,
                completion_tokens,
                // Streamed tool_calls accumulated by read_sse_body (ADR-178).
                tool_calls: result.tool_calls,
            })
        }
    }

    /// Open a TLS connection to `host:443` and send `request`, returning the
    /// stream positioned to read the response. Shared by the buffered and
    /// streaming HTTPS paths so connect/handshake error handling exists once.
    fn tls_send(
        host: &str,
        request: &[u8],
    ) -> Result<native_tls::TlsStream<TcpStream>, BackendError> {
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| BackendError::Transport(format!("tls init: {e}")))?;
        let tcp = TcpStream::connect((host, 443))
            .map_err(|e| {
                // Log the full hostname for the operator; sanitize the client-
                // facing message to avoid leaking internal topology (ADR-157).
                eprintln!("pasture: cloud backend connect {host}: {e}");
                BackendError::Transport(format!("cloud backend unreachable ({e})"))
            })?;
        let mut stream = connector
            .connect(host, tcp)
            .map_err(|e| BackendError::Transport(format!("tls handshake: {e}")))?;
        stream
            .write_all(request)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(stream)
    }

    /// Perform an HTTPS request over TLS, returning the raw response text.
    fn https_exchange(host: &str, request: &[u8]) -> Result<String, BackendError> {
        let mut stream = tls_send(host, request)?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;

    fn req() -> CompletionRequest {
        CompletionRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hi".to_string(),
                ..Default::default()
            }],
            stream: false,
            has_tools: false,
            sampling: Default::default(),
        }
    }

    #[test]
    fn test_provider_from_str() {
        assert_eq!(Provider::from_name("openai"), Some(Provider::OpenAI));
        assert_eq!(Provider::from_name("claude"), Some(Provider::Anthropic));
        assert_eq!(Provider::from_name("bogus"), None);
    }

    #[test]
    fn test_openai_body_and_headers() {
        let b = Provider::OpenAI.build_body(&req());
        assert!(b.contains("\"model\":\"test-model\""));
        assert!(b.contains("\"role\":\"user\""));
        let h = Provider::OpenAI.headers("KEY");
        assert_eq!(h[0].0, "Authorization");
        assert_eq!(h[0].1, "Bearer KEY");
    }

    #[test]
    fn test_anthropic_body_and_headers() {
        let b = Provider::Anthropic.build_body(&req());
        assert!(b.contains("\"max_tokens\""));
        let h = Provider::Anthropic.headers("KEY");
        assert!(h.iter().any(|(k, v)| k == "x-api-key" && v == "KEY"));
        assert!(h.iter().any(|(k, _)| k == "anthropic-version"));
    }

    #[test]
    fn test_parse_openai_response() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#;
        let (c, tc, p, comp) = Provider::OpenAI.parse_response(body).unwrap();
        assert_eq!(c, "hello");
        assert_eq!(tc, None);
        assert_eq!((p, comp), (5, 3));
    }

    #[test]
    fn test_parse_openai_response_tool_calls() {
        // ADR-177: a tool-call response has content:null and a tool_calls array.
        let body = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{}"}}]}}],"usage":{"prompt_tokens":8,"completion_tokens":4}}"#;
        let (c, tc, p, comp) = Provider::OpenAI.parse_response(body).unwrap();
        assert_eq!(c, "", "content is empty for a pure tool call");
        assert!(tc.is_some(), "tool_calls must be surfaced");
        assert!(tc.unwrap().contains("get_weather"));
        assert_eq!((p, comp), (8, 4));
    }

    #[test]
    fn test_parse_anthropic_response() {
        let body = r#"{"content":[{"type":"text","text":"hi there"}],"usage":{"input_tokens":7,"output_tokens":2}}"#;
        let (c, tc, p, comp) = Provider::Anthropic.parse_response(body).unwrap();
        assert_eq!(c, "hi there");
        assert_eq!(tc, None);
        assert_eq!((p, comp), (7, 2));
    }

    #[test]
    fn test_parse_anthropic_response_tool_use() {
        // ADR-179: a pure tool_use response has no text block; tool_calls must
        // be extracted and serialised in OpenAI format.
        let body = r#"{"content":[{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{"location":"SF"}}],"usage":{"input_tokens":10,"output_tokens":5}}"#;
        let (c, tc, p, comp) = Provider::Anthropic.parse_response(body).unwrap();
        assert_eq!(c, "", "no text block → empty content");
        let tc = tc.expect("tool_calls must be extracted");
        assert!(tc.contains("\"name\":\"get_weather\""), "{tc}");
        assert!(tc.contains("\"id\":\"toolu_01\""), "{tc}");
        assert!(tc.contains("\"type\":\"function\""), "{tc}");
        // input is re-serialised as the arguments JSON string
        assert!(tc.contains("location"), "{tc}");
        assert_eq!((p, comp), (10, 5));
    }

    #[test]
    fn test_parse_anthropic_response_mixed_text_and_tool_use() {
        // ADR-179: a mixed response has both text and tool_use blocks.
        let body = r#"{"content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"toolu_02","name":"search","input":{"q":"rust"}}],"usage":{"input_tokens":8,"output_tokens":6}}"#;
        let (c, tc, _p, _comp) = Provider::Anthropic.parse_response(body).unwrap();
        assert_eq!(c, "Let me check.");
        let tc = tc.expect("tool_calls present in mixed response");
        assert!(tc.contains("\"name\":\"search\""), "{tc}");
    }

    #[test]
    fn test_anthropic_body_translates_tools_to_anthropic_format() {
        // ADR-179: when sampling.tools is set, the Anthropic body must contain
        // "input_schema" (Anthropic format), not "parameters" (OpenAI format).
        let mut r = req();
        r.sampling.tools = Some(crate::json::parse(
            r#"[{"type":"function","function":{"name":"get_weather","description":"Get the weather","parameters":{"type":"object","properties":{"location":{"type":"string"}}}}}]"#,
        ).unwrap());
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"input_schema\""), "Anthropic format uses input_schema, not parameters: {b}");
        assert!(!b.contains("\"parameters\""), "parameters must be renamed to input_schema: {b}");
        assert!(b.contains("\"name\":\"get_weather\""), "{b}");
        assert!(b.contains("\"description\":\"Get the weather\""), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_anthropic_body_no_tools_when_absent() {
        // ADR-179: no tools in sampling → no tools field in Anthropic body.
        let b = Provider::Anthropic.build_body(&req());
        assert!(!b.contains("\"tools\""), "{b}");
    }

    #[test]
    fn test_build_body_stream_anthropic_no_stream_options() {
        // ADR-179: Anthropic rejects stream_options; only "stream":true should be added.
        let b = Provider::Anthropic.build_body_stream(&req());
        assert!(b.contains("\"stream\":true"), "{b}");
        assert!(!b.contains("stream_options"), "stream_options is OpenAI-only: {b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_build_body_stream_openai_still_includes_stream_options() {
        // ADR-179: OpenAI path must not regress — it still needs stream_options.include_usage.
        let b = Provider::OpenAI.build_body_stream(&req());
        assert!(b.contains("\"stream\":true"), "{b}");
        assert!(b.contains("\"stream_options\""), "{b}");
        assert!(b.contains("\"include_usage\":true"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_parse_anthropic_stream_line_tool_use_start() {
        // ADR-179: content_block_start with tool_use emits a ToolCallDelta with id+name.
        let line = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"get_weather","input":{}}}"#;
        match parse_anthropic_stream_line(line) {
            Some(OpenAiStreamEvent::ToolCallDelta(frag)) => {
                assert!(frag.contains("\"name\":\"get_weather\""), "{frag}");
                assert!(frag.contains("\"id\":\"toolu_01\""), "{frag}");
                assert!(frag.contains("\"index\":0"), "{frag}");
                assert!(crate::json::parse(&frag).is_ok(), "valid JSON: {frag}");
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_anthropic_stream_line_input_json_delta() {
        // ADR-179: content_block_delta with input_json_delta emits a ToolCallDelta
        // with the partial_json fragment as the arguments value.
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"loc"}}"#;
        match parse_anthropic_stream_line(line) {
            Some(OpenAiStreamEvent::ToolCallDelta(frag)) => {
                assert!(frag.contains("\"arguments\""), "{frag}");
                assert!(frag.contains("\"index\":0"), "{frag}");
                assert!(crate::json::parse(&frag).is_ok(), "valid JSON: {frag}");
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_anthropic_stream_line_text_block_start_ignored() {
        // ADR-179: content_block_start with type=text yields None (text arrives in delta).
        let line = r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
        assert_eq!(parse_anthropic_stream_line(line), None);
    }

    #[test]
    fn test_anthropic_body_translates_tool_choice_auto() {
        // ADR-180: tool_choice:"auto" → {"type":"auto"}
        let mut r = req();
        r.sampling.tools = Some(crate::json::parse(r#"[{"type":"function","function":{"name":"f","parameters":{}}}]"#).unwrap());
        r.sampling.tool_choice = Some(crate::json::parse(r#""auto""#).unwrap());
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"tool_choice\":{\"type\":\"auto\"}"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_anthropic_body_translates_tool_choice_required() {
        // ADR-180: tool_choice:"required" → {"type":"any"}
        let mut r = req();
        r.sampling.tools = Some(crate::json::parse(r#"[{"type":"function","function":{"name":"f","parameters":{}}}]"#).unwrap());
        r.sampling.tool_choice = Some(crate::json::parse(r#""required""#).unwrap());
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"tool_choice\":{\"type\":\"any\"}"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_anthropic_body_translates_tool_choice_named_function() {
        // ADR-180: {"type":"function","function":{"name":"get_weather"}} → {"type":"tool","name":"get_weather"}
        let mut r = req();
        r.sampling.tools = Some(crate::json::parse(r#"[{"type":"function","function":{"name":"get_weather","parameters":{}}}]"#).unwrap());
        r.sampling.tool_choice = Some(crate::json::parse(r#"{"type":"function","function":{"name":"get_weather"}}"#).unwrap());
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"tool_choice\":{\"type\":\"tool\",\"name\":\"get_weather\"}"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_anthropic_body_no_tool_choice_when_no_tools() {
        // ADR-180: tool_choice is only emitted when tools are also present (Anthropic requires both).
        let mut r = req();
        r.sampling.tool_choice = Some(crate::json::parse(r#""auto""#).unwrap());
        // No tools set
        let b = Provider::Anthropic.build_body(&r);
        assert!(!b.contains("\"tool_choice\""), "{b}");
    }

    #[test]
    fn test_read_sse_body_anthropic_tool_use_stream() {
        // ADR-179: full Anthropic tool-use SSE stream assembles tool_calls correctly.
        let resp = "HTTP/1.1 200 OK\r\n\r\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"get_weather\",\"input\":{}}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\"\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\": \\\"SF\\\"}\" }}\n\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let r = read_sse_body(&mut cur, Provider::Anthropic, &mut |_| {}).unwrap();
        assert_eq!(r.content, "", "pure tool_use has empty text content");
        let tc = r.tool_calls.expect("tool_calls accumulated from Anthropic stream");
        assert!(tc.contains("\"name\":\"get_weather\""), "{tc}");
        assert!(tc.contains("\"id\":\"toolu_01\""), "{tc}");
        // arguments: the two partial_json fragments concatenated
        assert!(tc.contains("location"), "{tc}");
        assert_eq!(r.usage, Some((20, 15)), "usage from message_start+message_delta");
    }

    #[test]
    fn test_parse_response_missing_content_errors() {
        assert!(Provider::OpenAI
            .parse_response(r#"{"choices":[]}"#)
            .is_err());
    }

    #[test]
    fn test_parse_response_surfaces_server_error() {
        let body =
            r#"{"error":{"message":"model 'foo' not found","type":"invalid_request_error"}}"#;
        let err = Provider::OpenAI.parse_response(body).unwrap_err();
        assert!(err.to_string().contains("model 'foo' not found"), "{err}");
    }

    #[test]
    fn test_parse_response_surfaces_string_error() {
        let body = r#"{"error":"bad request"}"#;
        let err = Provider::Anthropic.parse_response(body).unwrap_err();
        assert!(err.to_string().contains("bad request"), "{err}");
    }

    #[test]
    fn test_build_body_logprobs_sets_flag() {
        let b = Provider::OpenAI.build_body_logprobs(&req());
        assert!(b.contains("\"logprobs\":true"), "{b}");
        assert!(crate::json::parse(&b).is_ok());
    }

    #[test]
    fn test_mean_logprob_from_openai() {
        let body = r#"{"choices":[{"logprobs":{"content":[{"logprob":-0.1},{"logprob":-0.3}]}}]}"#;
        let m = mean_logprob_from_openai(body).unwrap();
        assert!((m - (-0.2)).abs() < 1e-9, "{m}");
        // Absent logprobs -> None.
        assert_eq!(
            mean_logprob_from_openai(r#"{"choices":[{"message":{"content":"hi"}}]}"#),
            None
        );
    }

    #[test]
    fn test_build_body_stream_sets_flag() {
        let b = Provider::OpenAI.build_body_stream(&req());
        assert!(b.contains("\"stream\":true"), "{b}");
        assert!(b.contains("\"model\":\"test-model\""));
        // Still valid JSON.
        assert!(crate::json::parse(&b).is_ok());
    }

    #[test]
    fn test_openai_body_threads_sampling() {
        let mut r = req();
        r.sampling = crate::backend::SamplingParams {
            temperature: Some(0.0),
            max_tokens: Some(256),
            ..Default::default()
        };
        let b = Provider::OpenAI.build_body(&r);
        assert!(b.contains("\"temperature\":0"), "{b}");
        assert!(b.contains("\"max_tokens\":256"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "{b}");
    }

    #[test]
    fn test_anthropic_body_honors_max_tokens_override() {
        let mut r = req();
        r.sampling = crate::backend::SamplingParams {
            max_tokens: Some(50),
            temperature: Some(0.3),
            stop: vec!["END".to_string()],
            ..Default::default()
        };
        let b = Provider::Anthropic.build_body(&r);
        assert!(b.contains("\"max_tokens\":50"), "{b}");
        assert!(!b.contains("\"max_tokens\":1024"), "{b}");
        assert!(b.contains("\"temperature\":0.3"), "{b}");
        // Anthropic uses stop_sequences, not stop.
        assert!(b.contains("\"stop_sequences\":[\"END\"]"), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "{b}");
    }

    #[test]
    fn test_anthropic_body_default_max_tokens_when_absent() {
        let b = Provider::Anthropic.build_body(&req());
        assert!(b.contains("\"max_tokens\":1024"), "{b}");
    }

    #[test]
    fn test_parse_openai_stream_line() {
        let d = r#"data: {"choices":[{"delta":{"content":"He"}}]}"#;
        assert_eq!(
            parse_openai_stream_line(d),
            Some(OpenAiStreamEvent::Delta("He".to_string()))
        );
        assert_eq!(
            parse_openai_stream_line("data: [DONE]"),
            Some(OpenAiStreamEvent::Done)
        );
        assert_eq!(parse_openai_stream_line(""), None);
        assert_eq!(
            parse_openai_stream_line(r#"data: {"choices":[{"delta":{}}]}"#),
            None
        );
    }

    #[test]
    fn test_parse_anthropic_stream_line() {
        let d = r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        assert_eq!(
            parse_anthropic_stream_line(d),
            Some(OpenAiStreamEvent::Delta("Hi".to_string()))
        );
        assert_eq!(
            parse_anthropic_stream_line(r#"data: {"type":"message_stop"}"#),
            Some(OpenAiStreamEvent::Done)
        );
        assert_eq!(
            parse_anthropic_stream_line(r#"data: {"type":"message_start"}"#),
            None
        );
    }

    #[test]
    fn test_read_sse_body_openai() {
        let resp =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\", world\"}}]}\n\n\
data: [DONE]\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let mut got = String::new();
        let r = read_sse_body(&mut cur, Provider::OpenAI, &mut |d| got.push_str(d)).unwrap();
        assert_eq!(r.content, "Hello, world");
        assert_eq!(got, "Hello, world"); // deltas were delivered incrementally
        assert_eq!(r.usage, None); // no usage chunk in this stream
        assert_eq!(r.tool_calls, None);
    }

    #[test]
    fn test_read_sse_body_openai_usage_chunk() {
        // ADR-173: usage chunk with empty choices carries prompt+completion tokens.
        let resp =
            "HTTP/1.1 200 OK\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
data: [DONE]\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let r = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap();
        assert_eq!(r.content, "hi");
        assert_eq!(r.usage, Some((5, 2)));
    }

    #[test]
    fn test_read_sse_body_openai_tool_calls() {
        // ADR-178: streamed tool_call fragments accumulate into a complete array;
        // content is empty for a pure tool call.
        let resp = "HTTP/1.1 200 OK\r\n\r\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"loc\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":\\\"SF\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let r = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap();
        assert_eq!(r.content, "", "pure tool call has empty content");
        let tc = r.tool_calls.expect("tool_calls accumulated");
        assert!(tc.contains("\"name\":\"get_weather\""), "{tc}");
        assert!(tc.contains("\"id\":\"call_1\""), "{tc}");
        // arguments concatenated across fragments
        assert!(tc.contains("{\\\"loc\\\":\\\"SF\\\"}"), "{tc}");
    }

    #[test]
    fn test_read_sse_body_anthropic_and_chunk_lines_ignored() {
        // Interleave a chunked-size line ("1a") to prove non-data lines are skipped.
        let resp = "HTTP/1.1 200 OK\r\n\r\n\
1a\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"B\"}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let r = read_sse_body(&mut cur, Provider::Anthropic, &mut |_| {}).unwrap();
        assert_eq!(r.content, "AB");
    }

    #[test]
    fn test_read_sse_body_anthropic_usage_split_across_events() {
        // ADR-175: input_tokens arrives in message_start, output_tokens in the
        // final message_delta. The two partial Usage events must merge to (in, out).
        let resp = "HTTP/1.1 200 OK\r\n\r\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let r = read_sse_body(&mut cur, Provider::Anthropic, &mut |_| {}).unwrap();
        assert_eq!(r.content, "hi");
        // input from message_start; output from message_delta (NOT the placeholder 1).
        assert_eq!(r.usage, Some((25, 42)));
    }

    #[test]
    fn test_tool_call_accumulator_empty_and_merge() {
        // Empty → None.
        assert_eq!(ToolCallAccumulator::default().finish(), None);
        // Two indexed calls, arguments split across fragments.
        let mut acc = ToolCallAccumulator::default();
        acc.push(r#"[{"index":0,"id":"a","type":"function","function":{"name":"f","arguments":"{\"x\":"}}]"#);
        acc.push(r#"[{"index":0,"function":{"arguments":"1}"}}]"#);
        acc.push(r#"[{"index":1,"id":"b","type":"function","function":{"name":"g","arguments":"{}"}}]"#);
        let out = acc.finish().unwrap();
        let v = parse(&out).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2, "{out}");
        assert_eq!(arr[0].get("id").and_then(|x| x.as_str()), Some("a"));
        assert!(out.contains("\"name\":\"f\""), "{out}");
        // arguments concatenated in order
        assert!(out.contains("{\\\"x\\\":1}"), "{out}");
        assert_eq!(arr[1].get("id").and_then(|x| x.as_str()), Some("b"));
    }

    #[test]
    fn test_tool_call_delta_event_parsed() {
        // ADR-178: a delta.tool_calls line is surfaced as a ToolCallDelta event.
        let line = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","type":"function","function":{"name":"f","arguments":""}}]}}]}"#;
        match parse_openai_stream_line(line) {
            Some(OpenAiStreamEvent::ToolCallDelta(frag)) => {
                assert!(frag.contains("\"name\":\"f\""), "{frag}");
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
    }

    #[test]
    fn test_anthropic_stream_line_usage_events() {
        // message_start → Usage(input, 0); message_delta → Usage(0, output).
        assert_eq!(
            parse_anthropic_stream_line(
                "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}"
            ),
            Some(OpenAiStreamEvent::Usage(7, 0))
        );
        assert_eq!(
            parse_anthropic_stream_line(
                "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":13}}"
            ),
            Some(OpenAiStreamEvent::Usage(0, 13))
        );
    }

    #[test]
    fn test_read_sse_body_error_status() {
        let resp = "HTTP/1.1 401 Unauthorized\r\n\r\n{\"error\":{\"message\":\"bad key\"}}";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let err = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
    }

    #[test]
    fn test_http_status_error_5xx_is_retryable() {
        use crate::backend::is_retryable;
        // 5xx -> Transport (retryable, IMP-9); 4xx -> Protocol (not).
        assert!(is_retryable(&http_status_error(503, "x".into())));
        assert!(is_retryable(&http_status_error(500, "x".into())));
        assert!(!is_retryable(&http_status_error(401, "x".into())));
        assert!(!is_retryable(&http_status_error(400, "x".into())));
    }

    #[test]
    fn test_read_sse_body_5xx_is_transport() {
        use crate::backend::is_retryable;
        let resp = "HTTP/1.1 503 Service Unavailable\r\n\r\n{\"error\":\"busy\"}";
        let mut cur = std::io::Cursor::new(resp.as_bytes().to_vec());
        let err = read_sse_body(&mut cur, Provider::OpenAI, &mut |_| {}).unwrap_err();
        assert!(is_retryable(&err), "503 should be retryable: {err}");
    }

    #[test]
    fn test_parse_http_response_content_length() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "{\"ok\":true}");
    }

    #[test]
    fn test_parse_http_response_chunked() {
        // Two chunks: "Hello" (5) and " world" (6), then terminating 0 chunk.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nHello\r\n6\r\n world\r\n0\r\n\r\n";
        let (status, body) = parse_http_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "Hello world");
    }

    #[test]
    fn test_parse_http_response_status_error() {
        let raw = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\n\r\n{}";
        let (status, _) = parse_http_response(raw).unwrap();
        assert_eq!(status, 401);
    }

    #[test]
    fn test_dechunk_single() {
        let raw =
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nhello world\r\n0\r\n\r\n";
        let (_, body) = parse_http_response(raw).unwrap();
        assert_eq!(body, "hello world");
    }

    #[test]
    fn test_dechunk_truncated_returns_error() {
        // ADR-107: a chunked body that declares more bytes than are present must
        // return a protocol error, not silently return partial data.
        // "a\r\n" declares 10 bytes but only 5 ("hello") follow.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\nhello";
        let err = parse_http_response(raw);
        assert!(err.is_err(), "truncated chunk must be an error, not silent partial data");
    }

    #[test]
    fn test_api_key_from_env_trims_whitespace() {
        // api_key_from_env must strip surrounding whitespace so keys written via
        // `export KEY=$(cat file)` (which appends a newline) still work.
        std::env::set_var("PASTURE_OPENAI_API_KEY", "sk-test\n");
        let k = api_key_from_env(Provider::OpenAI).unwrap();
        assert_eq!(k, "sk-test", "trailing newline should be trimmed");
        std::env::remove_var("PASTURE_OPENAI_API_KEY");

        std::env::set_var("PASTURE_OPENAI_API_KEY", "  sk-padded  ");
        let k2 = api_key_from_env(Provider::OpenAI).unwrap();
        assert_eq!(k2, "sk-padded", "surrounding spaces should be trimmed");
        std::env::remove_var("PASTURE_OPENAI_API_KEY");

        std::env::set_var("PASTURE_ANTHROPIC_API_KEY", "   \n  ");
        assert!(
            api_key_from_env(Provider::Anthropic).is_none(),
            "all-whitespace key should be None after trimming"
        );
        std::env::remove_var("PASTURE_ANTHROPIC_API_KEY");
    }

    // ---- ADR-182 tests ----

    #[test]
    fn test_openai_body_emits_tool_call_id_for_tool_message() {
        // ADR-182: role:"tool" with tool_call_id → "tool_call_id" field in the message object.
        // OpenAI requires this field; without it the API returns 400.
        let mut r = req();
        r.messages = vec![
            crate::backend::Message {
                role: "user".to_string(),
                content: "What is the weather?".to_string(),
                ..Default::default()
            },
            crate::backend::Message {
                role: "tool".to_string(),
                content: "72°F".to_string(),
                tool_call_id: Some("call_1".to_string()),
            },
        ];
        let b = Provider::OpenAI.build_body(&r);
        assert!(b.contains("\"tool_call_id\":\"call_1\""), "{b}");
        assert!(b.contains("\"role\":\"tool\""), "{b}");
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_anthropic_body_translates_tool_result_message() {
        // ADR-182: Anthropic requires tool result messages as role:"user" with a
        // tool_result content block — NOT role:"tool".
        let mut r = req();
        r.messages = vec![
            crate::backend::Message {
                role: "user".to_string(),
                content: "What is the weather?".to_string(),
                ..Default::default()
            },
            crate::backend::Message {
                role: "tool".to_string(),
                content: "72°F".to_string(),
                tool_call_id: Some("toolu_01".to_string()),
            },
        ];
        let b = Provider::Anthropic.build_body(&r);
        // Must be role:"user" (Anthropic form), not role:"tool"
        assert!(!b.contains("\"role\":\"tool\""), "Anthropic must not emit role:tool: {b}");
        assert!(b.contains("\"role\":\"user\""), "{b}");
        assert!(b.contains("\"type\":\"tool_result\""), "{b}");
        assert!(b.contains("\"tool_use_id\":\"toolu_01\""), "{b}");
        assert!(b.contains("72"), "{b}"); // content value
        assert!(crate::json::parse(&b).is_ok(), "valid JSON: {b}");
    }

    #[test]
    fn test_openai_body_plain_messages_unchanged_by_adr182() {
        // ADR-182 regression: ordinary user/assistant messages must NOT gain tool_call_id.
        let b = Provider::OpenAI.build_body(&req());
        assert!(!b.contains("tool_call_id"), "{b}");
    }
}

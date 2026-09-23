//! OpenAI-compatible response body builders (ADR-275, IMP-49).
//!
//! Pure functions: given already-computed values (a `CompletionResponse`,
//! token counts, ids), build the exact JSON/SSE/Prometheus text Pasture sends
//! on the wire. No I/O, no `Proxy` state — moved out of `proxy.rs` (which had
//! grown past 4,700 lines mixing HTTP-socket plumbing with these) as a
//! mechanical extraction with zero behaviour change (W5).
//!
//! `unix_now` stays in `proxy.rs` — it is a general time utility `impl Proxy`
//! also uses for non-response state (budget-day tracking), not a
//! response-building concern.

use crate::backend::{CompletionResponse, EmbeddingsResponse};
use crate::json::escape_string;
use crate::proxy::unix_now;
use std::sync::atomic::{AtomicU64, Ordering};

/// Build an OpenAI-compatible error body: `{"error":{"message":..,"type":..}}`
/// (SPEC §3.5). Both fields are JSON-escaped.
pub fn build_error_response(message: &str, kind: &str) -> String {
    build_error_response_coded(message, kind, None)
}

/// OpenAI-shape error envelope including the optional `code` field. OpenAI always
/// includes `param` and `code` keys (null when unknown); strict SDK deserializers
/// (openai-python `APIError.param`/`.code`, LiteLLM) read them, so they are always
/// emitted. `param` is null here — our call sites rarely know the offending field —
/// while `code` is populated for the cases where it is unambiguous (e.g. a
/// rate-limit or auth rejection).
pub fn build_error_response_coded(message: &str, kind: &str, code: Option<&str>) -> String {
    let code_field = match code {
        Some(c) => format!("\"{}\"", escape_string(c)),
        None => "null".to_string(),
    };
    format!(
        "{{\"error\":{{\"message\":\"{}\",\"type\":\"{}\",\"param\":null,\"code\":{}}}}}",
        escape_string(message),
        escape_string(kind),
        code_field,
    )
}

/// Build an OpenAI-compatible `GET /v1/models` list response (IMP-8).
/// Each model id is emitted as an `object: "model"` entry owned by "pasture".
/// The `created` field is required by the OpenAI Model schema (ADR-171); Pasture
/// has no per-model creation timestamp so `now` (request time) is used — the same
/// approach taken by LiteLLM and other routing proxies.
pub fn build_models_response(models: &[String]) -> String {
    let now = unix_now();
    let entries: Vec<String> = models
        .iter()
        .map(|m| {
            format!(
                "{{\"id\":\"{}\",\"object\":\"model\",\"created\":{now},\"owned_by\":\"pasture\"}}",
                escape_string(m)
            )
        })
        .collect();
    format!("{{\"object\":\"list\",\"data\":[{}]}}", entries.join(","))
}

/// Build a `GET /v1/models/{id}` response for a single configured model, or
/// `None` if the id is not in the advertised list (→ 404). OpenAI shape.
pub fn build_model_response(models: &[String], id: &str) -> Option<String> {
    if models.iter().any(|m| m == id) {
        Some(format!(
            "{{\"id\":\"{}\",\"object\":\"model\",\"created\":{},\"owned_by\":\"pasture\"}}",
            escape_string(id),
            unix_now()
        ))
    } else {
        None
    }
}

/// Build the `GET /v1/stats` JSON body (IMP-metrics): live counters from the
/// cost log. All values are PII-free aggregates (I3). Rates are rounded to 4 dp.
#[allow(clippy::too_many_arguments)]
pub fn build_stats_response(
    s: &crate::cost::CostSummary,
    cache_hits: u64,
    cache_misses: u64,
    cache_size: usize,
    cache_cap: usize,
    sem_hits: u64,
    sem_misses: u64,
    sem_size: usize,
    sem_cap: usize,
    budget_used: u64,
    budget_limit: u64,
    output_pii_categories: &[(&'static str, u64)],
    local_health: &'static str,
    input_pii_categories: &[(&'static str, u64)],
    local_health_last_error: Option<&str>,
    injection_guard_stats: &[(String, u64)],
    cloud_health: &'static str,
    cloud_health_last_error: Option<&str>,
    estimated_savings_usd: f64,
) -> String {
    let round4 = |x: f64| (x * 10_000.0).round() / 10_000.0;
    let fmt_cats = |cats: &[(&'static str, u64)]| -> String {
        cats.iter()
            .map(|(cat, n)| format!("\"{cat}\":{n}"))
            .collect::<Vec<String>>()
            .join(",")
    };
    // ADR-225: injection-guard outcome tally, keyed by "label:action" (e.g.
    // "role_switch:blocked"). Owned Strings (unlike the &'static str PII
    // category keys) since classify_injection returns owned labels.
    let injection_json: Vec<String> = injection_guard_stats
        .iter()
        .map(|(k, n)| format!("\"{}\":{n}", escape_string(k)))
        .collect();
    // Socratic follow-up to IMP-30/34: HealthCheck has captured `last_error`
    // since it was introduced, but nothing ever read it back out — an
    // operator could see "down" without knowing *why* (timeout? connection
    // refused? malformed response?) without grepping stderr. This is the same
    // diagnostic text `track_local_call` already prints there; exposing it
    // here is not new information disclosure, just a queryable copy of it.
    let last_error_json = match local_health_last_error {
        Some(e) => format!("\"{}\"", escape_string(e)),
        None => "null".to_string(),
    };
    // IMP-35/ADR-227: symmetric to local_health_last_error above.
    let cloud_last_error_json = match cloud_health_last_error {
        Some(e) => format!("\"{}\"", escape_string(e)),
        None => "null".to_string(),
    };
    format!(
        "{{\"object\":\"pasture.stats\",\"total\":{},\"local\":{},\"cloud\":{},\"cache\":{},\
\"cloud_rate\":{},\"cache_rate\":{},\"prompt_tokens\":{},\"completion_tokens\":{},\
\"cloud_cost_usd\":{},\"cache_hits\":{cache_hits},\"cache_misses\":{cache_misses},\
\"cache_size\":{cache_size},\"cache_capacity\":{cache_cap},\
\"semantic_cache_hits\":{sem_hits},\"semantic_cache_misses\":{sem_misses},\
\"semantic_cache_size\":{sem_size},\"semantic_cache_capacity\":{sem_cap},\
\"budget_daily_tokens_used\":{budget_used},\"budget_daily_tokens_limit\":{budget_limit},\
\"output_pii_categories\":{{{}}},\"local_health\":\"{local_health}\",\
\"local_health_last_error\":{last_error_json},\
\"cloud_health\":\"{cloud_health}\",\"cloud_health_last_error\":{cloud_last_error_json},\
\"input_pii_categories\":{{{}}},\"injection_guard_stats\":{{{}}},\
\"estimated_savings_usd\":{}}}",
        s.total,
        s.local,
        s.cloud,
        s.cache,
        round4(s.cloud_rate()),
        round4(s.cache_rate()),
        s.prompt_tokens,
        s.completion_tokens,
        round4(s.cloud_cost_usd),
        fmt_cats(output_pii_categories),
        fmt_cats(input_pii_categories),
        injection_json.join(","),
        round4(if estimated_savings_usd.is_finite() {
            estimated_savings_usd
        } else {
            0.0
        }),
    )
}

/// Format a float vector as a JSON array (finite values; non-finite → 0).
fn fmt_float_array(v: &[f64]) -> String {
    let nums: Vec<String> = v
        .iter()
        .map(|x| {
            if x.is_finite() {
                format!("{x}")
            } else {
                "0".to_string()
            }
        })
        .collect();
    format!("[{}]", nums.join(","))
}

/// Build an OpenAI-compatible `POST /v1/embeddings` response (IMP-8).
pub fn build_embeddings_response(resp: &EmbeddingsResponse) -> String {
    let data: Vec<String> = resp
        .vectors
        .iter()
        .enumerate()
        .map(|(i, vec)| {
            format!(
                "{{\"object\":\"embedding\",\"index\":{i},\"embedding\":{}}}",
                fmt_float_array(vec)
            )
        })
        .collect();
    format!(
        "{{\"object\":\"list\",\"data\":[{}],\"model\":\"{}\",\"usage\":{{\"prompt_tokens\":{},\"total_tokens\":{}}}}}",
        data.join(","),
        escape_string(&resp.model),
        resp.prompt_tokens,
        resp.prompt_tokens
    )
}

/// A unique completion id (`chatcmpl-…`), matching OpenAI's per-response id that
/// logging/observability/dedup tooling keys on. Uniqueness within the process is
/// guaranteed by an atomic counter; the wall-clock prefix adds cross-run variety.
pub(crate) fn next_completion_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{}{:08}", unix_now(), n)
}

/// A unique request id (`req_…`), matching OpenAI's per-response `x-request-id`
/// that clients and support tooling key on for tracing. Generated server-side
/// when the caller did not supply an `X-Request-ID`, so every response is
/// correlatable. Uniqueness within the process is guaranteed by an atomic
/// counter; the wall-clock prefix adds cross-run variety.
pub(crate) fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{}{:08}", unix_now(), n)
}

/// Return a deterministic `system_fingerprint` string for a given model name.
/// Uses FNV-1a (64-bit) truncated to 32 bits → `fp_pasture_XXXXXXXX`.
/// Identical model → identical fingerprint across requests and processes.
pub fn fingerprint_for_model(model: &str) -> String {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in model.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("fp_pasture_{:08x}", h as u32)
}

/// Return the OpenAI `finish_reason` string for a completed response (ADR-181).
/// A response with a `tool_calls` array uses `"tool_calls"`; all others use `"stop"`.
/// Used for both the SSE stop chunk and the OTel span `finish_reason` attribute.
pub fn finish_reason_for(resp: &CompletionResponse) -> &'static str {
    if resp.tool_calls.is_some() {
        "tool_calls"
    } else {
        "stop"
    }
}

pub fn build_openai_response(resp: &CompletionResponse, route_label: &str) -> String {
    build_openai_response_with_injection(resp, route_label, None)
}

/// Like `build_openai_response`, but adds `x_pasture_injection_flag` when
/// the injection guard fires in flag mode (IMP-20).
pub fn build_openai_response_with_injection(
    resp: &CompletionResponse,
    route_label: &str,
    injection_flag: Option<&str>,
) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    let fp = fingerprint_for_model(&resp.model);
    let flag_field = match injection_flag {
        Some(label) => format!(",\"x_pasture_injection_flag\":\"{}\"", escape_string(label)),
        None => String::new(),
    };
    // A tool-call response carries a `tool_calls` array and the `tool_calls`
    // finish reason (ADR-177); an ordinary completion carries neither.
    let (tool_calls_field, finish_reason) = match &resp.tool_calls {
        Some(tc) => (format!(",\"tool_calls\":{tc}"), "tool_calls"),
        None => (String::new(), "stop"),
    };
    format!(
        "{{\"id\":\"{}\",\"object\":\"chat.completion\",\"created\":{},\"model\":\"{}\",\"system_fingerprint\":\"{fp}\",\"x_pasture_route\":\"{}\"{flag_field},\"choices\":[{{\"index\":0,\"message\":{{\"role\":\"assistant\",\"content\":\"{}\"{tool_calls_field}}},\"logprobs\":null,\"finish_reason\":\"{finish_reason}\"}}],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
        next_completion_id(),
        unix_now(),
        escape_string(&resp.model),
        route_label,
        escape_string(&resp.content),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build a Prometheus text-format metrics response for `GET /metrics`.
/// Uses the standard exposition format (version 0.0.4): `# HELP`, `# TYPE`, then
/// metric lines. Counter names follow Prometheus naming conventions (total suffix
/// on counters, no suffix on gauges).
#[allow(clippy::too_many_arguments)]
pub fn build_metrics_response(
    s: &crate::cost::CostSummary,
    cache_hits: u64,
    cache_misses: u64,
    cache_size: usize,
    cache_cap: usize,
    sem_hits: u64,
    sem_misses: u64,
    sem_size: usize,
    sem_cap: usize,
    budget_used: u64,
    budget_limit: u64,
    estimated_savings_usd: f64,
) -> String {
    // Prometheus text exposition format v0.0.4.
    // Braces in label selectors are literal Prometheus syntax — not format args.
    format!(
        "# HELP pasture_requests_total Total requests handled by backend\n\
# TYPE pasture_requests_total counter\n\
pasture_requests_total{{route=\"local\"}} {local}\n\
pasture_requests_total{{route=\"cloud\"}} {cloud}\n\
pasture_requests_total{{route=\"cache\"}} {cache}\n\
# HELP pasture_prompt_tokens_total Total prompt tokens processed\n\
# TYPE pasture_prompt_tokens_total counter\n\
pasture_prompt_tokens_total {prompt_tokens}\n\
# HELP pasture_completion_tokens_total Total completion tokens generated\n\
# TYPE pasture_completion_tokens_total counter\n\
pasture_completion_tokens_total {completion_tokens}\n\
# HELP pasture_cloud_cost_usd_total Estimated cumulative cloud cost USD\n\
# TYPE pasture_cloud_cost_usd_total counter\n\
pasture_cloud_cost_usd_total {cloud_cost}\n\
# HELP pasture_cache_hits_total Cache hit count since process start\n\
# TYPE pasture_cache_hits_total counter\n\
pasture_cache_hits_total {cache_hits}\n\
# HELP pasture_cache_misses_total Cache miss count since process start\n\
# TYPE pasture_cache_misses_total counter\n\
pasture_cache_misses_total {cache_misses}\n\
# HELP pasture_cache_entries Current number of entries in the cache\n\
# TYPE pasture_cache_entries gauge\n\
pasture_cache_entries {cache_size}\n\
# HELP pasture_cache_capacity Maximum entries the cache holds (0=disabled)\n\
# TYPE pasture_cache_capacity gauge\n\
pasture_cache_capacity {cache_cap}\n\
# HELP pasture_semantic_cache_hits_total Semantic cache hit count since process start\n\
# TYPE pasture_semantic_cache_hits_total counter\n\
pasture_semantic_cache_hits_total {sem_hits}\n\
# HELP pasture_semantic_cache_misses_total Semantic cache miss count since process start\n\
# TYPE pasture_semantic_cache_misses_total counter\n\
pasture_semantic_cache_misses_total {sem_misses}\n\
# HELP pasture_semantic_cache_entries Current number of entries in the semantic cache\n\
# TYPE pasture_semantic_cache_entries gauge\n\
pasture_semantic_cache_entries {sem_size}\n\
# HELP pasture_semantic_cache_capacity Maximum entries the semantic cache holds (0=disabled)\n\
# TYPE pasture_semantic_cache_capacity gauge\n\
pasture_semantic_cache_capacity {sem_cap}\n\
# HELP pasture_budget_daily_tokens_used Cloud tokens used today against the daily budget (includes in-flight reservations)\n\
# TYPE pasture_budget_daily_tokens_used gauge\n\
pasture_budget_daily_tokens_used {budget_used}\n\
# HELP pasture_budget_daily_tokens_limit Daily cloud token budget (0=unlimited)\n\
# TYPE pasture_budget_daily_tokens_limit gauge\n\
pasture_budget_daily_tokens_limit {budget_limit}\n\
# HELP pasture_estimated_savings_usd_total Estimated USD saved by serving requests locally, priced at the configured cloud rate\n\
# TYPE pasture_estimated_savings_usd_total counter\n\
pasture_estimated_savings_usd_total {savings}\n\
# HELP gen_ai_client_token_usage_total Cumulative GenAI client token usage (IMP-39: OTel GenAI semantic-convention alias of pasture_prompt_tokens_total/pasture_completion_tokens_total, labelled per the gen_ai.token.type attribute; Pasture exposes cumulative counters here, not per-request histogram buckets, since it has no OTel SDK)\n\
# TYPE gen_ai_client_token_usage_total counter\n\
gen_ai_client_token_usage_total{{gen_ai_token_type=\"input\"}} {prompt_tokens}\n\
gen_ai_client_token_usage_total{{gen_ai_token_type=\"output\"}} {completion_tokens}\n\
# HELP gen_ai_requests_total Total requests by GenAI provider (IMP-39: OTel GenAI semantic-convention alias of pasture_requests_total, labelled per the gen_ai.provider.name attribute; \"cache\" is a Pasture-specific extension, not part of the OTel provider vocabulary)\n\
# TYPE gen_ai_requests_total counter\n\
gen_ai_requests_total{{gen_ai_provider_name=\"local\"}} {local}\n\
gen_ai_requests_total{{gen_ai_provider_name=\"cloud\"}} {cloud}\n\
gen_ai_requests_total{{gen_ai_provider_name=\"cache\"}} {cache}\n",
        local = s.local,
        cloud = s.cloud,
        cache = s.cache,
        prompt_tokens = s.prompt_tokens,
        completion_tokens = s.completion_tokens,
        cloud_cost = s.cloud_cost_usd,
        savings = if estimated_savings_usd.is_finite() {
            estimated_savings_usd
        } else {
            0.0
        },
    )
}

/// Build a stub `/v1/moderations` response. Pasture does not run content
/// moderation; the stub exists only so client SDKs that unconditionally call the
/// endpoint do not 404.
///
/// **Honesty contract (ADR-244).** The stub must never make a safety claim it
/// cannot back. It previously returned `flagged:false` with all-zero scores while
/// advertising `"model":"text-moderation-stable"` — OpenAI's real moderation
/// model — so a client using the verdict to gate display received a machine-
/// readable "this content is safe" for text Pasture never examined. That
/// contradicts how the rest of this proxy behaves: it rejects multimodal parts
/// rather than answering blind, and rejects `tools` on `/v1/responses` rather
/// than silently dropping them (ADR-241). So the response now self-identifies:
/// `model` is `pasture-no-moderation` and a top-level `x_pasture_moderated:false`
/// states outright that no moderation ran. The OpenAI-shaped `results[0]`
/// (`flagged` / `categories` / `category_scores`) is unchanged, so existing SDK
/// clients still parse it — the fix adds truth without breaking compatibility.
pub fn build_moderations_response() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let id = CTR.fetch_add(1, Ordering::Relaxed);
    format!(
        "{{\"id\":\"modr-pasture{id:08}\",\"model\":\"pasture-no-moderation\",\
\"x_pasture_moderated\":false,\
\"results\":[{{\"flagged\":false,\
\"categories\":{{\"hate\":false,\"hate/threatening\":false,\"harassment\":false,\
\"harassment/threatening\":false,\"self-harm\":false,\"self-harm/intent\":false,\
\"self-harm/instructions\":false,\"sexual\":false,\"sexual/minors\":false,\"violence\":false,\
\"violence/graphic\":false}},\
\"category_scores\":{{\"hate\":0.0,\"hate/threatening\":0.0,\"harassment\":0.0,\
\"harassment/threatening\":0.0,\"self-harm\":0.0,\"self-harm/intent\":0.0,\
\"self-harm/instructions\":0.0,\"sexual\":0.0,\"sexual/minors\":0.0,\"violence\":0.0,\
\"violence/graphic\":0.0}}}}]}}"
    )
}

/// Build a legacy `text_completion` response for `POST /v1/completions`.
/// Uses `"object":"text_completion"` and `choices[].text` (not `message.content`)
/// so old SDK clients that probe the pre-chat API receive a valid reply.
pub fn build_legacy_completion_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    let fp = fingerprint_for_model(&resp.model);
    // IDs for text completions use the "cmpl-" prefix (matching OpenAI convention).
    let id = next_completion_id().replace("chatcmpl-", "cmpl-");
    format!(
        "{{\"id\":\"{id}\",\"object\":\"text_completion\",\"created\":{},\"model\":\"{}\",\
\"system_fingerprint\":\"{fp}\",\"x_pasture_route\":\"{route_label}\",\
\"choices\":[{{\"text\":\"{}\",\"index\":0,\"logprobs\":null,\"finish_reason\":\"stop\"}}],\
\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":{},\"total_tokens\":{}}}}}",
        unix_now(),
        escape_string(&resp.model),
        escape_string(&resp.content),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build an OpenAI Responses-API reply (`object:"response"`) for
/// `POST /v1/responses` (IMP-38, ADR-241). The generated text is placed in the
/// canonical `output[0].content[0]` (`type:"output_text"`) and also mirrored in
/// the top-level `output_text` convenience field the SDKs expose. Usage uses the
/// Responses names (`input_tokens`/`output_tokens`), not the chat names.
pub fn build_responses_response(resp: &CompletionResponse, route_label: &str) -> String {
    let total = resp.prompt_tokens + resp.completion_tokens;
    let resp_id = next_completion_id().replace("chatcmpl-", "resp_");
    let msg_id = next_completion_id().replace("chatcmpl-", "msg_");
    let text = escape_string(&resp.content);
    format!(
        "{{\"id\":\"{resp_id}\",\"object\":\"response\",\"created_at\":{},\"model\":\"{}\",\
\"status\":\"completed\",\"x_pasture_route\":\"{route_label}\",\
\"output\":[{{\"type\":\"message\",\"id\":\"{msg_id}\",\"status\":\"completed\",\
\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\",\
\"annotations\":[]}}]}}],\"output_text\":\"{text}\",\
\"usage\":{{\"input_tokens\":{},\"output_tokens\":{},\"total_tokens\":{}}}}}",
        unix_now(),
        escape_string(&resp.model),
        resp.prompt_tokens,
        resp.completion_tokens,
        total,
    )
}

/// Build an OpenAI-compatible streaming chunk (`chat.completion.chunk`). The `id`,
/// `model`, `fingerprint`, and `created` are supplied by the caller so every chunk
/// of one stream shares them (OpenAI behaviour). Passing `created` as a parameter
/// instead of calling `unix_now()` here ensures the timestamp is identical for
/// every chunk in the stream, including the final usage and stop chunks (ADR-170).
pub fn build_openai_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    delta: &str,
    route_label: &str,
    finish: Option<&str>,
    created: u64,
) -> String {
    let delta_field = if delta.is_empty() {
        "{}".to_string()
    } else {
        format!("{{\"content\":\"{}\"}}", escape_string(delta))
    };
    let finish_field = match finish {
        Some(f) => format!("\"{f}\""),
        None => "null".to_string(),
    };
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{delta_field},\"logprobs\":null,\"finish_reason\":{finish_field}}}]}}",
        escape_string(model)
    )
}

/// Build a leading streaming chunk that surfaces the injection-guard label to the
/// client in flag mode (ADR-191), mirroring the buffered response's top-level
/// `x_pasture_injection_flag`. Empty delta, `finish_reason:null`; the content
/// chunks follow. Emitted once, as the first data frame after the SSE headers, so
/// a streaming client can detect a flagged request exactly like a buffered one.
pub fn build_openai_injection_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    route_label: &str,
    label: &str,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"x_pasture_injection_flag\":\"{}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"logprobs\":null,\"finish_reason\":null}}]}}",
        escape_string(model),
        escape_string(label)
    )
}

/// Build a streaming chunk carrying a `tool_calls` array in the delta (ADR-178).
/// `tool_calls` is the raw JSON array. Shares the stream's `id`/`model`/
/// `fingerprint`/`created`; `finish_reason` is null (the following stop chunk
/// carries `"tool_calls"`).
pub fn build_openai_tool_calls_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    tool_calls: &str,
    route_label: &str,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":{tool_calls}}},\"logprobs\":null,\"finish_reason\":null}}]}}",
        escape_string(model)
    )
}

/// Build the final streaming chunk carrying token `usage` (emitted only when the
/// client sets `stream_options.include_usage`). Per the OpenAI contract this
/// chunk has an empty `choices` array. Shares the stream's `id`, `model`,
/// `fingerprint`, and `created` timestamp (ADR-170).
pub fn build_openai_usage_chunk(
    id: &str,
    model: &str,
    fingerprint: &str,
    route_label: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    created: u64,
) -> String {
    format!(
        "{{\"id\":\"{id}\",\"object\":\"chat.completion.chunk\",\"created\":{created},\"model\":\"{}\",\"system_fingerprint\":\"{fingerprint}\",\"x_pasture_route\":\"{route_label}\",\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt_tokens},\"completion_tokens\":{completion_tokens},\"total_tokens\":{}}}}}",
        escape_string(model),
        prompt_tokens + completion_tokens
    )
}

/// Wrap a payload in a Server-Sent Events `data:` frame.
pub fn sse_frame(payload: &str) -> String {
    format!("data: {payload}\n\n")
}

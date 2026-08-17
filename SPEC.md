# Pasture — Specification (SPEC.md)

Version: tracks `Cargo.toml` (0.26.0 + Unreleased, current through ADR-189). Status:
normative for the HTTP API and routing engine; descriptive for the CLI. Keywords
**MUST**, **SHOULD**, **MAY** per RFC 2119.

> This is the contract the implementation is held to. The **Conformance** section
> (§12) records where the code already satisfies the spec and which gaps this round
> closes. Design rationale lives in `ARCHITECTURE.md` (ADRs); the forward backlog in
> `COMPETITIVE.md` / `RESEARCH.md`.

---

## 1. Overview

Pasture is a single, zero-dependency Rust binary that exposes an **OpenAI-compatible
HTTP proxy** and a **CLI**. For each chat request it decides — deterministically and
from the host hardware — whether to serve it from a **local** model or escalate to a
**cloud** API, keeping sensitive prompts local. It is built on the Rust standard
library only; the cloud (HTTPS/TLS) path is gated behind the optional `cloud` feature.

**Invariants (apply everywhere):**
- **I1 Zero-dependency default.** The default build MUST link no non-std crates.
  Anything needing a dependency MUST sit behind a Cargo feature (currently `cloud`).
- **I2 Privacy-first.** A prompt classified sensitive MUST NOT leave the machine
  unless `PASTURE_ALLOW_SENSITIVE_CLOUD` is set. Sensitive prompts MUST NOT be cached.
- **I3 No PII in logs.** Logs and the cost record MUST contain only category labels,
  counts, route, model, cost, and confidence numbers — never prompt/response content
  or matched PII values.
- **I4 Determinism.** Routing decisions MUST be a pure function of (prompt text,
  tool-presence, sensitivity, hardware threshold, available backends, forced route).
- **I5 Localhost default.** The proxy MUST bind `127.0.0.1` unless configured
  otherwise; it is not network-exposed by default.

---

## 2. CLI

`pasture <command> [args] [--flags]`. Commands (descriptive):

| Command | Purpose |
|---------|---------|
| `hw` | print detected RAM / CPU / GPU |
| `route <text>` | dry-run: show the route and reason, no inference |
| `chat <text>` | one-shot completion through the router |
| `serve` | start the OpenAI-compatible proxy |
| `up` | ensure Ollama + model, then serve |
| `setup` | beginner welcome + `doctor` check |
| `connect <app>` | print client setup (Cursor / Open WebUI / Continue / SDK / lmstudio) |
| `calibrate [--target R \| --sweep \| --logprob \| --error --labels <f> \| --auroc --labels <f>]` | recommend `PASTURE_THRESHOLD` / cascade threshold from the cost log; `--auroc` self-tests whether the confidence signal predicts correctness at all (IMP-47) |
| `models` | recommend local models for this machine |
| `doctor` | diagnose setup and print fixes |
| `eval [--external <file>] [--json]` | routing accuracy + threshold sweep |
| `stats [--json]` | summarize the cost log |
| `improvements [path] [--review]` | print the self-improvement ledger (verified change history) |
| `config` | print effective configuration |
| `donate` / `refer` | monetization surfaces (links only) |
| `version` / `help` | version / usage |

Global flags: `--local` / `--cloud` force a route; `--addr host:port` sets the listen
address. Forced routes MUST still be overridden by privacy (§5).

---

## 3. HTTP API

Base URL: `http://<addr>/v1` (default `http://127.0.0.1:8645/v1`). HTTP/1.1, plain
(no TLS on the proxy itself — it is localhost by default, I5). Served by a bounded
worker pool (size = `available_parallelism`, clamped 2..=32).

### 3.1 `POST /v1/chat/completions`
Request body: OpenAI chat-completion JSON. Recognized fields:
- `messages` (**required**, non-empty array of `{role, content}`). `content` is either a
  string **or** an OpenAI array-of-parts (`[{"type":"text","text":...}, ...]`); text parts are
  concatenated (newline-joined) and routed as text (IMP-31). A non-text part (image/audio/file)
  ⇒ `400` (Pasture routes text only and must not answer a vision request as if the image were
  absent). Missing/empty ⇒ `400`.
- `model` (optional string; default `"default"`). Pasture routes by content, so the
  client's `model` is advisory.
- `stream` (optional bool; default false). `true` ⇒ SSE response (§3.4).
- `stream_options.include_usage` (optional bool). With streaming, `true` appends a
  final `usage` chunk before `[DONE]` (§3.4, IMP-stream-usage).
- `tools` / `functions` (optional arrays). A **non-empty** array marks the request as
  tool-using ⇒ hard signal ⇒ cloud (§4, IMP-10). Fields are passed through unchanged.
- **Sampling parameters** (optional, IMP-sampling): `temperature`, `top_p`,
  `max_tokens` (alias `max_completion_tokens`), `stop` (string or array of strings),
  `seed`, `presence_penalty`, `frequency_penalty`. Present values MUST be forwarded to
  the selected backend (OpenAI/OpenAI-compat as top-level fields; Ollama under
  `options`, length cap as `num_predict`; Anthropic as `max_tokens` +
  `stop_sequences`). Non-finite numbers are rejected. Sampling params are part of the
  cache key (§6), so they never cross-serve responses.
- **`response_format`** (optional, IMP-response-format): structured-output / JSON mode.
  Forwarded verbatim to OpenAI/OpenAI-compat; for Ollama mapped to the top-level
  `format` field (`{"type":"json_object"}` → `"json"`, `{"type":"json_schema",…}` →
  the embedded schema). Part of the cache key.

Success (non-stream): `200`, body is an OpenAI `chat.completion` object containing
`id` (a unique `chatcmpl-…`, IMP-completion-id), `object:"chat.completion"`,
**`created`** (Unix seconds), `model`, `choices[0]`
(`message.role="assistant"`, `message.content`, `finish_reason:"stop"`), `usage`
(`prompt_tokens`, `completion_tokens`, `total_tokens`), and the Pasture extension
**`x_pasture_route`** ∈ {`local`,`cloud`,`cache`}.

### 3.1a `POST /v1/responses` (OpenAI Responses API shim, IMP-38)
A compatibility shim for the OpenAI Responses API (`client.responses.create`),
which several clients — e.g. the Codex CLI — now require in place of Chat
Completions. Translates the request into the internal chat shape and routes it
through the **same** pipeline (routing, privacy, cache, cost log), then formats
the reply as a Responses object.

Request: `{"model", "input", …}`. `input` is a **string** (→ one user message) or
an **array** of `{role, content}` items, where `content` is a string or an array
of text parts (`type:"input_text"`/`"output_text"`/`"text"`). A top-level
`instructions` string becomes a leading system message. `max_output_tokens` maps
to `max_tokens`; `developer` role → `system`.

Success (non-stream): `200`, `{"object":"response","status":"completed","model",`
`"x_pasture_route","output":[{"type":"message","role":"assistant","content":`
`[{"type":"output_text","text",…}]}],"output_text":<same text>,"usage":`
`{"input_tokens","output_tokens","total_tokens"}}`.

**Scope (v1):** text-only, non-streaming. `"stream":true` and a non-empty
`"tools"` array are rejected with `400` pointing at `/v1/chat/completions` — a
deliberate choice over silently dropping tools (the migration failure mode where
a tool call surfaces as raw text). Streaming Responses (`response.output_text.delta`
SSE events) and tool use are documented follow-ups.

### 3.2 `GET /v1/models`
`200`, OpenAI list: `{"object":"list","data":[{"id","object":"model","owned_by":"pasture"}…]}`
listing the configured local (and, if enabled, cloud) model ids; de-duplicated. An
empty list is still valid (IMP-8).

### 3.2a `GET /v1/models/{id}`
`200` with `{"id","object":"model","owned_by":"pasture"}` when `{id}` is a configured
model; otherwise `404` with the error envelope (§3.5). Query strings and a trailing
slash are tolerated (IMP-model-retrieve).

### 3.2b `POST /v1/embeddings`
Request body: `{"model":<string>,"input":<string|string[]>}`. `input` MUST be a
non-empty string or a non-empty array of non-empty strings. Missing or invalid
`input` ⇒ `400`.

Success: `200`, OpenAI embeddings shape:
`{"object":"list","data":[{"object":"embedding","index":<n>,"embedding":[…]}…],`
`"model":<string>,"usage":{"prompt_tokens":<n>,"total_tokens":<n>}}`.

Routing: embeddings are forwarded to the **local backend only** (Ollama `/api/embed`
or OpenAI-compat `/v1/embeddings`). Cloud escalation does not apply. If no local
backend is reachable the proxy returns `502` with the OpenAI error envelope (§3.5).
Privacy rules do not apply to embeddings (the vector is returned to the caller, not
logged). Implemented by `handle_embeddings` (IMP-8 completion).

### 3.2b-2 `POST /v1/moderations` (compatibility stub — performs NO moderation)
A stub that exists **only** so OpenAI SDK versions which call `/v1/moderations`
unconditionally do not break on a `404` (ADR-061). Pasture **does not run content
moderation of any kind**: the `input` is accepted, ignored, and never examined.

Success: `200`, an OpenAI-shaped moderation object whose `results[0]` carries the
usual `flagged` / `categories` / `category_scores` fields (all safe/zero) so SDK
clients parse it, **plus two honesty markers (ADR-244)**: `model` is
`"pasture-no-moderation"` (it MUST NOT impersonate a real moderation model such
as `text-moderation-stable`) and a top-level **`"x_pasture_moderated": false`**
declares machine-readably that no moderation was performed.

> ⚠️ **This response is not a safety verdict.** `flagged:false` here means "not
> checked", never "checked and found safe". Clients MUST NOT gate display,
> storage, or forwarding on it. Check `x_pasture_moderated` and call a real
> moderation service if you need one.

### 3.2c `GET /v1/stats`
`200`, JSON snapshot of the cost-log counters plus live in-memory state (IMP-metrics).
Read-only and PII-free (I3); a missing cost log reads as all-zeros. No auth
(localhost-default, I5). Implemented by `handle_stats` (ADR-038); field-complete as
of ADR-233 (previously this section documented only a stale subset of the response).
Full field list, in response order:

| Field | Type | Meaning |
|---|---|---|
| `object` | string | always `"pasture.stats"` |
| `total` | int | requests served, all routes |
| `local` | int | requests served locally |
| `cloud` | int | requests served by cloud |
| `cache` | int | requests served from the exact-match cache |
| `cloud_rate` | float | `cloud / total`, 4dp |
| `cache_rate` | float | `cache / total`, 4dp |
| `prompt_tokens` / `completion_tokens` | int | cumulative, from the cost log |
| `cloud_cost_usd` | float | cumulative estimated cloud spend, 4dp |
| `cache_hits` / `cache_misses` | int | exact-match cache (IMP-11) |
| `cache_size` / `cache_capacity` | int | exact-match cache occupancy |
| `semantic_cache_hits` / `semantic_cache_misses` | int | semantic cache (IMP-12) |
| `semantic_cache_size` / `semantic_cache_capacity` | int | semantic cache occupancy |
| `budget_daily_tokens_used` / `budget_daily_tokens_limit` | int | IMP-26 daily budget gauge; `limit:0` means unset |
| `output_pii_categories` | object | category → count tally, response text (IMP-33) |
| `input_pii_categories` | object | category → count tally, request text that triggered local-only routing (IMP-28) |
| `local_health` | string | `"healthy"` \| `"degraded"` \| `"down"` (IMP-30) |
| `local_health_last_error` | string \| null | last local-backend failure message (ADR-222) |
| `cloud_health` | string | `"healthy"` \| `"degraded"` \| `"down"` (IMP-35, ADR-227) |
| `cloud_health_last_error` | string \| null | last primary-cloud failure message; never set by the secondary/fallback provider |
| `injection_guard_stats` | object | `"label:action"` → count tally, e.g. `"role_switch:blocked"` (IMP-20, ADR-225) |
| `estimated_savings_usd` | float | cumulative estimated savings from local routing, 4dp (IMP-37): the local-route prompt+completion tokens priced at `PASTURE_CLOUD_PRICE_PER_1M`, i.e. what those requests would have cost on the configured cloud backend. `0` when no cloud price is set. Cache hits are excluded (ambiguous counterfactual) |

### 3.2c-2 `GET /v1/history`
`200`, per-UTC-day rollups of the cost log (IMP-48). Where §3.2c answers "what is
true now", this adds the time axis so a dashboard can show whether things are
improving. Read-only and PII-free (I3) — it reads the same cost log, so no
separate history file exists to drift; a missing log reads as `"days":[]` (not an
error). Rate-limit-exempt like `/v1/stats` and `/metrics`; auth applies when set.

Body: `{"object":"pasture.history","days":[…]}`, **ascending by day**, at most the
**30** most recent days that have data. Each element:

| Field | Type | Meaning |
|---|---|---|
| `day` | int | UTC midnight (Unix seconds) of the day |
| `local` / `cloud` / `cache` | int | requests that day per route (both caches count as `cache`, as in §3.2c) |
| `prompt_tokens` / `completion_tokens` | int | that day's totals, all routes |
| `cloud_cost_usd` | float | that day's cloud spend, 4dp |
| `estimated_savings_usd` | float | that day's local tokens priced at `PASTURE_CLOUD_PRICE_PER_1M`, 4dp — same pricing as IMP-37; `0` when no price is set |

### 3.2d `POST /v1/route`
Routing **preview / dry-run** (ADR-198). Request body: a chat-completions body
(`messages` required, plus optional `model`/`tools`). Runs the privacy + routing
decision **without calling any backend** — no tokens spent, no cost logged, nothing
sent to a cloud provider. Success: `200` with
`{"object":"pasture.route","route":∈{local,cloud},"reason":<string>,"budget":<string|null>,`
`"sensitive":<bool>,"categories":[<label>…],"estimated_tokens":<n>,"predicted_output_tokens":<n>,`
`"predicted_total_tokens":<n>,"estimated_cost_usd":<f>,"has_tools":<bool>}`.
`route` is the **effective** route: the IMP-26 budget/spike guard is applied
read-only (ADR-200), so an over-budget cloud request previews as `local`
(local-only redirect) or carries a `budget` note (warn / would-be-blocked); the
guard reserves nothing. `estimated_tokens` is the content-only routing-heuristic
value; the predicted token fields and `estimated_cost_usd` use `estimation_text`
+ the IMP-24 output prediction priced from `PASTURE_CLOUD_PRICE_PER_1M` (ADR-199),
and cost is 0 unless the request would actually be served on cloud. PII-free —
`categories` carries only labels, never matched values (I3). Subject to the §7
auth/rate-limit gate. Implemented by `handle_route_preview`.

The CLI `pasture route <text>` is the offline twin of this endpoint (ADR-201/202):
it prints the same facts in human form, or — with `--json` — emits the **identical**
`pasture.route` object (`has_tools` always `false`, since the CLI routes plain
text). Both surfaces share one computation (`compute_route_preview`) so the CLI's
effective route, cost, and budget verdict cannot disagree with the HTTP endpoint.
The CLI seeds the read-only budget/spike check from today's `cost_log` records
(the same source the live proxy's atomics are seeded from at startup).

### 3.3 `GET /health`
`200`, `{"status":"ok"}`.

### 3.3a `GET /dashboard` (and `GET /`)
`200`, `Content-Type: text/html; charset=utf-8`. Returns the embedded web
dashboard: a single self-contained HTML page (inline CSS + vanilla JS, no
external references) that polls `GET /v1/stats` client-side every 5 seconds and
renders the local/cloud split, cloud spend vs. daily budget, exact + semantic
cache hit rates, and both backends' circuit-breaker health. `GET /` serves the
identical page. The page is **read-only** — it issues no mutating requests.

Like `/health`, the HTML shell is **exempt from auth and rate limiting** (a
browser navigation cannot attach an `Authorization` header, and the shell
carries no data). When auth (§7) is configured, the shell still loads; its
`fetch('/v1/stats')` receives `401`, and the page then prompts for a token and
stores it in the browser tab's `sessionStorage` for subsequent polls. Only
`GET` is allowed; other methods ⇒ `405` with `Allow: GET`.

### 3.4 Streaming (SSE)
When `stream:true`: `200`, `Content-Type: text/event-stream`. Each frame is
`data: <chat.completion.chunk>\n\n` with `id` (one unique id shared by all chunks of
the stream, IMP-completion-id), `object:"chat.completion.chunk"`,
**`created`**, `x_pasture_route`, and `choices[0].delta`. A final chunk carries
`finish_reason:"stop"`. When the request sets `stream_options.include_usage:true`,
one further chunk follows with an empty `choices` array and a `usage` object
(`prompt_tokens`/`completion_tokens`/`total_tokens`, IMP-stream-usage). The stream
ends with `data: [DONE]\n\n`. Routing and privacy decide **before** the first byte;
cascade does not apply to streaming (the local answer cannot be un-sent).

### 3.5 Errors
All error responses MUST use the OpenAI envelope:
`{"error":{"message":<string>,"type":<string>}}`. Status → type mapping:

| Condition | Status | `type` |
|-----------|-------:|--------|
| malformed request / bad JSON / missing messages | `400` | `invalid_request_error` |
| missing/invalid bearer token when auth is enabled (§7) | `401` | `invalid_request_error` |
| a socket read times out before the request completes (§7) | `408` | `invalid_request_error` |
| request body exceeds the size cap (§7) | `413` | `invalid_request_error` |
| rate limit exceeded when a limit is set (§7) | `429` | `rate_limit_error` |
| unknown route/path or method | `404` | `invalid_request_error` |
| no backend available / sensitive-but-no-local / forced-route-unavailable | `503` | `routing_error` |
| backend (local/cloud) failure with no fallback | `502` | `upstream_error` |

### 3.6 CORS (opt-in)
When `PASTURE_CORS_ORIGINS` is set (a comma-separated allow-list, or `*`), the proxy
supports browser clients (IMP-cors): an `OPTIONS` preflight to any path is answered
`204` with `Access-Control-Allow-Origin/-Methods/-Headers` and `Access-Control-Max-Age`
**before** the §7 auth/rate-limit gate (preflight carries no credentials), and
`Access-Control-Allow-Origin` is reflected on all responses (buffered and SSE). A
specific (non-`*`) allowed origin also receives `Vary: Origin`; a disallowed origin
receives no CORS header. Default (unset) = no CORS headers, `OPTIONS` ⇒ `404`.

---

## 4. Routing engine (deterministic)

`decide_full(text, forced, sensitive, has_tools)` evaluates in this exact order and
returns at the first match (I4):

1. **Sensitivity.** If `sensitive` and not `allow_sensitive_cloud`: route **local**
   if a local backend exists, else `503` (`SensitiveButNoLocal`). Overrides `forced`.
2. **Forced.** If `forced` is set: that route if its backend exists, else `503`
   (`ForcedRouteUnavailable`).
3. **Availability.** none ⇒ `503` (`NoBackendAvailable`); only one present ⇒ that one.
4. **Hard signals** (only if `code_to_cloud`): route **cloud** if any of —
   code fences (ADR-210: balanced opening+closing ``` at line start; in-line
   backticks in prose do not count); reasoning markers (EN/JA); strict-format/code-gen
   markers (EN/JA); `≥3` clause-terminating question marks (ADR-209 — a URL query
   `?` followed by alphanumeric does not count; full-width `？` always counts);
   `≥3` **distinct** math symbol types (ADR-208 — e.g. `^`, `+`, `=` together;
   single-char repetition like `/` in a URL does not trigger); **multi-step**
   (ADR-242 — `≥3` distinct sequencing cues such as *first/then/finally* or
   まず/次に/最後に, matched whole-word so "then" inside "strengthen" does not
   count, **or** a numbered list of `≥3` items; catches short multi-step plans
   the reasoning markers and length threshold miss, which small local models
   handle worse per 2026 SLM benchmarks); **`has_tools`** (IMP-10).
5. **Length.** `estimate_tokens(text) ≥ threshold` ⇒ **cloud**, else **local**.

**Token estimation:** script-aware (ADR-022). CJK/Hangul/fullwidth ≈ 1 token/char;
other text ≈ 1 token / 4 chars.

**Hardware thresholds:** GPU ≥ 8 GB VRAM → 2000; GPU present or RAM ≥ 16 GB → 800;
else → 300. Overridable by `PASTURE_THRESHOLD`.

**Three request projections (purpose-built, never conflated):** the request is reduced
to text three different ways, each matched to its job (ADR-184/187):
- **`routing_text`** = message `content` only — drives the routing **decision** and the
  token-length heuristic above. Tool bytes are excluded so they don't inflate length.
- **`estimation_text`** = `routing_text` + serialised `tools`/`tool_choice` + assistant
  `tool_calls` — drives **token accounting** (cost log, budget/spike), because those
  bytes are really billed by the provider.
- **`privacy_text`** = `routing_text` + assistant `tool_calls` arguments — drives PII
  **classification** (§5), so a secret living only in a tool-call argument cannot escape
  the sensitivity guard. Tool *definitions* are excluded (developer schema, not PII).

---

## 5. Privacy classification

`classify(text)` returns category labels only (I3). **Eleven categories:**

| Category | Detection rule |
|---|---|
| `keyword` | EN/JA case-insensitive keyword match (credentials, financial, government ID, medical) |
| `email` | `local@domain.tld` heuristic |
| `ip` | Four-octet IPv4 in 0–255 |
| `credit_card` | 13–19 digit run passing Luhn check |
| `my_number` | Japanese My Number (マイナンバー): exactly 12 digits passing the check-digit (検査用数字) test (ADR-212) |
| `iban` | IBAN bank account **value** (compact form, 15–34 chars) passing the ISO 7064 MOD-97-10 checksum, independent of the `iban` keyword (ADR-240) |
| `phone` | International `+`-form (8–15 digits) and JP domestic mobile/hyphenated landline |
| `api_key` | Known vendor prefix + min length (20+ prefixes: OpenAI, GitHub, Stripe, SendGrid, AWS, Google OAuth, npm, …) |
| `jwt` | `eyJ…` + 3 base64url segments |
| `pem_key` | `-----BEGIN … PRIVATE KEY-----` block (RSA/EC/OPENSSH/PKCS8 etc.) |
| `url_credential` | `scheme://user:password@host` embedded credentials |
| `env_secret` | `KEY=value` / `export KEY=value` where KEY name suggests a secret (password/secret/token/auth/…) |

**Full-width normalization (ADR-213/214):** before running the digit-based
detectors (`ip`, `credit_card`, `my_number`, `iban`, `phone`), the classifier normalizes
to ASCII: full-width digits (`０`–`９`, U+FF10–FF19), the full-width full stop
(`．` → `.`), the full-width hyphen and Unicode dash family (`－‐‑‒–—―` → `-`),
and the ideographic space (`　` → ` `). So numeric PII typed in full-width form
(common in Japanese input, e.g. `１２３４５６７８９０１８`,
`１９２．１６８．１．１`, `４１１１－１１１１－１１１１－１１１１`) is still
detected and kept local. This is a targeted, std-only normalization (not a full
NFKC, which would require an external crate).

Any hit ⇒ sensitive ⇒ forced local (§4 step 1) and never cached (I2).
The bias is deliberately toward over-classifying: false positive = stays local (cheap);
false negative = data leak (unacceptable).

---

## 6. Cascade, cache, backends

- **Cascade** (`PASTURE_CASCADE`, opt-in; needs cloud + a key): answer local, then
  escalate to cloud only when the local mean-logprob `< PASTURE_CASCADE_LOGPROB`
  (else a text heuristic). Never for sensitive content; never for streaming; on cloud
  failure (after retries, §6) it keeps the local answer.
- **Cache** (`PASTURE_CACHE=<n>`, opt-in): exact-match on hash(model + messages),
  bounded FIFO. Hits skip the backend (`x_pasture_route:"cache"`, cost 0). Never
  caches sensitive prompts (I2). Applies to both buffered and `stream:true` requests
  (IMP-31b): a streamed hit replays the cached content as SSE; a streamed miss
  populates the cache. The semantic cache remains buffered-only.
  **Semantic-cache lexical second-gate (IMP-46, opt-in):** when
  `PASTURE_SEMANTIC_MIN_LEXICAL > 0`, a cosine hit must ALSO share at least that
  Jaccard token-set overlap with the cached prompt before it is served. This
  rejects embedding false-positives — prompts that are cosine-close but share
  almost no words, which would otherwise return a wrong cached answer — while a
  small floor (e.g. 0.15) preserves genuine paraphrase hits. Default `0.0` =
  disabled, so existing behaviour is unchanged unless configured.
  **Time-sensitive bypass (IMP-41):** a prompt whose correct answer changes over
  time (detected by deterministic EN+JA markers -- "today", "latest", "current
  price", 今日, 最新, … -- via `routing::is_time_sensitive`) is never served
  from nor stored into **either** cache, on both the buffered and streaming
  paths: an exact text match on "what's the weather today?" is still a stale
  answer. Orthogonal to routing: such prompts may still route local.
- **Backends.** Local: Ollama (default) or any OpenAI-compatible server
  (`PASTURE_LOCAL_BACKEND`). Cloud (`cloud` feature, BYOK): OpenAI or Anthropic over
  HTTPS. Both support streaming. **Cloud resilience (IMP-9):** a transient failure
  (connection/timeout, provider **5xx**) is retried with exponential backoff
  (`PASTURE_CLOUD_RETRY`, default 2); on final failure the request falls back to local
  when available. 4xx is not retried.

---

## 7. Limits & security

- The proxy MUST cap the request **header** block (1 MiB) and the request **body**
  (`MAX_BODY_BYTES`, 16 MiB). A `Content-Length` over the cap, or a body that grows
  past it, MUST yield `413` (not unbounded reads) — DoS hardening (IMP-21).
- The JSON parser MUST bound recursion depth (128) (ADR-026).
- **Connection timeout (IMP-timeout).** Each connection SHOULD have a read/write
  timeout (`PASTURE_REQUEST_TIMEOUT`, default 30s; 0 disables) so a slow/dead client
  cannot pin a worker (slow-loris). A read that times out before the request completes
  yields `408`.
- **Auth (opt-in, IMP-15).** When `PASTURE_AUTH_TOKEN` is set, every `/v1/*` request
  MUST carry `Authorization: Bearer <token>` (compared in constant time); a missing or
  wrong token yields `401`. `/health` is exempt. Default (unset) = no auth, matching the
  localhost-only model (I5).
- **Rate limit (opt-in, IMP-15).** When `PASTURE_RATE_LIMIT=<n>` (requests/minute) is
  set, a global token bucket caps `/v1/*`; requests over budget yield `429`. `/health`
  is exempt. Default (0) = unlimited.

### 7.1 Cost budget & spike guard (IMP-26)

Applied **only** to cloud-bound, non-sensitive completions; the daily counter and the
spike average are both **UTC-day-scoped** (lazy reset at midnight, no timer thread;
the daily counter is seeded from the cost log at startup so it survives a restart,
ADR-155/185).

- **Daily budget.** `PASTURE_BUDGET_DAILY_TOKENS=<n>` caps cumulative cloud tokens
  (prompt+completion) per UTC day. Tokens are **pre-reserved atomically** before the
  request and reconciled against actual usage afterward (ADR-163), so a `stream:true`
  request cannot bypass the cap. The estimate counts tool definitions and `tool_calls`
  payloads, not just message content (`estimation_text`, ADR-184). On exceedance the
  `PASTURE_BUDGET_ACTION` fires: `local-only` (default) silently reroutes to local;
  `warn` proceeds to cloud and logs to stderr; `block` returns `429`.
- **Spike guard.** `PASTURE_SPIKE_FACTOR=<f>` (default 50; 0 disables) routes a single
  request **local** when its estimated tokens exceed `f ×` the running cloud-request
  average, catching a runaway prompt even when the daily budget is off.

### 7.2 Prompt-injection guard (IMP-20)

`PASTURE_INJECTION_GUARD` ∈ {`off` (default), `flag`, `block`} runs deterministic,
case-insensitive lexical matching (EN+JA) at the proxy boundary against role-switch /
system-override phrases (`role_switch`) and data-exfiltration phrases (`exfil_attempt`).
`flag` annotates the response with `x_pasture_injection_flag:<label>` and proceeds;
`block` returns `400`. Only the matched **label** is ever recorded — never prompt
content (I3).

Matching runs over normalized text (ADR-249: invisible/bidi/tag characters
stripped; full-width letters and Cyrillic/Greek homoglyphs folded) and covers
tool-call arguments as well as message content (ADR-247). A third label,
**`encoded_payload`**, is emitted when an injection phrase is recovered by
decoding a base64 / hex / ROT13 run and re-screening the **decoded** text
(decode-and-rescreen, ADR-252). Decoding is iterative (ADR-253), so nested and
mixed layerings (`base64(base64(x))`, `hex(base64(x))`, `base64(rot13(x))`) are
peeled; the traversal is bounded by depth, node-count and byte budgets — a flag always requires a real phrase, so
merely looking encoded is never sufficient. The plaintext pass runs first, so
`encoded_payload` is reserved for phrases that were genuinely hidden.

---

## 8. Configuration

Precedence: built-in defaults → key=value config file → `PASTURE_*` env vars (env
wins). Variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_LISTEN_ADDR` | `127.0.0.1:8645` | proxy listen address |
| `PASTURE_OLLAMA_HOST` / `PASTURE_OLLAMA_PORT` | `127.0.0.1` / `11434` | local Ollama endpoint |
| `PASTURE_LOCAL_MODEL` | `llama3` | local model id |
| `PASTURE_LOCAL_BACKEND` | `ollama` | `ollama` \| `lmstudio`/`openai` |
| `PASTURE_LOCAL_OPENAI_URL` | `http://127.0.0.1:1234/v1` | OpenAI-compat local URL |
| `PASTURE_CLOUD_PROVIDER` / `PASTURE_CLOUD_MODEL` | — / `gpt-4o-mini` | cloud provider / model |
| `PASTURE_CLOUD_FALLBACK_PROVIDER` / `PASTURE_CLOUD_FALLBACK_MODEL` | _(off)_ | secondary cloud provider/model tried when the primary fails all retries (IMP-9) |
| `PASTURE_OPENAI_API_KEY` / `PASTURE_ANTHROPIC_API_KEY` | — | BYOK (never logged) |
| `PASTURE_THRESHOLD` | hardware | token length threshold override |
| `PASTURE_CASCADE` | off | enable cascade |
| `PASTURE_CASCADE_LOGPROB` | `-1.0` | cascade escalation threshold |
| `PASTURE_CACHE` | `0` | exact-match cache capacity |
| `PASTURE_CLOUD_RETRY` | `2` | cloud transient-failure retries |
| `PASTURE_ALLOW_SENSITIVE_CLOUD` | off | allow sensitive → cloud |
| `PASTURE_COST_LOG` | `pasture-cost.jsonl` | cost log path |
| `PASTURE_LANG` | auto | `ja` \| `en` |
| `PASTURE_LOCAL_ONLY` | off | force all traffic local; cloud disabled (ADR-035) |
| `PASTURE_LOCAL_FAST_MODEL` | _(off)_ | lightweight model for simple short queries (dual-local) |
| `PASTURE_FAST_THRESHOLD` | `50` | token threshold below which the fast model is used |
| `PASTURE_INJECT_CONTEXT` | off | prepend date/OS system message for PC-assistant mode |
| `PASTURE_AUTH_TOKEN` | _(off)_ | require `Authorization: Bearer <token>` on `/v1/*` (ADR-040) |
| `PASTURE_RATE_LIMIT` | `0` | global requests/min cap on `/v1/*` (0 = unlimited) |
| `PASTURE_CORS_ORIGINS` | _(off)_ | CORS allow-list (comma-separated, or `*`) for browser clients (§3.6) |
| `PASTURE_REQUEST_TIMEOUT` | `30` | per-connection read/write timeout in seconds (0 = none, §7) |
| `PASTURE_AUTH_TOKEN` | _(off)_ | require `Authorization: Bearer <token>` on `/v1/*` (§7) |
| `PASTURE_RATE_LIMIT` | `0` | global requests/min cap on `/v1/*` (0 = unlimited, §7) |
| `PASTURE_MAX_BODY_BYTES` | `16777216` | request body cap in bytes; larger ⇒ `413` (§7) |
| `PASTURE_LOCAL_TIMEOUT` | `120` | per-request local-backend read timeout (seconds) |
| `PASTURE_CLOUD_PRICE_PER_1M` | `0,0` | cloud price `"<input>,<output>"` USD per 1M tokens (§9, ADR-166) |
| `PASTURE_BUDGET_DAILY_TOKENS` | `0` | daily cloud-token cap; 0 = disabled (§7.1, ADR-26) |
| `PASTURE_BUDGET_ACTION` | `local-only` | over-budget action: `local-only` \| `warn` \| `block` (§7.1) |
| `PASTURE_SPIKE_FACTOR` | `50` | route local if a request exceeds `factor × running cloud avg`; 0 = off (§7.1) |
| `PASTURE_PSEUDONYMIZE` | off | mask PII with reversible tokens on cloud-bound requests (§14, IMP-19) |
| `PASTURE_OUTPUT_PII_SCAN` | off | tally PII categories seen in response text; detection-only, exposed on `/v1/stats` (IMP-33) |
| `PASTURE_INPUT_PII_SCAN` | off | tally which PII categories trigger local-only routing; exposed on `/v1/stats` (IMP-28) |
| `PASTURE_DECISION_LOG` | _(off)_ | path to the routing decision audit log (JSONL: signals, threshold, route, reason; no PII) (IMP-29) |
| `PASTURE_HEALTH_COOLDOWN_SECS` | `30` | circuit-breaker cooldown, shared by both directions: once local is Down, redirect non-sensitive Local decisions to cloud (IMP-34); once cloud is Down, redirect natural Cloud decisions to local (IMP-35). Redirect stops once cooldown elapses (a probe request is let through); 0 disables both breakers |
| `PASTURE_INJECTION_GUARD` | `off` | prompt-injection guard: `off` \| `flag` \| `block` (§7.2, IMP-20) |
| `PASTURE_SEMANTIC_CACHE` | `0` | semantic (embedding) cache capacity; 0 = disabled (§6, IMP-12) |
| `PASTURE_SEMANTIC_THRESHOLD` | `0.92` | cosine-similarity threshold for a semantic-cache hit (§6) |
| `PASTURE_SEMANTIC_MIN_LEXICAL` | `0.0` | lexical second-gate floor (Jaccard token overlap) a cosine hit must also clear; `0` disables (§6, IMP-46) |
| `PASTURE_CACHE_TTL` | `0` | cached-entry TTL in seconds; 0 = no TTL (FIFO only) |
| `PASTURE_CACHE_CONTROL` | off | inject Anthropic `cache_control` prompt-caching hint (no-op for OpenAI) |
| `PASTURE_HARD_PROMPTS` | _(off)_ | path to known-hard-prompts file; near-matches escalate to cloud (IMP-14) |
| `PASTURE_HARD_THRESHOLD` | `0.85` | cosine similarity at which a prompt counts as "near a known-hard prompt" |
| `PASTURE_SKILLS` | _(off)_ | skill→route overrides, e.g. `code:local,math:cloud` (IMP-25) |
| `PASTURE_SYSTEM_PROMPT` | _(off)_ | system prompt prepended to every proxied request |
| `PASTURE_ACCESS_LOG` | _(off)_ | path for a per-request JSONL access log (PII-free, I3) |
| `PASTURE_OTEL_LOG` | _(off)_ | path for an OpenTelemetry GenAI trace log (§9.1, IMP-23) |
| `PASTURE_STATE` | `pasture-state.txt` | small state file (donation-nudge counter) |
| `PASTURE_DONATE_URL` | _(off)_ | donation URL surfaced by `donate` and the nudge |
| `PASTURE_NO_NUDGE` | off | disable the periodic stderr donation nudge |

> **Spec-drift note (this round):** earlier spec text referenced `PASTURE_PROXY_TOKEN`;
> the implemented variable is **`PASTURE_AUTH_TOKEN`** (the only name recognised). The
> cloud-price variable is the single **`PASTURE_CLOUD_PRICE_PER_1M`** (`"<in>,<out>"`),
> not separate per-input/per-output vars.

---

## 9. Cost log (JSONL)

One line per completion. Fields: `ts` (Unix s), `route`, `model`, `prompt_tokens`,
`completion_tokens`, `cost_usd` (0 for local/cache), and optional `logprob`. No PII
(I3). `stats` aggregates: counts by route, cloud rate, cache-hit rate, token totals,
spend, and the logprob distribution. `cost_usd` is real when `PASTURE_CLOUD_PRICE_PER_1M`
is set (ADR-166), else structural `0`.

### 9.1 OpenTelemetry trace log (opt-in, IMP-23)

When `PASTURE_OTEL_LOG=<path>` is set, one JSONL span is appended per request using
GenAI semantic-convention attributes: `gen_ai.system`, `gen_ai.request.model`,
`gen_ai.usage.input_tokens`/`output_tokens`, `pasture.route` ∈ {local,cloud,cache},
`status` ∈ {ok,error}, and `finish_reason` (`stop` | `tool_calls`, ADR-181). Trace/span
ids are time+counter derived (not crypto-random). No PII (I3).

---

## 10. Evaluation

`eval` runs a built-in labelled set (plain→local, hard→cloud, sensitive→local) and a
threshold sweep, reporting accuracy, cloud rate, false/missed escalations. Fully
offline (I1).

---

## 11. i18n

All user-facing strings come from a std-only catalog keyed `namespace.component.key`,
Japanese-first with English fallback and `{name}` interpolation; language auto-detected
or via `PASTURE_LANG`. A test enforces EN/JA key parity.

---

## 12. Tool / function calling (ADR-177…189)

Pasture proxies the full OpenAI tool-calling loop across both providers, end to end:

- **Request in.** `tools` / `tool_choice` are forwarded verbatim; a non-empty `tools`
  array is also a hard routing signal (§4, IMP-10). Assistant history messages may
  carry `content:null` with a `tool_calls` array (valid; ADR-183), and tool-result
  messages carry `tool_call_id` (ADR-182). Both fields are parsed and preserved.
- **Provider translation.** For OpenAI the fields pass through; for Anthropic,
  `role:"tool"` results are rewritten to a `user` message with a `tool_result` content
  block, and assistant `tool_calls` to `tool_use` blocks (ADR-179/180/183).
- **Response out.** A tool-call completion carries a `tool_calls` array and
  `finish_reason:"tool_calls"` (ADR-177/181), in both buffered and SSE form (ADR-178).
- **Cache.** The exact-match key includes message-level `tool_calls`/`tool_call_id`
  (ADR-186), so two histories that differ only in tool arguments never cross-serve.
- **Privacy.** Tool-call arguments are classified for PII (`privacy_text`, §4/§5,
  ADR-187) and pseudonymized when masking is on (§14, ADR-188/189).

---

## 13. Reversible pseudonymization (opt-in, IMP-19 / ADR-188-189)

When `PASTURE_PSEUDONYMIZE=1`, **cloud-bound** requests have detected PII replaced with
stable opaque tokens before they leave the machine, and the cloud response has the
tokens restored to the original values. Categories: `<EMAIL_n>`, `<IP_n>` (v4+v6),
`<PHONE_n>` (domestic single-token forms via the per-token loop; `+`-prefixed
international numbers that span whitespace such as `+1 555 123 4567` via a span
pre-pass, ADR-207), `<KEY_n>`, `<CARD_n>` (Luhn-valid credit cards, ADR-196),
`<JWT_n>` (ADR-197), `<URL_n>` (userinfo in `scheme://user:pass@host`, ADR-203),
`<ENV_n>` (secret value in `KEY=value` / `export KEY=value` assignments, ADR-203),
`<PEM_n>` (complete PEM private-key block, header + body + footer, ADR-204),
`<MYNUMBER_n>` (Japanese My Number / マイナンバー — 12 digits + check digit, ADR-212),
`<IBAN_n>` (IBAN bank account value, compact form, MOD-97 checksum — masked by a
span pre-pass that runs **before** credit-card masking so a short all-digit IBAN
is claimed whole rather than partially caught by the 13–19-digit card scan, ADR-240).
Coverage:

- **Message content** — tokenised and restored (buffered + SSE, the latter handling a
  token split across deltas, ADR-168).
- **Tool-call arguments** — PII inside the JSON-encoded `arguments` is masked via a
  JSON-string-aware walker (ADR-188); the same value in content and arguments shares one
  token, restored from a single mapping entry.
- **Cloud-generated `tool_calls` in the response** — also restored, so a token the model
  echoes back never reaches the client raw (ADR-189).

The mapping lives only in memory for the request and is **never logged** (I3/I5). This is
best-effort over the patterns Pasture detects — it is **not** a guarantee that no other
sensitive data is sent; `PASTURE_LOCAL_ONLY` remains the hard guarantee.

---

## 14. Conformance & gaps

**Satisfied by the current implementation:** §2 CLI; §3.1–3.3 (incl. `/v1/models`,
IMP-8) **+ §3.2b `/v1/embeddings` (IMP-8 completion, ADR-034) + §3.2c `/v1/stats`
(IMP-metrics, ADR-038)**; §4 routing incl. tools (IMP-10) **+ `local_only`
(ADR-035)**; §5 privacy; §6 cascade/cache/backends incl. cloud retry+fallback
(IMP-9); §8 config **+ `PASTURE_LOCAL_ONLY`, `PASTURE_LOCAL_FAST_MODEL`,
`PASTURE_INJECT_CONTEXT` (ADR-035)**; §9 cost log; §10 eval; §11 i18n; §7 header cap
+ parser depth.

**Gaps closed in this round (to satisfy this spec):**
- **§3.5 error envelope.** Errors now emit `{"error":{"message,type}}` (were flat
  `{"error":"…"}`).
- **§3.1/§3.4 `created`.** Chat responses and stream chunks now include the OpenAI
  `created` timestamp.
- **§7 body cap.** `read_request` now rejects oversized bodies with `413`
  (`MAX_BODY_BYTES`), closing the unbounded-body DoS (IMP-21, partial).
- **§3.2b `POST /v1/embeddings`.** Local-backend embeddings pass-through now
  implemented: `Backend::embeddings` trait method, `OllamaBackend` + `OpenAiCompatBackend`
  implementations, `handle_embeddings` in the proxy, `parse_embeddings_request` +
  `build_embeddings_response` + `fmt_float_array` helpers (ADR-034).

**Shipped since initial spec (tracked in COMPETITIVE.md / ARCHITECTURE.md):**
- `tool_choice` is not separately inspected; only the presence of `tools`/`functions` arrays
  matters for hard-signal routing (ADR-031, IMP-10). Full `tool_choice` parsing remains future work.
- **Full multi-turn tool/function calling** across OpenAI + Anthropic, incl. cache-key,
  privacy, and pseudonymization coverage (§12/§13, ADR-177…189).
- Multi-provider cloud fallback chain: `PASTURE_CLOUD_FALLBACK_PROVIDER` / `PASTURE_CLOUD_FALLBACK_MODEL`
  configure a secondary cloud provider tried when the primary fails all retries (IMP-9, ADR-136).
- Auth + rate-limit: **`PASTURE_AUTH_TOKEN`** + `PASTURE_RATE_LIMIT` (IMP-15, §7).
- Cost budget + spike guard: `PASTURE_BUDGET_DAILY_TOKENS` / `_ACTION` / `PASTURE_SPIKE_FACTOR`
  (§7.1, IMP-26, ADR-155/163/184/185).
- Prompt-injection guard: `PASTURE_INJECTION_GUARD` (§7.2, IMP-20).
- Semantic cache: `PASTURE_SEMANTIC_CACHE` via local embeddings (IMP-12, ADR-123).
- Calibrated-uncertainty escalation: `calibrate --logprob` + mean-logprob cascade (IMP-13, ADR-124).

**Known remaining gaps / future work:**
- `tool_choice` value (`"auto"`/`"none"`/named) is forwarded but not used as a routing
  signal beyond mere tool presence.
- `\uXXXX`-escaped PII inside JSON is decoded by the parser but the pseudonymizer's
  string walker matches on the decoded text only when it forms a recognisable token;
  adversarially split escapes are out of scope (best-effort, §13).
- The semantic cache is buffered-only (no SSE replay).

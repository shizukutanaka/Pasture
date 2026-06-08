# Pasture — Specification (SPEC.md)

Version: tracks `Cargo.toml` (0.26.0 + Unreleased). Status: normative for the
HTTP API and routing engine; descriptive for the CLI. Keywords **MUST**, **SHOULD**,
**MAY** per RFC 2119.

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
| `connect <app>` | print client setup (Cursor / Open WebUI / Continue / SDK / lmstudio) |
| `calibrate [--logprob]` | recommend `PASTURE_THRESHOLD` / cascade threshold from the cost log |
| `models` | recommend local models for this machine |
| `doctor` | diagnose setup and print fixes |
| `eval` | routing accuracy + threshold sweep |
| `stats` | summarize the cost log |
| `improvements [path]` | print the self-improvement ledger (verified change history) |
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
- `messages` (**required**, non-empty array of `{role, content}`; `content` MUST be a
  string). Missing/empty ⇒ `400`.
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

Success (non-stream): `200`, body is an OpenAI `chat.completion` object containing
`id`, `object:"chat.completion"`, **`created`** (Unix seconds), `model`, `choices[0]`
(`message.role="assistant"`, `message.content`, `finish_reason:"stop"`), `usage`
(`prompt_tokens`, `completion_tokens`, `total_tokens`), and the Pasture extension
**`x_pasture_route`** ∈ {`local`,`cloud`,`cache`}.

### 3.2 `GET /v1/models`
`200`, OpenAI list: `{"object":"list","data":[{"id","object":"model","owned_by":"pasture"}…]}`
listing the configured local (and, if enabled, cloud) model ids; de-duplicated. An
empty list is still valid (IMP-8).

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

### 3.2c `GET /v1/stats`
`200`, JSON snapshot of the cost-log counters (IMP-metrics):
`{"object":"pasture.stats","total","local","cloud","cache","cloud_rate","cache_rate",`
`"prompt_tokens","completion_tokens","cloud_cost_usd"}`. Read-only and PII-free (I3);
a missing cost log reads as all-zeros. No auth (localhost-default, I5). Implemented by
`handle_stats` (ADR-038).

### 3.3 `GET /health`
`200`, `{"status":"ok"}`.

### 3.4 Streaming (SSE)
When `stream:true`: `200`, `Content-Type: text/event-stream`. Each frame is
`data: <chat.completion.chunk>\n\n` with `id`, `object:"chat.completion.chunk"`,
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
   code fences; reasoning markers (EN/JA); strict-format/code-gen markers (EN/JA);
   `≥3` question marks; `≥4` math symbols; **`has_tools`** (IMP-10).
5. **Length.** `estimate_tokens(text) ≥ threshold` ⇒ **cloud**, else **local**.

**Token estimation:** script-aware (ADR-022). CJK/Hangul/fullwidth ≈ 1 token/char;
other text ≈ 1 token / 4 chars.

**Hardware thresholds:** GPU ≥ 8 GB VRAM → 2000; GPU present or RAM ≥ 16 GB → 800;
else → 300. Overridable by `PASTURE_THRESHOLD`.

---

## 5. Privacy classification

`classify(text)` returns category labels only (I3). **Ten categories:**

| Category | Detection rule |
|---|---|
| `keyword` | EN/JA case-insensitive keyword match (credentials, financial, government ID, medical) |
| `email` | `local@domain.tld` heuristic |
| `ip` | Four-octet IPv4 in 0–255 |
| `credit_card` | 13–19 digit run passing Luhn check |
| `phone` | International `+`-form (8–15 digits) and JP domestic mobile/hyphenated landline |
| `api_key` | Known vendor prefix + min length (20+ prefixes: OpenAI, GitHub, Stripe, SendGrid, AWS, Google OAuth, npm, …) |
| `jwt` | `eyJ…` + 3 base64url segments |
| `pem_key` | `-----BEGIN … PRIVATE KEY-----` block (RSA/EC/OPENSSH/PKCS8 etc.) |
| `url_credential` | `scheme://user:password@host` embedded credentials |
| `env_secret` | `KEY=value` / `export KEY=value` where KEY name suggests a secret (password/secret/token/auth/…) |

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
  caches sensitive prompts (I2).
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
- **Auth (opt-in, IMP-15).** When `PASTURE_AUTH_TOKEN` is set, every `/v1/*` request
  MUST carry `Authorization: Bearer <token>` (compared in constant time); a missing or
  wrong token yields `401`. `/health` is exempt. Default (unset) = no auth, matching the
  localhost-only model (I5).
- **Rate limit (opt-in, IMP-15).** When `PASTURE_RATE_LIMIT=<n>` (requests/minute) is
  set, a global token bucket caps `/v1/*`; requests over budget yield `429`. `/health`
  is exempt. Default (0) = unlimited.

---

## 8. Configuration

Precedence: built-in defaults → key=value config file → `PASTURE_*` env vars (env
wins). Variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_LISTEN_ADDR` | `127.0.0.1:8645` | proxy listen address |
| `PASTURE_OLLAMA_HOST` / `_PORT` | `127.0.0.1` / `11434` | local Ollama endpoint |
| `PASTURE_LOCAL_MODEL` | `llama3` | local model id |
| `PASTURE_LOCAL_BACKEND` | `ollama` | `ollama` \| `lmstudio`/`openai` |
| `PASTURE_LOCAL_OPENAI_URL` | `http://127.0.0.1:1234/v1` | OpenAI-compat local URL |
| `PASTURE_CLOUD_PROVIDER` / `_MODEL` | — / `gpt-4o-mini` | cloud provider / model |
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

---

## 9. Cost log (JSONL)

One line per completion. Fields: `ts` (Unix s), `route`, `model`, `prompt_tokens`,
`completion_tokens`, `cost_usd` (0 for local/cache), and optional `logprob`. No PII
(I3). `stats` aggregates: counts by route, cloud rate, cache-hit rate, token totals,
spend, and the logprob distribution.

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

## 12. Conformance & gaps

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

**Deferred (tracked in COMPETITIVE.md / RESEARCH.md):**
- `tool_choice` is not separately inspected (only `tools`/`functions` arrays).
- Multi-provider cloud fallback chain (IMP-9 remainder); auth + rate-limit (IMP-15);
  semantic cache (IMP-12); calibrated-uncertainty escalation (IMP-13).

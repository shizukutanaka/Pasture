# Changelog

All notable changes to this project are documented here.
Format follows Keep a Changelog; versioning follows SemVer.

## [Unreleased]

### Added — X-Request-ID echo (IMP-request-id)

- The proxy now echoes the caller-supplied `X-Request-ID` request header back on
  every response (buffered, streaming/SSE, error, HEAD). Clients use this header
  to correlate asynchronous responses and trace distributed calls. Matches OpenAI
  API and LiteLLM behaviour.
- CRLF injection guarded: `\r` and `\n` are stripped from the header value before
  it is reflected, preventing header-injection attacks.
- The header is absent on the response when the client does not include it. 3 tests.

### Added — `HEAD /health` and `/v1/engines` alias (IMP-compat-methods)

- `HEAD /health` is now accepted alongside `GET /health`. The response carries
  the same headers (including the correct `Content-Length` for the body the GET
  would return) with no response body, as required by RFC 7231 §4.3.2. Monitoring
  tools that prefer HEAD for liveness checks now work. 1 test.
- `GET /v1/engines[/{id}]` is now an alias for `GET /v1/models[/{id}]`. The
  OpenAI v1 "engines" path was deprecated but is still used by old SDK versions
  and some LLM clients; aliasing it avoids 404 errors during model discovery. 2 tests.

### Added — HTTP/1.1 keep-alive connection reuse (IMP-keepalive)

- The server now supports HTTP/1.1 persistent connections: a single worker thread
  can serve up to 100 sequential requests on one TCP connection before closing it.
  Per-connection `conn_buf` carries read-ahead bytes across requests so pipelined
  data isn't discarded. HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close;
  `Connection: close` from the client or any error response closes immediately.
  SSE streaming always closes after the stream. The existing slow-loris timeout
  guard and body-size cap still apply to every request in the pipeline.
  std-only; 4 tests (two requests on one connection, `Connection: close` termination,
  `Connection: keep-alive` header present, HTTP/1.0 defaults to close).

### Added — `logprobs: null` in choice objects (IMP-logprobs-field)

- Both the buffered response choice and every streaming chunk choice now include
  `logprobs: null`, completing OpenAI choice-object parity. OpenAI always emits this
  key (null unless logprobs were requested); strict client schema validators that
  expect the field present now accept Pasture's responses. std-only; 2 tests.

### Added — `model` field in streaming chunks (IMP-chunk-model)

- Every SSE chunk (delta, stop, and usage) now carries the `model` field with the
  requested model name, matching the OpenAI streaming contract. Clients that use the
  per-chunk model for display, logging, or routing decisions now work correctly.
  All chunks of one stream share the same model value computed at stream start.
  std-only; 3 tests (delta chunk, usage chunk, cross-chunk consistency).

### Added — `system_fingerprint` on all responses and stream chunks (IMP-fingerprint)

- All buffered chat responses and every SSE chunk (including the final usage chunk) now
  carry a `system_fingerprint` field — a deterministic `fp_pasture_XXXXXXXX` string
  derived from the model name via FNV-1a truncated to 32 bits. OpenAI clients that key
  on this field for caching invalidation or change detection now behave correctly.
  All chunks of one stream share the same fingerprint (computed once at stream start).
  std-only; 4 tests (determinism, response shape, chunk shape, stream consistency).

### Added — per-connection socket timeout / slow-loris guard (IMP-timeout)

- `serve` now applies a per-connection read **and** write timeout
  (`PASTURE_REQUEST_TIMEOUT`, seconds, default `30`; `0` disables). A slow or dead
  client previously pinned one of the bounded worker threads indefinitely; a handful
  could exhaust the pool (slow-loris DoS). A timed-out read now returns **408** and
  frees the worker. Continuation of the existing DoS caps (body/header/recursion).
  std-only; 3 tests incl. a fast 50 ms timeout round-trip. Grounded in ADR-046.

### Added — `GET /v1/models/{id}` single-model retrieve (IMP-model-retrieve)

- The OpenAI `models.retrieve(id)` endpoint is now served: `/v1/models/{id}` returns the
  model object (`{id,object:"model",owned_by:"pasture"}`) when the id is configured, or a
  `404` error envelope otherwise. The bare `/v1/models` list path is unchanged. Query
  strings and trailing slashes are tolerated. Purely additive, std-only; 5 tests.
  Grounded in ADR-045.

### Fixed — unique completion ids (IMP-completion-id)

- Responses now carry a unique `id` (`chatcmpl-…`) instead of the constant
  `"pasture"`, matching OpenAI; logging/tracing/de-dup tooling keys on this. For
  streaming, one id is generated per stream and shared by every chunk (including the
  usage chunk). Uniqueness via a process-global atomic counter; std-only. 2 tests
  (uniqueness, per-stream id consistency). Grounded in ADR-044.

### Added — structured-output (JSON mode) passthrough (IMP-response-format)

- The proxy now **forwards `response_format`** (OpenAI JSON mode and `json_schema`
  structured outputs) to the backend instead of dropping it — every peer (OpenAI,
  vLLM, LM Studio, Ollama) supports it, so a JSON-mode request previously returned
  free-form text. Forwarded verbatim for OpenAI/OpenAI-compat; translated for Ollama
  (`{"type":"json_object"}` → top-level `format:"json"`; `json_schema` → the schema).
- Added a JSON **serializer** (`JsonValue::to_json_string`) — the dependency-free JSON
  module could parse but not round-trip; it emits sorted keys (deterministic), reused
  for the cache key, which now includes `response_format`. std-only; 9 tests. ADR-043.

### Added — streaming token usage (IMP-stream-usage)

- When a streaming request sets `stream_options.include_usage` (an OpenAI feature
  also supported by LiteLLM/vLLM), the SSE stream now emits a final chunk with an
  empty `choices` array and a `usage` object (`prompt_tokens`/`completion_tokens`/
  `total_tokens`) before `data: [DONE]`. Clients that track cost or length on
  streamed responses previously got no token counts. Off unless requested, so the
  default stream is unchanged; std-only. New `build_openai_usage_chunk`; 4 tests
  (parse, build, stream round-trips with and without the flag). Grounded in ADR-042.

### Added — CORS support for browser clients (IMP-cors)

- **`PASTURE_CORS_ORIGINS`** (comma-separated allow-list, or `*`): opt-in CORS so
  browser-based UIs (Open WebUI web, custom dashboards) can call the proxy — peers
  (Ollama, LM Studio, LiteLLM) all support this. `OPTIONS` preflight is answered with
  `204` + `Access-Control-Allow-*` *before* the auth/rate-limit gate (preflight is
  credential-free); `Access-Control-Allow-Origin` is reflected on all responses,
  including SSE; a specific origin also sends `Vary: Origin`.
- **Off by default** — a localhost server with permissive CORS is reachable by any
  website the user visits, so it must be enabled deliberately. Deterministic, std-only;
  8 tests (policy parse/match, header building, OPTIONS preflight, reflected header).
  Grounded in ADR-041.

### Added — optional auth + rate-limit for exposed deployments (IMP-15)

- **`PASTURE_AUTH_TOKEN`**: when set, all `/v1/*` requests must present
  `Authorization: Bearer <token>` (constant-time comparison); `/health` stays
  exempt for liveness probes. Missing/invalid → `401` (OpenAI error envelope).
- **`PASTURE_RATE_LIMIT`** (requests/minute): a global std-only token bucket
  (continuous refill, burst = the budget) caps `/v1/*`; over-limit → `429`.
- Both are evaluated by a single pure `check_gate` before route dispatch and are
  **off by default**, so the zero-config single-user localhost path is unchanged.
  `serve` now warns when bound to a non-localhost address without a token.
- New `ratelimit` module (deterministic, clock-split, no-sleep tests); 12 tests.
  Closes the auth/rate-limit gap noted in earlier release notes. Grounded in ADR-040.

### Fixed — sampling parameters were silently dropped (IMP-sampling)

- The proxy now **forwards client sampling parameters** to the backend instead of
  ignoring them — parity with LiteLLM/OpenRouter/Ollama/LM Studio/vLLM. Recognised:
  `temperature`, `top_p`, `max_tokens` (and the `max_completion_tokens` alias),
  `stop` (string or array), `seed`, `presence_penalty`, `frequency_penalty`.
  Previously a client asking for `temperature:0` (determinism) or `max_tokens`
  (cost/length cap) was silently ignored — a correctness gap vs every peer.
- Per-backend serialisation: OpenAI/OpenAI-compat as top-level fields; Ollama under
  `options` (length cap as `num_predict`); Anthropic honours the client `max_tokens`
  (was hardcoded 1024) plus `temperature`/`top_p`/`stop_sequences`.
- The **response cache key now includes the sampling params**, so a `temperature:0`
  answer is never served to a `temperature:1` request. Non-finite numbers are
  rejected at parse time. New `SamplingParams` type; 11 tests. Grounded in ADR-039.

### Added — live metrics endpoint (IMP-metrics / IMP-16)

- New **`GET /v1/stats`**: a live JSON snapshot of the cost-log counters —
  `total`, per-route counts (`local`/`cloud`/`cache`), `cloud_rate`, `cache_rate`,
  `prompt_tokens`, `completion_tokens`, `cloud_cost_usd` (`object:"pasture.stats"`).
  Read-only and PII-free (I3), reusing `cost::summarize`; a missing cost log reads
  as all-zeros. Gives a live observability view without parsing JSONL by hand;
  previously this was only available via the `stats` CLI command. Purely additive,
  std-only. New `Proxy::handle_stats` + pure `build_stats_response`; 4 tests
  (shape, empty-log, counted requests, socket round-trip). Grounded in ADR-038.

### Added — self-improvement ledger (IMP-13)

- New **`IMPROVEMENTS.jsonl`**: a machine-readable, causal record of every change
  (`change` / `reason` / `effect` / `status` / `grounding`), replacing prose
  scattered across CHANGELOG/ARCHITECTURE/COMPETITIVE with a queryable asset.
- New **`src/improve.rs`**: std-only parser/summarizer over the crate's own JSON
  reader, with lifecycle statuses (`shipped`/`deferred`/`retired`).
- New **`pasture improvements [path]`** command: summarize + list the verified
  change history (counts by status; full causal record per entry).
- A compile-time test (`include_str!` + validator) asserts the bundled ledger
  parses and every entry is a valid, explainable improvement — the record is
  CI-checked and cannot rot silently.
- New **SELF_IMPROVEMENT.md**: honestly maps the recursive-self-improvement framing
  (`RSI = Search × Verification × Compression`; the asset is verified history, not
  the model) onto Pasture's existing mechanisms (cost log = Trace Store, `eval` =
  Hidden Benchmark, `cargo test` = Verifier, IMP→ADR→code = Skill Compiler), and
  lists the anti-goals rejected to protect I1–I5 / ADR-002 / ADR-004 (P2P compute,
  learned routers, evolution engines, self-generating infra, content logging).
- Grounded in ADR-037. Std-only, zero new dependencies.

### Added — Privacy/PII detection hardening (10 categories)

Expanded `privacy.rs` from 7 to **10 detection categories** — all std-only, zero-dep:

- **`pem_key`** (new): detects `-----BEGIN … PRIVATE KEY-----` blocks (RSA, EC,
  OPENSSH, PKCS8 etc.) with near-zero false positives. SSH private keys, TLS keys,
  any PEM secret pasted into a prompt are now caught before reaching the cloud.

- **`url_credential`** (new): detects embedded credentials in URLs of the form
  `scheme://user:password@host` (postgres, mysql, redis, ftp, …). Requires a
  non-empty password part; `http://host:8080/` (port-only) is NOT flagged.

- **`env_secret`** (new): detects `KEY=value` / `export KEY=value` lines where the
  variable name contains secret-sounding substrings (pass, secret, token, auth,
  credential, private, pwd, _key, api_key, apikey). Trivial values (empty, null,
  true/false) are excluded. Catches `.env` file pastes, shell session sharing, etc.

- **`keyword`** expanded: +14 EN terms (bearer token, auth token, refresh token,
  signing key, encryption key, bank account, account number, routing number, swift
  code, IBAN, national id, taxpayer id, driver's license, date of birth) +10 JA terms
  (生年月日, 口座番号, 保険証, 年金番号, 運転免許, 在留カード, 住所, 氏名, 電話番号,
  銀行口座, 個人番号) — now the most comprehensive EN/JA PII keyword list for
  routing proxies.

- **`api_key` prefixes** expanded: +9 vendor-specific prefixes (Stripe `sk_live_`/
  `sk_test_`/`rk_live_`/`whsec_`, SendGrid `SG.`, Google OAuth `ya29.`, npm `npm_`,
  DigitalOcean `dop_v1_`, HashiCorp Vault `hvs.`, Cloudflare `v1.0-`). Total: 20+
  prefixes covering all major SaaS API key formats.

26 new tests (259 total). SPEC.md §5 updated with the full category table.

### Added — GPU-less / local-only PC enhancement

- **`PASTURE_LOCAL_ONLY=1` (local-only mode):** disables the cloud backend entirely —
  all traffic stays on-device regardless of content signals or prompt length. Ideal for
  air-gapped machines, privacy-first setups, or PCs without a GPU. The routing engine
  still applies privacy rules (no change needed: sensitive content was already kept
  local). Config file key: `local_only = true`. `with_local_only` builder on
  `RoutingEngine`; covered by 4 new routing tests.

- **`PASTURE_LOCAL_FAST_MODEL` — dual-local model routing:** configure a second, smaller
  local model (e.g. `qwen2.5:1.5b` or `phi3:mini`) that handles simple short queries
  (no hard signals, < `PASTURE_FAST_THRESHOLD` estimated tokens, default 50). Harder
  prompts still go to the main local model. Lets CPU-only machines trade quality vs.
  speed per query with zero cloud cost. Config: `local_fast_model = <name>`,
  `fast_threshold = <N>`. `Proxy::with_fast_model` builder; test coverage in proxy.

- **`PASTURE_INJECT_CONTEXT=1` — PC-assistant context injection:** prepends a system
  message containing the current UTC date and OS name before each completion request.
  Existing system messages are merged rather than duplicated. This grounds lightweight
  local models with the information they otherwise lack (date, environment) so they can
  answer scheduling, file-path, or system questions accurately. Implemented via
  `inject_context_into` + `utc_date_str` (std-only, zero-dep Gregorian calendar).
  `Proxy::with_inject_context` builder; date calc tested for epoch + Y2K + 2026-06-08.

- **Ultra-light model tier in `pasture models`:** when no GPU is detected and RAM <
  8 GB, the command now shows a CPU-only tier with Phi-3-mini, Gemma-2-2B,
  Qwen2.5-1.5B/0.5B, and TinyLlama — the lightest models that still produce useful
  output — plus tips on `PASTURE_LOCAL_ONLY` and `PASTURE_INJECT_CONTEXT`. EN + JA.

### Added — API-surface parity & tool-aware routing
- `GET /v1/models` (IMP-8): OpenAI-compatible list of the configured local (and
  cloud) model ids. Many clients probe this on connect; it previously 404'd. Purely
  additive, std-only. New `build_models_response` + `Proxy::with_models` (de-dups,
  drops empties), unit-tested; empty list still returns a valid `{"object":"list",…}`.
- **`POST /v1/embeddings` (IMP-8 completion, ADR-034):** clients such as LlamaIndex,
  LangChain, and semantic-cache implementations call this endpoint; it now routes to
  the local backend (Ollama `/api/embed` or OpenAI-compat `/v1/embeddings`) and
  returns the standard OpenAI embeddings shape. Cloud escalation does not apply.
  New `Backend::embeddings` trait method with `Ollama` + `OpenAiCompat` impls and a
  deterministic `MockBackend`; `handle_embeddings`, `parse_embeddings_request`,
  `build_embeddings_response`, `fmt_float_array` (finite-safe) in `proxy.rs`.
  Std-only, zero new dependencies. SPEC.md §3.2b is now normative for this endpoint.
- Tool/function-calling awareness (IMP-10): requests with a non-empty
  `tools`/`functions` array are detected in `parse_request` and treated as a hard
  routing signal, escalating to cloud where tool use is reliable. Gated by the same
  `code_to_cloud` rule and still overridden by privacy (sensitive stays local).
  Fields pass through unchanged. New `RoutingEngine::decide_full`; deterministic,
  std-only. Tests: tool detection, cloud escalation, privacy precedence, rule-off.
- Grounded in the peer/API-surface and routing analysis (COMPETITIVE.md / RESEARCH.md).

### Added — specification & conformance hardening
- New **SPEC.md**: the normative HTTP API + routing contract (endpoints, request/
  response schemas, error model, routing order, privacy, limits, config, cost log).
- Writing the spec surfaced three gaps, now fixed (ADR-033):
  - Error responses use the OpenAI envelope `{"error":{"message","type"}}` (were a
    flat `{"error":"…"}`), via `build_error_response` + `ProxyError::kind`.
  - Chat responses and SSE chunks now include the OpenAI `created` timestamp.
  - The request reader rejects oversized bodies with **413** (`MAX_BODY_BYTES`,
    16 MiB) instead of an unbounded read — a remote-DoS guard (IMP-21, partial).
  - Covered by socket round-trip tests (200/400/404/413 + envelope + `created`).

### Added — cloud resilience (IMP-9)
- Cloud requests now retry transient failures (connection/timeout, and provider
  **5xx**, newly classified as retryable `Transport` vs non-retryable 4xx) with
  exponential backoff (`PASTURE_CLOUD_RETRY`, default 2), and on final failure fall
  back to the local backend when available instead of erroring the request. New pure
  `complete_with_retry` + `is_retryable` + `http_status_error`, unit-tested with a
  flaky mock (no sleeps). std-only; the HTTPS path stays behind the `cloud` feature.

## [0.26.0] - 2026-06-05

### Changed — concurrent proxy (throughput)
- `serve` now handles connections concurrently with a bounded worker pool
  (size = available CPU parallelism, clamped 2..=32) instead of one connection at
  a time. The accept loop feeds a bounded queue, so a connection flood applies
  backpressure rather than spawning unbounded threads. Shared state (the cache)
  is already behind a mutex; std-only, zero new dependencies.
- Verified: 8 concurrent requests against a 150 ms/req backend finish in ~615 ms
  (serial would be ~1200 ms), all correct.

### Docs
- Added publication docs for open-sourcing: SECURITY.md, CONTRIBUTING.md,
  CODE_OF_CONDUCT.md, and GitHub issue/PR templates. Cargo.lock is committed
  (binary crate).

### Notes
- The default listen address stays 127.0.0.1, so the proxy is not network-exposed
  by default. (Auth/rate-limiting for exposed deployments remain future work.)

## [0.25.0] - 2026-06-05

### Added
- `pasture stats` now shows the cascade confidence distribution when logged:
  n, mean, min, p10 and median of the local mean log-probability, plus a hint to
  tune escalation with `calibrate --logprob`. Lets you see your local model's
  confidence spread before choosing `PASTURE_CASCADE_LOGPROB`. New pure
  `logprob_summary` / `LogprobStats`, unit-tested. Numbers only, never content (I5).

## [0.24.0] - 2026-06-04

### Added — calibrate the cascade confidence threshold from your own data
- `pasture calibrate --logprob [--target R]`: recommends `PASTURE_CASCADE_LOGPROB`
  from the distribution of local-answer mean log-probabilities recorded during
  cascade, so about R of answers escalate (default 0.2). Replaces the hand-set
  default with a value derived from your own model's behaviour.
- The cascade now records the local mean log-probability to the cost log
  (`"logprob"` field) — a single number, never content (I5). New pure
  `calibrate_logprob_threshold` (lower-tail quantile), unit-tested.

### Why
- The routing literature (UCCI arXiv 2605.18796; "Is Escalation Worth It?"
  arXiv 2605.06350) stresses that cascade confidence scores are uncalibrated and
  their thresholds must be tuned per workload — hand-tuning is the status quo
  pain point. Full calibration (isotonic regression) needs correctness labels a
  single local user lacks; calibrating to an escalation *budget* from the user's
  own logprob distribution is the honest label-free analogue. Verified
  end-to-end: 20 logged samples yield distinct thresholds for 30% vs 50% targets.

## [0.23.0] - 2026-06-04

### Added — research-backed cascade confidence (core routing)
- The FrugalGPT-style cascade now escalates on the local model's **mean token
  log-probability** when the backend can report it (OpenAI-compatible / LM Studio
  via `logprobs`), falling back to the text heuristic otherwise. Grounded in
  arXiv 2605.02241 (May 2026), which finds average log-probability a
  training-free signal that matches or beats supervised routers (RouteLLM) for
  local->cloud routing in-distribution — exactly Pasture's use case.
- `Backend::complete_scored` (default: no signal); `OpenAiCompatBackend`
  requests `logprobs` and returns the mean. `cascade::should_escalate`,
  `cloud::mean_logprob_from_openai`, `Provider::build_body_logprobs`,
  `JsonValue::as_f64` — all pure and unit-tested.
- `PASTURE_CASCADE_LOGPROB` (default -1.0): escalate when mean logprob drops
  below this. Surfaced in `pasture config`. Verified end-to-end: a low-logprob
  local answer escalates, a high-logprob one stays local.

### Notes
- The default threshold (-1.0) is a sensible starting point; the paper notes the
  optimal cutoff is model-dependent, so it is env-tunable. Sensitive content is
  still never cascaded (privacy).

## [0.22.1] - 2026-06-04

### Added / Changed
- CI: added a `gitleaks` secret-scanning job (I4) alongside fmt/clippy/test/build
  and `cargo audit`.
- Docs: `RELEASE_CHECKLIST.md` mapping the §8 release gate to current state and
  flagging the irreversible publish steps that require maintainer approval.

### Fixed
- Removed stray runtime cost-log files (`*-cost.jsonl`) that had leaked into the
  working tree from local demos (already `.gitignore`d; now also excluded from
  release tarballs).

## [0.22.0] - 2026-06-04

### Added
- `pasture config`: prints the effective configuration (resolved env vars and
  defaults) — listen address, local backend/model, routing threshold and its
  source, cloud provider/model, cascade/cache settings, cost-log path and
  language. Complements `doctor` for debugging setup after the `PASTURE_*`
  rename. API keys are never printed — only whether each is set (I5; verified a
  secret value never appears in the output).

## [0.21.1] - 2026-06-04

### Security (independent review pass: parser hardening)
- The zero-dependency JSON parser now bounds recursion (MAX_DEPTH = 128).
  `parse_value -> parse_object/array -> parse_value` was unbounded, so a client
  could send deeply nested JSON (e.g. `[[[[...`) to the proxy and overflow the
  stack, aborting the process — a remote denial of service. Over-deep input now
  returns a graceful parse error. Verified: a 200k-deep body is rejected with
  "maximum nesting depth exceeded" and the server stays up. Legitimate chat
  payloads nest only a few levels.

## [0.21.0] - 2026-06-04

### Changed — BREAKING: project renamed Kabosu -> Pasture
- Crate, binary, and repo renamed to `pasture`
  (github.com/shizukutanaka/pasture). Invoke as `pasture ...`.
- Environment variables renamed `KABOSU_*` -> `PASTURE_*` (e.g.
  `PASTURE_THRESHOLD`, `PASTURE_LOCAL_BACKEND`, `PASTURE_OPENAI_API_KEY`).
  Old `KABOSU_*` variables are no longer read — update your config.
- Proxy response: `x_kabosu_route` header field -> `x_pasture_route`; response
  `id` -> `"pasture"`.
- No behavioural changes beyond the rename: routing, privacy, streaming, cache,
  cascade, calibration and all CLI commands are unchanged. 185 tests pass
  (default + cloud), clippy/fmt clean.

## [0.20.0] - 2026-06-04

### Added
- True SSE streaming for the cloud backend (feature = "cloud"), completing
  streaming across every route: Ollama, LM Studio/OpenAI-compatible, cloud
  OpenAI, and cloud Anthropic. `HttpsCloudBackend::stream_complete` sends
  `stream: true` and forwards deltas as they arrive.
- Provider-agnostic, transport-agnostic `read_sse_body` (works over any `Read`)
  with `parse_anthropic_stream_line` and `Provider::parse_stream_line`. The
  streaming-assembly logic is unit-tested offline over in-memory cursors
  (OpenAI multi-delta, Anthropic text deltas, chunked size-lines ignored, error
  status). Chunked-transfer size lines are skipped because they never start with
  `data:` (one SSE event per chunk, which OpenAI and Anthropic honour).

### Verified
- The TLS streaming transport was exercised live against api.anthropic.com:
  connect, handshake, write, header parse and the non-2xx path all work (a dummy
  key surfaces the real `HTTP 401: invalid x-api-key`). Happy-path deltas are
  covered by the offline unit tests above.

## [0.19.0] - 2026-06-04

### Security (independent review pass: privacy classification gaps)
- Japanese domestic phone numbers are now detected. Previously `looks_like_phone`
  required a leading `+` (international form), so common JP numbers like
  `090-1234-5678` or `03-1234-5678` were classified non-sensitive and could be
  routed to the cloud — a real PII leak for a Japanese-first product (I5/I10).
  Now matches JP mobile (070/080/090 + 8 digits) and hyphenated domestic
  numbers, while bare non-phone digit runs are not tripped.
- JWT / bearer tokens (`eyJ.........`) are now detected (new `jwt` category).
- Added Slack user (`xoxp-`) and GitLab (`glpat-`) token prefixes.

### Notes
- Detection emits category labels only — never the matched value (I5). Sensitive
  content stays local even under a forced `--cloud`, unless the operator opts in
  with `PASTURE_ALLOW_SENSITIVE_CLOUD`.

## [0.18.0] - 2026-06-04

### Added — IMP-2 data-driven threshold calibration
- `pasture calibrate [--target <rate>]`: recommends a `PASTURE_THRESHOLD` from the
  distribution of prompt sizes in your own cost log, so that roughly `<rate>` of
  similar prompts route to the cloud (default 0.2). Uses only token counts
  already logged (no prompts, no PII — I5). New `calibrate` module with a pure,
  unit-tested `calibrate_threshold(tokens, target) -> (threshold, achieved)`.
- `PASTURE_THRESHOLD` (config key `threshold`) overrides the hardware-derived
  routing threshold; `RoutingEngine::with_threshold`. Engine construction for
  route/serve/chat now goes through one `make_engine` helper.

### Notes
- This is an honest, offline, zero-dependency analogue of cost/quality
  calibration (RouterBench, UCCI). Learned routers (RouteLLM, Hybrid LLM) achieve
  more but require preference/quality-gap labels a single local user lacks. The
  recommendation is length-only; content signals (reasoning/code/privacy)
  escalate further, so the realised cloud rate is at least the reported figure.

## [0.17.0] - 2026-06-04

### Changed (independent review pass: cross-lingual routing fix)
- Token estimation is now script-aware. CJK (Chinese/Japanese kana & ideographs)
  and Hangul tokenize at roughly one token per character, while Latin text
  averages ~4 chars/token. The previous chars/4 estimate undercounted Japanese
  ~4x, so long non-Latin prompts wrongly stayed on the local model. Now they
  escalate appropriately. Verified: a 350-char Japanese prompt is estimated at
  ~332 tokens (was ~88) and correctly routes to cloud at threshold 300. Latin
  estimates are unchanged. This matters for a Japanese-first product (I10).

## [0.16.0] - 2026-06-04

### Added
- True SSE streaming for the OpenAI-compatible local backend (LM Studio,
  llama.cpp server, vLLM, ...). `OpenAiCompatBackend::stream_complete` sends
  `stream: true` and forwards each `delta.content` as it arrives, instead of
  buffering the whole answer. Verified end-to-end: LM Studio-style SSE deltas
  are re-emitted incrementally as pasture `chat.completion.chunk` frames.
- `cloud` helpers `build_body_stream` and `parse_openai_stream_line` (pure,
  unit-tested); `OpenAiStreamEvent`.

## [0.15.1] - 2026-06-04

### Fixed (independent review pass)
- Server-reported errors from OpenAI-compatible backends are now surfaced
  verbatim. Previously a provider/LM Studio error body (e.g. a 500 for a model
  id that doesn't match a loaded model) was flattened into a generic "missing
  content" message; now the response carries `server error: <message>` so the
  cause is obvious. Applies to both cloud and LM Studio paths.

### Reviewed (no change needed)
- Audited proxy request reading: `Content-Length` bodies are fully read across
  multiple TCP reads (large prompts are not truncated), with a 1 MiB header
  guard.

## [0.15.0] - 2026-06-04

### Added
- LM Studio support (and any OpenAI-compatible local server: llama.cpp server,
  vLLM, LocalAI). New `OpenAiCompatBackend` (plain HTTP, reuses the OpenAI
  request/response shaping) lets pasture use such a server as its local engine.
  Select with `PASTURE_LOCAL_BACKEND=lmstudio` (or `openai`); endpoint via
  `PASTURE_LOCAL_OPENAI_URL` (default http://127.0.0.1:1234/v1).
- `connect lmstudio`: explains LM Studio's role and how to use it as pasture's
  engine. `doctor` and `up` now understand the OpenAI-compatible engine
  (probe `/v1/models`, no Ollama-specific auto-pull). Localised (EN + JA).
- `doctor` helpers `parse_base_url`, `probe_openai`, `parse_openai_model_names`
  (pure, unit-tested).

### Notes
- Ollama remains the default local backend. Verified end-to-end: with
  `PASTURE_LOCAL_BACKEND=lmstudio`, a request is forwarded to the local OpenAI
  server and returned with `x_pasture_route: "local"`.

## [0.14.0] - 2026-06-04

### Added (grounded in current research, June 2026)
- `connect [app]` command: exact, localized setup steps to point a client at
  the proxy. Apps: Open WebUI, Continue (VS Code), Cursor, and generic OpenAI
  SDK/curl — each verified against current docs. No arg lists the apps and
  prints the generic snippet.
- `models` command: recommends current local models by RAM tier and highlights
  the tier matching the detected RAM (8 GB: llama3.2 / gemma3:4b / qwen3:4b;
  16 GB: llama3.1:8b / qwen2.5-coder:7b; multilingual: Qwen family).
- i18n keys for `connect.*` / `models.*` (EN + JA), key parity enforced.

### Changed
- Default local model is now `llama3.2` (3B, runs on 8 GB, the current small
  general default) instead of `llama3`. Override with `PASTURE_LOCAL_MODEL`.

### Notes
- Ollama already exposes an OpenAI-compatible endpoint; pasture's value is the
  routing layer (local<->cloud, privacy gating, cascade, cache) and a single
  base_url in front of both — reflected in the connect guidance.

## [0.13.0] - 2026-06-04

### Added (fewer steps for beginners)
- `up` now auto-starts Ollama: if the daemon is not running it spawns
  `ollama serve`, waits for it to come up, then continues. If Ollama is not
  installed it falls back to the clear install message.
- On startup, `serve`/`up` print a localised "connect your app" banner showing
  the base_url and an `OPENAI_BASE_URL`/`OPENAI_API_KEY` example — closing the
  last gap ("now how do I point my app at it?"). New i18n keys
  `connect.help`, `up.starting_ollama`, `up.ollama_started` (EN + JA).

## [0.12.0] - 2026-06-04

### Added
- Internationalisation (I10): zero-dependency `i18n` module with Japanese and
  English catalogs (`namespace.component.key`), `{name}` interpolation, and
  English fallback. Language auto-detected from `PASTURE_LANG`/`LC_ALL`/`LANG`.
  The welcome screen and all `doctor`/`up` output are now localised; a test
  enforces key parity between the two catalogs.
- `up` command: the minimal-steps path to running. Verifies Ollama is up,
  auto-pulls the configured model if missing (`ollama pull`), then starts the
  proxy — one command from "Ollama installed" to "serving".
- Bootstrap scripts `install.sh` / `install.ps1`: build and place the binary on
  PATH, then point the user at `pasture up`.

## [0.11.0] - 2026-06-04

### Added (beginner-friendly)
- `doctor` command: one-shot environment check. Probes whether Ollama is
  running, whether the configured model is installed, whether the proxy port is
  free, and (cloud build) whether an API key is set — each failure prints the
  exact fix command. `doctor` module (`probe_ollama`, `parse_model_names`,
  `has_model`, `port_available`); JSON parsing pure & unit-tested.
- `setup` command: friendly welcome + the environment check.
- Running `pasture` with no arguments now prints a 3-step welcome (and exits 0)
  instead of a terse usage dump.
- `GETTING_STARTED.md`: a thorough zero-to-running guide in Japanese (glossary,
  3-step quickstart, troubleshooting mapped to `doctor` output, optional cloud,
  FAQ).

## [0.10.0] - 2026-06-04

### Added
- `stats` command and cost-log analysis (`cost::read_log`, `parse_log_line`,
  `summarize`). Summarizes the JSONL cost log: request counts per route
  (local/cloud/cache), cloud rate, cache hit rate, token totals, cloud spend,
  and backend calls saved by the cache. Offline, zero-dependency; observed
  cloud rate is the empirical basis for tuning the routing threshold (IMP-2
  groundwork). Malformed lines are skipped; a missing log is reported cleanly.

## [0.9.1] - 2026-06-04

### CI / Build
- GitHub Actions CI (`ci.yml`): fmt + clippy (`-D warnings`) + test + release
  build across a `default` / `cloud` feature matrix, plus `cargo audit`. Pinned
  to MSRV 1.75.0 via `rust-toolchain.toml` (catches edition2024 regressions).
- Tag-triggered `release.yml`: builds the zero-dependency binary for
  linux/macOS/windows and attaches it to the GitHub Release. No registry
  publish, no secrets. Automates the §8 release gate up to (manual) signing.

## [0.9.0] - 2026-06-04

### Added
- Exact-match response cache (IMP-6). Opt-in via `PASTURE_CACHE=<n>` (cache
  size; 0 disables). Identical non-sensitive requests return a stored answer
  without calling any backend, saving cloud cost; cache hits are labelled
  `x_pasture_route: "cache"` and logged at zero cost. Bounded FIFO eviction,
  zero-dependency (std hasher). Verified end-to-end: a repeated request routes
  `local` then `cache`.
- `cache` module (`request_key`, `ResponseCache`); `Proxy::with_cache`;
  `Config::cache_size`.

### Changed
- Response/route labels are now strings, allowing `local` / `cloud` / `cache`
  in `x_pasture_route` for both buffered and streaming responses.

### Notes
- Sensitive prompts are never cached (I5).

## [0.8.0] - 2026-06-04

### Added
- Routing evaluation harness (IMP-5, RouterBench-style). New `eval` command and
  `eval` module: runs an 18-case labelled set (EN+JA: plain->local,
  hard->cloud, sensitive->local) reporting accuracy, cloud rate, and false/
  missed escalations; plus a token-threshold sweep showing the cost/locality
  trade-off. Offline, deterministic, zero-dependency.
- Baseline: 100% routing accuracy on the labelled set; sweep cloud-rate ranges
  from 75% (threshold 50) to 0% (threshold 2000).

## [0.7.0] - 2026-06-04

### Added
- Cascade routing (IMP-1, FrugalGPT arXiv:2305.05176 / Confident-or-Seek
  arXiv:2502.04428). Opt-in via `PASTURE_CASCADE=1`: the request is answered by
  the local model first, and only escalated to the cloud when the local answer
  looks low-confidence (v0 heuristics: empty/near-empty or EN/JA uncertainty &
  refusal markers). If the cloud call fails, the local answer is returned
  (graceful fallback).
- `cascade` module (`is_low_confidence`, pure & tested); `Proxy::with_cascade`;
  `Config::cascade`.

### Notes
- Cascade never escalates sensitive content (privacy wins) and is skipped for
  streaming requests (the local answer cannot be un-sent). It requires a cloud
  backend (`--features cloud` + API key) to escalate.

## [0.6.0] - 2026-06-04

### Added
- Cloud backends over real HTTPS (ADR-005 resolved), gated behind the opt-in
  `cloud` feature. Providers: OpenAI and Anthropic. Request/response shaping
  and HTTP framing (Content-Length + chunked) are pure and unit-tested;
  verified end-to-end against api.anthropic.com (TLS handshake + 401 path).
- `cloud` module; `chat`/`serve` use the cloud backend when the feature is
  built and an API key is present, enabling real local<->cloud routing.
- Config: `cloud_model` (+ `PASTURE_CLOUD_MODEL`). BYOK keys from
  `PASTURE_OPENAI_API_KEY` / `PASTURE_ANTHROPIC_API_KEY` (never logged, I5).

### Changed
- The default build remains zero-dependency and MSRV 1.75. The `cloud` feature
  adds a pinned, MSRV-1.75-compatible TLS stack (native-tls 0.2.12 /
  openssl 0.10.64 / openssl-sys 0.9.102) using the system OpenSSL.

### Security / Governance
- Dependency addition is feature-gated and pinned (supply-chain review, G7).
  No live keys used; verification used a dummy key only.

## [0.5.0] - 2026-06-04

### Added
- Server-Sent Events streaming (IMP-7, competitor parity). The proxy now
  honours `"stream": true`, converting the local Ollama NDJSON stream into
  OpenAI `chat.completion.chunk` frames terminated by `data: [DONE]`. Non-
  streaming requests are unchanged.
- `Backend::stream_complete` (default emits the full answer as one chunk;
  Ollama overrides with true token streaming).
- `pasture chat` now streams output to the terminal as it arrives.
- Pure helpers `build_openai_chunk`, `sse_frame`, `parse_ollama_stream_line`.

## [0.4.0] - 2026-06-04

### Added
- Richer difficulty features (IMP-4, grounded in survey arXiv:2506.06579):
  routing now escalates to cloud on reasoning-depth markers, strict-format /
  code-generation requests, math-symbol density, and multiple questions
  (>= 3) — not just code fences and length. EN + JA markers. Decision reason
  lists the matched signals (labels only).
- `hard_signals`, `question_count`, `looks_mathy` (pure, tested).

## [0.3.0] - 2026-06-04

### Added
- Privacy-classification routing (IMP-3): prompts containing likely sensitive
  content (email, IPv4, credit-card via Luhn, phone, API-key prefixes, EN/JA
  keywords) are kept on the local model and never sent to the cloud. Overrides
  even a forced `--cloud`; errors instead of leaking when no local backend
  exists. Opt out with `PASTURE_ALLOW_SENSITIVE_CLOUD`.
- `privacy` module; `route`/`chat` now report sensitivity (labels only).
- Routing: `decide_with_sensitivity`, `with_allow_sensitive_cloud`, new
  `SensitiveButNoLocal` error.
- Config: `allow_sensitive_cloud`.

### Security
- Only category labels are surfaced or logged — never matched values (I5).

### References
- PRISM (arXiv:2511.22788); "sensitive data stays local" pattern in peers.

## [0.2.0] - 2026-06-04

### Added
- `donate` command and gentle, infrequent donation nudge (every 25 runs,
  stderr only, opt-out via `PASTURE_NO_NUDGE`).
- `refer [provider]` command surfacing operator-configurable cloud-provider
  affiliate links (`PASTURE_REF_<KEY>`); no codes shipped, no user identifiers.
- `monetize` module (donation/referral surfaces, run-count state).
- Cloudflare Worker (`worker/`) for a $1/month Stripe donation Checkout with
  webhook signature verification. Secrets via Worker bindings; stores no PII.
- Config: `donate_url`, `no_nudge`, `state_path` (+ `PASTURE_*` env).

### Security
- No Stripe keys in source; test-mode-first. Live mode / deploy gated behind
  explicit human approval (release-approval skill, Class C).

## [0.1.0] - 2026-06-04

### Added
- Hardware detection (RAM / CPU threads / GPU + VRAM) with safe fallbacks.
- Deterministic, hardware-adaptive routing engine (local vs cloud) with
  explicit `--local` / `--cloud` overrides.
- OpenAI-compatible proxy server (`POST /v1/chat/completions`, `GET /health`)
  built on the standard library only.
- Local inference backend over Ollama (plain HTTP).
- Minimal zero-dependency JSON parser/serializer.
- Structured JSONL cost logging (no PII).
- CLI: `hw`, `route`, `chat`, `serve`, `version`, `help`.
- 71 unit tests; clippy `-D warnings` clean; rustfmt clean.

### Security
- BYOK cloud credentials are not yet handled (cloud backend stubbed).

### Notes
- Cloud backend requires HTTPS/TLS and is deferred to a future release
  (ARCHITECTURE.md, ADR-005).

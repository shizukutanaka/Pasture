# Changelog

All notable changes to this project are documented here.
Format follows Keep a Changelog; versioning follows SemVer.

## [Unreleased]

### Fixed — Access log records real status for rejected streaming requests (ADR-192)

- The streaming branch logged `status:200` to the access log **before** running
  the stream handler — but that handler can reject with 400 (injection block),
  429 (budget block), or 502/503 (routing/backend errors) before any SSE byte.
  Those rejections were all silently logged as 200, hiding them from anyone
  auditing the access log.  `stream_chat_to_socket` now returns the effective
  HTTP status and the dispatch loop logs it; a mid-stream disconnect is still
  logged as 200 (the headers were sent).  Buffered logging unchanged.  2 new
  tests.  676 tests.  (ADR-192)

### Fixed — Injection-guard flag mode annotates streaming responses (ADR-191)

- In `flag` mode the buffered path adds `x_pasture_injection_flag:<label>` to the
  response, but the streaming path only logged to stderr — so a streaming client
  could not tell a flagged request from a clean one (and it contradicted SPEC
  §7.2).  Streaming flag mode now emits a leading SSE chunk carrying the flag (a
  top-level field beside the existing `x_pasture_route`), on the backend-stream,
  exact-cache, and semantic-cache paths.  `block` mode was already correct in
  both paths.  Buffered/off/block behaviour unchanged.  2 new tests.  674 tests.
  (ADR-191)

### Docs + Tests — SPEC.md brought current; drift-guard test (ADR-190)

- `SPEC.md` had drifted well behind the implementation: it omitted **21**
  `PASTURE_*` env vars, named a non-existent `PASTURE_PROXY_TOKEN` (real:
  `PASTURE_AUTH_TOKEN`), and invented per-input/output price vars (real: the
  single `PASTURE_CLOUD_PRICE_PER_1M`).  The spec is now current through
  ADR-189 — added the budget/spike guard (§7.1), injection guard (§7.2), OTel
  trace log (§9.1), the three request projections (§4), tool/function calling
  (§12), and reversible pseudonymization (§13).
- New std-only drift-guard tests in `config.rs`: every env var the config layer
  reads via `std::env::var("PASTURE_*")` must appear in a SPEC.md config-table
  row, and no table row may name a var the code never reads.  This makes
  doc/implementation divergence a test failure.  It immediately caught four
  further gaps while updating the spec.  2 new tests.  672 tests.  (ADR-190)

### Fixed — Pseudonymize restore covers cloud-generated `tool_calls` (ADR-189)

- When a cloud model responded with its own tool call that echoed a
  pseudonymized token it saw in the request history (e.g. `<EMAIL_1>` passed
  via the context), the `tool_calls` field of the response was forwarded to
  the client with the raw opaque token — not the real value.  Only
  `resp.content` was being restored; `resp.tool_calls` was not.  The same
  omission affected the streaming path, where the tool-calls SSE chunk was
  emitted verbatim.

  Both paths now restore tokens in the `tool_calls` field using the same
  mapping already in scope (`pseudo_mapping` / `cache_mapping`).  The
  streaming path reuses the `cache_mapping` clone maintained for cache
  restoration (ADR-147) — no new state.  2 new tests.  670 tests.  (ADR-189)

### Fixed — Pseudonymizer scrubs PII in tool-call argument JSON (ADR-188)

- When `PASTURE_PSEUDONYMIZE=1` (masked-cloud mode), the pseudonymizer only
  processed message `content`; `tool_calls_json` was cloned unchanged.  PII in
  tool-call arguments (e.g. `{"email":"alice@example.com"}` passed to a
  `send_email` tool) therefore reached the cloud provider unmasked — defeating
  the whole purpose of pseudonymization.  The old `replace_in_text` whitespace
  tokenizer could not have fixed this even if applied directly: inside compact
  JSON there is no whitespace, so the email is embedded in one large token and
  never matched.

  New `replace_in_json_strings` walks every `"…"` literal in the JSON, decodes
  escape sequences, **recursively descends** into strings that start with `{`
  or `[` (handling the `arguments` field, which is itself JSON-encoded JSON),
  and then applies `replace_in_text` to the innermost decoded values.  The same
  PII value appearing in both message content and tool-call arguments shares one
  stable token (single `Ctx`), so `restore()` recovers it from either site.
  Benign (non-PII) arguments pass through un-modified.  5 new tests.  668
  tests.  (ADR-188)

### Fixed — Privacy classifier scans tool-call arguments (ADR-187)

- Sensitivity was decided by `classify(routing_text())`, which joins only
  message `content`.  PII living **solely** in an assistant message's
  `tool_calls_json` — a credit card passed to a `charge_card` tool, an email or
  API key in tool arguments — was invisible to the classifier.  The request was
  then deemed non-sensitive and, because `has_tools` (and long content)
  escalates to cloud, the PII was sent off the machine — violating the
  guarantee that sensitive content stays local unless
  `PASTURE_ALLOW_SENSITIVE_CLOUD` is set.

  New `CompletionRequest::privacy_text()` = `routing_text()` + every assistant
  `tool_calls_json`; `classify()` now runs on it.  The routing difficulty/length
  decision still uses content-only `routing_text()`.  Tool *definitions*
  (`sampling.tools`) are excluded by design (developer schema, not conversation
  PII — classifying them would false-positive every tool request).  The change
  only ever marks **more** requests sensitive (kept local, also uncached) — the
  safe direction; non-PII tool requests route identically.  Both buffered and
  streaming paths covered.  2 new tests.  663 tests.  (ADR-187)

### Fixed — Cache key includes message-level `tool_calls` / `tool_call_id` (ADR-186)

- The exact-match cache key (`request_key`) hashed only each message's `role`
  and trimmed `content` (plus sampling-level `tools`/`tool_choice` from
  ADR-177), but **not** the message-level `tool_calls_json` (assistant tool
  calls, ADR-183) or `tool_call_id` (tool-result id, ADR-182).  Two multi-turn
  histories with identical message content but different tool-call arguments —
  e.g. `book(NYC)` vs `book(LON)` — collided to the same key, so the second
  conversation could be served the first's cached reply.  Tool requests do
  reach the cache: `has_tools` does not mark a request sensitive (sensitivity
  is content-only via `classify`), so they are cacheable.

  `request_key` now folds `tool_call_id` and `tool_calls_json` into the
  per-message hash (exact match; no whitespace normalisation for structured
  data).  The change can only split previously-colliding keys, never merge
  distinct ones — no valid hit is lost, and plain non-tool requests (both
  fields `None`) keep an unchanged key.  1 new test.  661 tests.  (ADR-186)

### Fixed — Spike-detector average resets on UTC day rollover (ADR-185)

- The daily token budget resets at UTC midnight (ADR-155), but the spike
  detector's running average did not.  The spike check compares each request
  against `cloud_token_sum / cloud_request_count`, and those counters
  accumulated for the entire lifetime of the process — an all-time average, not
  a recent one.  Stale history never decayed: early outliers permanently skewed
  the average, so a large-but-legitimate prompt late in a long session might
  never trip the spike, and a long quiet history made the detector hair-trigger.

  `roll_budget_day_if_needed()` already zeroes `today_cloud_tokens` on a strict
  UTC-day advance (backward-clock-safe CAS); it now also zeroes the two spike
  counters, so the spike half of the IMP-26 guard is day-scoped like the budget
  half.  No new state (no ring buffer / EWMA); deterministic and std-only.
  Cold-start already bypasses the spike check, so the post-reset state is the
  well-tested cold path.  1 new test.  660 tests.  (ADR-185)

### Fixed — Token estimation counts tool definitions / `tool_calls` (ADR-184)

- The token-estimate fallback (used for the cost log and budget/spike guard
  when a backend sends no usage chunk) joined only message `content` via
  `routing_text()`, so the `tools` schema (often hundreds of tokens), the
  `tool_choice`, and assistant `tool_calls` arguments — all really sent to and
  billed by the provider — were excluded.  Tool-heavy requests were therefore
  systematically under-counted in the cost log, the daily budget gauge/cap, the
  spike detector, and cloud spend — the unsafe direction for a budget control.

  New `CompletionRequest::estimation_text()` extends `routing_text()` with the
  serialised tool payload; the five token-accounting sites use it, while the
  routing-decision and privacy sites keep `routing_text()` (content-only, as
  `classify()` requires; tool presence already forces escalation).  The
  actual-usage path (ADR-173/175) is unaffected — actual counts still win.
  Also cleaned three pre-existing clippy warnings.  1 new test.  659 tests.
  (ADR-184)

### Fixed — Multi-turn tool calling: assistant `tool_calls` history (ADR-183)

- Assistant messages with `content:null` and a `tool_calls` array (the
  standard shape the client sends back in the second turn) were rejected at
  parse time because `extract_message_content` errored on `JsonValue::Null`.
  Even if they had parsed, the `tool_calls` array was silently dropped, causing
  every backend to reject the request (OpenAI: 400 missing `tool_calls`;
  Anthropic: 422 wrong schema).  This made the standard two-step agent loop
  impossible when the full conversation history was included.

  Now `extract_message_content` returns `Ok("")` for `content:null` (missing
  content still errors for user/system messages — unchanged).  `Message`
  carries `tool_calls_json: Option<String>`; `parse_request` extracts it from
  assistant messages; OpenAI/Ollama body builders emit `"content":null,
  "tool_calls":[...]`; the Anthropic builder emits the matching
  `content:[{type:"tool_use",...}]` blocks.  7 new tests.  658 tests.  (ADR-183)

### Fixed — Multi-turn tool calling: `tool_call_id` forwarding (ADR-182)

- `Message` struct only stored `role` and `content`, so `tool_call_id` from
  `role:"tool"` result messages was silently dropped.  OpenAI requires this
  field and returns 400 without it; Anthropic requires the result wrapped as
  `role:"user"` with a `tool_result` content block (and rejects `role:"tool"`
  entirely).  Both failures made the second turn of every tool-calling agent
  loop impossible through Pasture.

  Now `Message` carries `tool_call_id: Option<String>`; `parse_request`
  extracts it; OpenAI and Ollama body builders emit it; the Anthropic builder
  translates `role:"tool"` to the `tool_result` format.  4 new tests.
  (ADR-182)

### Fixed — OTel span `finish_reason` for tool-call responses (ADR-181)

- The OTel trace log recorded incorrect `finish_reason` for tool-call responses:
  `emit_cache_hit_span` and `finalize_streamed` hardcoded `"stop"` even when
  `tool_calls` was present; `run_completion` (buffered path) never set the
  attribute at all — it was silently absent from every non-streaming span.
  The SSE wire format was correct; only the trace log was wrong.
  New `finish_reason_for(resp)` helper centralises the logic; all three emission
  sites and two inline ternary expressions are updated to use it.  3 new tests.
  651 tests.  (ADR-181)

### Fixed — Anthropic `tool_choice` translation (ADR-180)

- ADR-179 omitted `tool_choice` for Anthropic, so `tool_choice:"required"` was
  silently ignored — Anthropic defaulted to `auto`, breaking agent loops that
  use `required` to force at least one tool call.  Now `"auto"` maps to
  `{"type":"auto"}`, `"required"` to `{"type":"any"}`, and a named-function
  object to `{"type":"tool","name":"..."}`.  The field is only emitted when
  `tools` is present (Anthropic rejects the combination otherwise).  `"none"`
  and `null` are still handled upstream (ADR-176).  4 new tests.  648 tests.
  (ADR-180)

### Fixed — Anthropic tool-use forwarding (ADR-179)

- ADR-177/178 delivered tool calling for OpenAI-compatible backends, but the
  Anthropic provider had four distinct defects.  (1) `build_body_opts` silently
  dropped `sampling.tools` — tool definitions never reached the Anthropic API.
  (2) `parse_response` required `content[0].text`, so a `tool_use` response
  (common for Anthropic's Claude 3+ models) errored with "missing content[0].text"
  instead of extracting the tool call.  (3) `parse_anthropic_stream_line` ignored
  `content_block_start` (carries tool id/name) and `input_json_delta` events,
  so the `ToolCallAccumulator` never received any fragments on the Anthropic
  streaming path.  (4) `build_body_stream` unconditionally appended
  `stream_options:{include_usage:true}` — an OpenAI-only field that causes
  Anthropic to return 400 on every streaming request.
  
  Now: `translate_tools_to_anthropic()` maps the OpenAI tools schema
  (`parameters`) to Anthropic's (`input_schema`); `parse_response` walks the
  content array and extracts both `text` and `tool_use` blocks; streaming
  `content_block_start` and `input_json_delta` events are emitted as
  `ToolCallDelta` events accumulated by the existing `ToolCallAccumulator`;
  `stream_options` is suppressed for Anthropic.  `tool_choice` translation
  is a follow-up (ADR-180).  10 new tests.  644 tests.  (ADR-179)

### Added — Streaming responses carry tool_calls (ADR-178)

- ADR-177 fixed tool calling for buffered requests, but the streaming path fed the
  callback only `delta.content`, dropping `delta.tool_calls` fragments — so streaming
  tool requests (the common agent case) produced an empty stream with no tool call.
  The cached-stream replay had the same gap (`tool_calls` ignored).  Now a
  `ToolCallAccumulator` merges streamed tool-call fragments by index, `read_sse_body`
  returns them in an `SseStreamResult`, and the proxy emits the assembled array as one
  delta chunk followed by a stop chunk with `finish_reason:"tool_calls"` — on both the
  live and cached-replay paths.  OpenAI-compatible only (Anthropic remains a follow-up).
  4 new tests.  634 tests.  (ADR-178)

### Added — Tool/function calling is forwarded to the backend (ADR-177)

- IMP-10 detected `tools`/`tool_choice` and escalated to the stronger model, but the
  tool definitions were never forwarded to any backend — so the model could not make a
  tool call, and a tool-call response (`content: null`) would have errored.  Now the
  raw `tools`/`tool_choice` are carried on the request and forwarded on the
  OpenAI-compatible path (cloud OpenAI provider, local OpenAI-compatible backend, and
  Ollama's `tools`).  `tool_calls` responses are parsed and re-emitted with
  `finish_reason:"tool_calls"`, and the cache key now distinguishes different tools.
  Anthropic tool-use and streaming tool-call deltas remain follow-ups (they fall back to
  the prior behaviour).  7 new tests.  630 tests.  (ADR-177)

### Fixed — Explicit `tool_choice: null` no longer forces cloud escalation (ADR-176)

- A request carrying `"tool_choice": null` (emitted by serializers that include every
  field, e.g. Pydantic without `exclude_none`) was treated as an active tool choice —
  `JsonValue::Null.as_str()` is `None` and `None != Some("none")` is true — so `has_tools`
  was set and the request escalated to the cloud.  This silently forced *every* request
  from such a client off-device, defeating local-first routing.  Fixed: a `null`
  `tool_choice` is treated like an absent field.  Named-function objects and
  `auto`/`required` still escalate; `none` and `null` stay local.  2 new tests.  622 tests.
  (ADR-176)

### Fixed — Anthropic streaming also logs actual token counts (ADR-175)

- ADR-173 fixed streaming token accounting for the OpenAI provider, but
  `parse_anthropic_stream_line` never surfaced usage, so Anthropic cloud streaming
  still fell back to `estimate_tokens`.  Anthropic splits usage across two SSE events:
  `message_start` (input tokens) and the final `message_delta` (cumulative output
  tokens).  Fixed: the parser emits a partial `Usage` from each, and `emit_sse_lines`
  merges them (non-zero field wins).  2 new tests.  620 tests.  (ADR-175)

### Fixed — `http_post_streaming` reports real HTTP errors from local backend (ADR-174)

- When the local backend returned a non-2xx HTTP status (e.g. `401 Unauthorized`,
  `500 Internal Server Error`), `http_post_streaming` silently consumed the error body
  as SSE content, producing `Protocol("empty stream")` as the only visible error.
  Fixed: the function now parses the HTTP status line and returns `http_status_error`
  for non-2xx responses before invoking any `on_line` callback — matching
  `read_sse_body`'s (cloud streaming) behaviour.  5xx errors remain `Transport`
  (retryable, IMP-9); 4xx are `Protocol` (not retryable).  618 tests.  (ADR-174)

### Fixed — Streaming backends now log actual token counts, not estimates (ADR-173)

- Both `HttpLocalBackend::stream_complete` and `HttpsCloudBackend::stream_complete`
  used `estimate_tokens` for `prompt_tokens` / `completion_tokens`, so streaming
  requests logged wrong counts to the cost log, corrupted the daily-budget gauge, and
  fed calibrate with biased data.  Fixed: outgoing streaming requests now include
  `stream_options.include_usage:true`; a new `Usage(u64,u64)` variant in
  `OpenAiStreamEvent` captures the final usage chunk; `read_sse_body` returns
  `(String, Option<(u64,u64)>)`.  Backends that don't send a usage chunk fall back
  to `estimate_tokens`.  2 new tests.  618 tests.  (ADR-173)

### Fixed — `calibrate` warns when fewer than 30 samples are available (ADR-172)

- `pasture calibrate` and `pasture calibrate --logprob` recommended a threshold from any
  sample ≥ 1 with no caveat.  A quantile from n=5 has a 95% CI spanning most of the
  sample range — the resulting `PASTURE_THRESHOLD` is statistically unreliable.  Fixed:
  emit an advisory note when `n < 30` (EN + JA), before the recommended threshold, so
  users know to collect more requests first.  The threshold is still printed.  Zero new
  deps; no change to calibration math.  617 tests.  (ADR-172)

### Fixed — `GET /v1/models` entries now include the required `created` field (ADR-171)

- `build_models_response` and `build_model_response` emitted model objects without the
  `created` (integer) field required by the OpenAI Model schema.  Strict clients such as
  the OpenAI Python SDK (non-optional field) and Cursor reject incomplete model objects.
  Fixed: `unix_now()` injected as `created` in every model entry — the standard approach
  for proxies without per-model creation timestamps.  1 new test.  617 tests.  (ADR-171)

### Fixed — Streaming chunks now share a single `created` timestamp (ADR-170)

- `build_openai_chunk` and `build_openai_usage_chunk` each called `unix_now()` internally, so every SSE
  chunk in the same completion carried a different `created` timestamp.  The OpenAI streaming API contract
  requires `id`, `model`, `system_fingerprint`, and `created` to be identical across all chunks.  Fixed:
  both builders accept a `created: u64` parameter; `stream_chat_to_socket` and `write_cached_stream`
  capture `unix_now()` once per stream and pass it to every builder call.  The buffered-response path
  (`build_openai_response`) was already correct.  1 new regression test.  616 tests.  (ADR-170)

### Fixed — Spike-only guard (budget off) no longer corrupts the usage gauge (ADR-169)

- After ADR-163 added token pre-reservation, `apply_budget_guard` returned the estimated token count as
  `reserved` on its success path even when the daily budget was disabled. But the actual `fetch_add`
  reservation in `check_budget_and_spike` runs only when `budget_daily_tokens > 0`, so with spike detection
  alone no tokens were reserved — yet `log_cost` reconciled against the phantom reservation, corrupting the
  `today_cloud_tokens` gauge on `/metrics` and `/v1/stats` (it clamped toward 0, under-reporting real cloud
  usage).  Fixed: the guard reports `reserved=0` unless the daily budget is active, so `log_cost` adds the
  actual tokens post-hoc and the gauge tracks real usage.  1 new test (proven to fail pre-fix).  615 tests.
  (ADR-169)

### Fixed — Streaming PII-restore buffer is bounded (`StreamRestorer`) (ADR-168)

- The streaming pseudonymization restorer (`PASTURE_PSEUDONYMIZE=1`) held back everything after the
  last unterminated `<` until the stream ended, to reassemble tokens (`<EMAIL_1>`) split across SSE
  deltas. But a dangling `<` followed by a long run with no `>` — `a < b` in code, or an adversarial
  response — made the buffer grow without bound: nothing was emitted until `finish()`, freezing the
  stream and amplifying memory.  Fixed: `StreamRestorer` now precomputes the longest mapping token and
  flushes the buffer once the dangling fragment exceeds that length (a real token always closes its
  `>` within it, so this is correctness-preserving; the split-token case still restores). With an empty
  mapping the restorer is pure pass-through and never buffers.  3 new tests.  614 tests. (ADR-168)

### Fixed — Monitoring endpoints exempt from rate limiter (`/metrics`, `/v1/stats`) (ADR-167)

- `check_gate` exempted only `/health` from the global request-rate limiter. `/metrics` (outside
  the `/v1/*` namespace the limiter documents itself as covering) and `/v1/stats` (read-only, locally
  computed telemetry) consumed rate-limit tokens exactly like `/v1/chat/completions`. A standard
  Prometheus scraper at 15-second intervals (4 req/min) silently ate 40 % of a `PASTURE_RATE_LIMIT=10`
  inference budget without doing any inference work.  Fixed: `check_gate` now exempts `/metrics` and
  `/v1/stats` from rate-limit consumption after auth but before the token-bucket check.  Bearer-token
  auth (`PASTURE_AUTH_TOKEN`) is still enforced for both endpoints — public deployments still guard
  telemetry.  `/v1/models` remains rate-limited (within the documented `/v1/*` scope).  2 new tests
  (exhausted-bucket still allows monitoring; auth still required when configured).  611 tests.
  (ADR-167)

### Added — Optional cloud pricing makes `cloud_cost_usd` a real number (ADR-166)

- `log_cost` hardcoded `cost_usd = 0.0` and no pricing existed anywhere, so the `cloud_cost_usd` field
  in `/v1/stats`, the `pasture_cloud_cost_usd_total` Prometheus counter, and the `pasture stats` "cloud
  spend" line were *structurally always $0.0000* — a cost metric that can only read zero, which actively
  misleads.  Added optional cloud pricing via `PASTURE_CLOUD_PRICE_PER_1M="<input>,<output>"` (USD per 1M
  prompt / completion tokens, e.g. `2.50,10.00`; also a `cloud_price_per_1m` config-file key).  Cloud
  completions now log a real `cost_usd = prompt/1e6 × input + completion/1e6 × output`; local and cache
  stay free.  Malformed, negative, or non-finite prices are rejected/clamped to 0, so the default
  (unset) keeps cost at 0 honestly.  Consistent with the BYOK, no-stale-hardcoded-pricing philosophy.
  7 tests.  (ADR-166)

### Added — Daily token budget is observable on `/metrics` and `/v1/stats` (ADR-165)

- The daily cloud-token budget (IMP-26) was enforced but invisible: the metrics endpoints reported
  all-time cost-log totals, never today's consumption against the cap.  An operator who set
  `PASTURE_BUDGET_DAILY_TOKENS` had no gauge to anticipate the budget running out — requests would just
  start silently routing local (or 429ing).  Added `pasture_budget_daily_tokens_used` and
  `pasture_budget_daily_tokens_limit` gauges to the Prometheus `/metrics` output and the matching
  `budget_daily_tokens_used` / `budget_daily_tokens_limit` fields to the `/v1/stats` JSON.  The snapshot
  rolls the UTC day first, so a scrape on a new day reads 0 rather than yesterday's stale total, and it
  reflects the same enforcement counter (including in-flight reservations) that gates the next request.
  Token counts only — no PII; both endpoints stay behind the existing auth gate.  4 tests. (ADR-165)

### Fixed — Budget token release saturates at 0 across a UTC day rollover (ADR-164)

- ADR-163 introduced five `fetch_sub` calls on `today_cloud_tokens` to roll back or reconcile a budget
  pre-reservation.  When a request straddles UTC midnight — the cloud RTT outlasts the day, and
  `roll_budget_day_if_needed` resets the counter to 0 — the subtraction `fetch_sub(reserved − actual)`
  underflows, wrapping `0` to ~`u64::MAX`.  The wrapped counter dwarfs any budget, so every cloud
  request for the rest of the new day is blocked or redirected to local: one midnight-straddling
  request silently disables the cloud route until the next midnight.  The bare rollback paths (cascade
  cloud failure, streaming backend error) have the same exposure via a concurrent day roll.  Fixed with
  a single `release_cloud_tokens` helper (a `fetch_update` CAS loop using `saturating_sub`) replacing
  all five sites, clamping the counter at 0.  Two new tests reproduce the post-rollover state (counter
  0, reservation > actual) on the reconcile and local-fallback paths; both fail against `wrapping_sub`.
  (ADR-164)

### Fixed — Budget daily-cap TOCTOU: atomic pre-reservation in `check_budget_and_spike` (ADR-163)

- `check_budget_and_spike` read `today_cloud_tokens` with `load(Relaxed)`, compared against the daily
  budget, and returned.  `log_cost` performed the actual `fetch_add` only after the cloud backend
  responded — an entire RTT later.  Two concurrent cloud requests could both read "under budget" and
  both proceed, overshooting the cap by up to one request's worth of tokens.  The "block" action was
  supposed to be a hard stop, but two simultaneous requests near the ceiling could both pass it.
  Fixed by replacing the non-atomic `load`→`compare` with an atomic pre-reservation:
  `fetch_add(estimated_tokens)` then check the returned `prev` value; if `prev ≥ budget`,
  `fetch_sub(estimated_tokens)` (rollback) and reject.  `log_cost` now *reconciles* the pre-
  reservation with the actual token count instead of doing a fresh add; it also rolls back the
  reservation when a cloud request falls back to local.  The streaming error path rolls back the
  reservation before writing the OTel error span.  The cascade's inner `apply_budget_guard` also
  rolls back on cloud failure.  New test `test_budget_pre_reservation_blocks_at_ceiling` seeds the
  counter at the ceiling, fires a forced-cloud request, and asserts the counter does not grow
  (proving the rollback). (ADR-163)

### Fixed — Cost log and trace log appends are now atomic under concurrency (ADR-162)

- `CostRecord::append_to` and `Span::append_to` used `writeln!`, which writes the record and the
  trailing newline as two separate syscalls.  Under the thread-per-connection server, concurrent
  appends could interleave and concatenate two records on one line; `read_log`'s `filter_map` then
  silently dropped both, losing them from cost/budget accounting and the trace log.  Both paths now
  build the line with its newline and do a single `write_all` (atomic under `O_APPEND`), matching the
  access log.  New 8-thread concurrency test (fails against the old `writeln!`). (ADR-162)

### Fixed — Cascade escalations now respect the daily budget / spike guard (ADR-161)

- The cascade runs only when a request is routed Local, where `apply_budget_guard` is a no-op, so a
  low-confidence escalation to the cloud bypassed the daily token cap and spike redirect entirely — in
  `"block"` mode the budget never blocked a cascade.  `complete_cascade` now applies the guard before
  escalating; when the cloud is declined (over budget or `"block"`) it keeps its local answer, the same
  graceful degradation it does on a cloud failure (never a 429).  2 new tests. (ADR-161)

### Fixed — `PASTURE_CACHE_TTL` now bounds the semantic cache too (ADR-160)

- `with_cache_ttl` applied the TTL only to the exact-match cache; `SemanticCache` had no TTL, so with
  `PASTURE_CACHE_TTL=3600` a semantic hit could return an answer arbitrarily old (until FIFO eviction),
  silently violating the operator's staleness bound for time-sensitive prompts.  `SemanticCache` now
  has the same `max_age` + per-entry timestamp as `ResponseCache`; `find_similar` drops expired
  entries; the TTL applies to both caches.  2 new tests. (ADR-160)

### Fixed — Semantic cache is now keyed by sampling parameters (ADR-159)

- After ADR-158 the semantic cache keyed on model but still ignored sampling parameters, so it could
  serve a cached `temperature:0` (deterministic) answer to a `temperature:1.8` request — the exact
  cross-serve the exact-match cache deliberately prevents.  Each entry now stores a `sampling_key`
  (over temperature, top_p, max_tokens, seed, penalties, stop, response_format); `find_similar` skips
  entries whose sampling signature differs.  `hash_sampling` is shared by `request_key` and
  `sampling_key` so both caches agree on which knobs matter.  2 new tests. (ADR-159)

### Fixed — Semantic cache is now keyed by requested model (ADR-158)

- `SemanticCache` stored `(embedding, response)` with no model identity.  Two requests for the same
  prompt but different model names produce the same embedding, so the second could get a cache hit
  returning the first model's response — wrong model, wrong answer.  Each entry now carries the
  requested model; `find_similar` skips entries for other models.  1 new test. (ADR-158)

### Security — Internal backend addresses stripped from client-facing error messages (ADR-157)

- On a backend transport failure the chain `BackendError::Transport(format!("connect {addr}: {e}"))` →
  `ProxyError::Backend(…)` → `build_error_response(…)` sent the internal host:port (e.g.,
  `127.0.0.1:11434`) in the JSON error body to any client.  On network-exposed deployments
  (`PASTURE_LISTEN_ADDR=0.0.0.0`) this leaks internal topology.  The address is now logged to
  `stderr` for the operator and a sanitized `"local/cloud backend unreachable (…)"` is returned to
  clients; the OS error kind (connection refused, timed out) is preserved. (ADR-157)

### Fixed — Pseudonymizer now masks IPv6 addresses (IMP-19, ADR-156)

- IPv6 became sensitive in ADR-148 but the pseudonymizer still masked only IPv4, so with
  `PASTURE_ALLOW_SENSITIVE_CLOUD=1` + `PASTURE_PSEUDONYMIZE=1` an IPv6 address reached the cloud raw.
  `process_token` now masks IPv6 (as the `IP` category) alongside IPv4, restoring it on the response.
  1 new test. (ADR-156)

### Fixed — Daily budget resets only when the UTC day advances forward (IMP-26, ADR-155)

- `roll_budget_day_if_needed` reset the daily token counter on any day change (`stored != today`), so
  a backward wall-clock step across midnight (NTP correction, VM snapshot restore, manual change)
  wrongly zeroed the counter and granted a fresh cloud allowance. It now resets only when the day
  strictly advances (`today > stored`), the safe direction for cap enforcement. 1 new test. (ADR-155)

### Fixed — Difficulty-signal centroid lock no longer serializes concurrent requests (IMP-14, ADR-154)

- `similar_to_hard` held the `hard_centroids` mutex across the blocking `/embeddings` init call and
  across the per-request cosine computation, so concurrent difficulty-signal requests were serialized
  (and a slow/cold local backend stalled all of them during init). Centroids are now stored behind an
  `Arc`: the lock is held only to clone the `Arc` (and to store once at init); the embedding call and
  the cosine run off-lock. 1 new concurrency test. (ADR-154)

### Fixed — Streamed completions are accounted even when the client disconnects mid-stream (ADR-153)

- The streaming path returned early on a failed client write, *before* cost logging, OTel span
  emission, and cache storage — so a client that disconnected mid-stream consumed cloud tokens that
  were never cost-logged, never traced, and never counted against the daily budget. The accounting
  is now done (via a no-I/O `finalize_streamed`) before the client-facing frames, so it runs whether
  or not the client is still connected; only the closing SSE frames are skipped. 2 new tests. (ADR-153)

### Fixed — Exposed-without-auth warning now uses authoritative loopback detection (ADR-152)

- The startup security nudge decided "is this a localhost bind?" with string-prefix matching, which
  both false-warned on expanded IPv6 loopback (`[0:0:0:0:0:0:0:1]`) and suppressed the warning for a
  global address starting with a loopback-looking prefix (`::1:2:3:4`). It now parses the address
  with `std::net` and tests `ip().is_loopback()`; unspecified binds (`0.0.0.0`, `::`) are correctly
  treated as exposed. Std-only. 1 new test. (ADR-152)

### Changed — `/metrics` and `/v1/stats` compute the cost summary incrementally (IMP-32, ADR-151)

- Both endpoints re-read and re-parsed the entire (unbounded-growing) cost log on every request, so
  a Prometheus scrape every few seconds became an O(log-size) operation. They now fold only the lines
  appended since the previous call into a cached running summary; output is byte-identical to the full
  re-read, and a truncated/rotated log resets the cache. The `pasture stats` CLI still reads in full.
  2 new tests. (ADR-151)

### Added — Streaming requests now use the semantic cache and difficulty signal (IMP-12/IMP-14, ADR-150)

- The semantic cache (IMP-12) and the embedding difficulty-escalation signal (IMP-14) previously
  applied only to buffered requests; a `stream:true` request computed no query embedding, so it
  re-ran the backend on a semantic match and was never pre-escalated near a known-hard prompt. Both
  paths now share an `embedding_step()` helper: streaming serves semantic hits as SSE, escalates the
  route before the budget guard, and stores restored content into both caches on a miss. All
  embedding features stay opt-in; sensitive content never reaches them (I5). 4 new tests. (ADR-150)

### Fixed — Streaming requests now get the system prompt and context (ADR-149)

- The streaming path skipped the configured `PASTURE_SYSTEM_PROMPT` and the date/OS context message
  that the buffered path applies, so a `stream:true` request behaved differently and could even route
  differently (framing feeds routing). Both paths now share a `frame_request()` helper, applied in
  streaming after the injection guard and before routing. 2 new tests. (ADR-149)

### Fixed — IPv6 addresses now detected as sensitive (IMP-3, ADR-148)

- The privacy classifier flagged IPv4 addresses but not IPv6, so a prompt containing an address like
  `2001:db8::1` was classified non-sensitive and could be routed to the cloud. `looks_like_ipv6()`
  now parses punctuation-trimmed tokens with std's `Ipv6Addr` behind a two-colon pre-check (so clock
  times and code tokens are not false positives); IPv6 shares the `"ip"` category and force-local
  treatment with IPv4. Std-only, no new deps. 2 new tests. (ADR-148)

### Added — Streaming requests now use the exact-match cache (IMP-31b, ADR-147)

- The exact-match cache previously served only buffered requests; a `stream:true` request always
  called the backend and never populated the cache. Streaming now checks the cache before the budget
  guard (a hit costs nothing, so it is served even when over budget) and replays the cached content
  as SSE with `x_pasture_route:"cache"`; a streamed miss populates the cache for later requests.
  Sensitive prompts are still never cached (I2). The semantic cache stays buffered-only. SPEC §6
  updated. 4 new tests. (ADR-147)

### Fixed — Empty local answer always escalates in cascade (IMP-1, ADR-146)

- `should_escalate` let the logprob signal replace the text heuristic entirely, so an empty answer
  carrying a confident mean-logprob (e.g. `0.0`, above a `-1.0` threshold) stayed local and returned
  a blank response to the user. An empty/near-empty answer now escalates unconditionally, before the
  logprob is consulted; the logprob still governs non-empty answers. 1 new test. (ADR-146)

### Added — Array-form message `content` (OpenAI multimodal shape, IMP-31, ADR-145)

- `content` may now be an OpenAI array-of-parts (`[{"type":"text","text":...}, ...]`), not only a
  string. Text parts are concatenated and routed as text; the flattened text still flows through the
  privacy classifier (PII guard unchanged). A non-text part (image/audio/file) is rejected with a
  clear 400 — Pasture is a text router and must not answer a vision request as if the image were
  absent. The official OpenAI SDK's vision helper emits array content even for plain text, so this
  fixes a real compatibility gap. SPEC §3.1 updated. 4 new tests. (ADR-145)

### Fixed — OTel spans now emitted for cache hits (IMP-23, ADR-144)

- The trace schema documents `pasture.route="cache"`, but cache hits returned before the span was
  created, so `PASTURE_OTEL_LOG` showed zero cache traffic. The span is now started before the
  cache lookups and a new `emit_cache_hit_span()` helper writes it on each early return, with
  `gen_ai.system` set to the route label (no upstream provider for a cache hit). 1 new test. (ADR-144)

### Fixed — OTel spans now emitted on backend failures (IMP-23, ADR-143)

- `PASTURE_OTEL_LOG` wrote a span only on success: the buffered path propagated backend errors with
  `?` and the streaming path's error arm wrote an SSE frame, both dropping the in-flight span. A
  sustained cloud outage therefore left the trace log blank — the opposite of what tracing is for.
  `Span` gains `status` (`"ok"`/`"error"`) and `error_message`; `to_jsonl()` emits the real status
  plus a `pasture.error` attribute on failures. Both paths now fill and append an error span before
  returning. 4 new tests. (ADR-143)

### Fixed — Authenticate before rate-limiting (IMP-15, ADR-142)

- `check_gate` consumed a global rate-limit token *before* authenticating, so an unauthenticated
  request spent budget before its 401. With both `PASTURE_AUTH_TOKEN` and `PASTURE_RATE_LIMIT`
  set, an attacker without the token could flood the proxy, drain the shared bucket, and 429 the
  legitimate client. The gate now authenticates first and meters only authenticated requests;
  `/health` stays exempt from both. 1 new test. (ADR-142)

### Fixed — Daily token budget resets at UTC midnight (IMP-26, ADR-141)

- The daily cloud-token budget (`PASTURE_BUDGET_DAILY_TOKENS`) never reset on a UTC day rollover:
  for a long-running `serve` process the counter accumulated across days, so after the first
  midnight the "daily" cap silently became cumulative-since-startup and, once exceeded, stayed
  exceeded until restart. The counter now tracks the UTC day it belongs to and resets lazily on
  the first budget access of a new day (no timer thread; std-only). 2 new tests. (ADR-141)

### Fixed — Correct `gen_ai.system` in OTel spans (IMP-23, ADR-140)

- The OTel `gen_ai.system` attribute emitted `"cloud"`/`"local"` (not valid provider values)
  and was computed from whether a cloud backend was *configured* — so a request that actually
  routed local was mislabeled `"cloud"`. It is now derived from the real route at emit time:
  cloud spans report the configured provider (`PASTURE_CLOUD_PROVIDER`, e.g. `openai`/`anthropic`)
  via the new `Proxy::with_cloud_system`; local spans report the local backend's name. 2 new
  tests. (ADR-140)

### Fixed — OTel trace log now covers streaming (IMP-23, ADR-139)

- The OpenTelemetry GenAI span (`PASTURE_OTEL_LOG`) was written only for buffered completions;
  `"stream":true` requests produced no span, so the trace log silently omitted the most common
  client mode. `stream_chat_to_socket` now emits a span per successful streamed completion
  (model, token usage, route, `finish_reason`), matching the buffered path. Response caching on
  the streaming path is intentionally out of scope (a separate design). 2 new tests. (ADR-139)

### Fixed — Budget/spike guard now covers streaming (IMP-26, ADR-138)

- The daily token budget (`PASTURE_BUDGET_DAILY_TOKENS`) and spike redirect (`PASTURE_SPIKE_FACTOR`)
  were enforced only on buffered completions; a `"stream":true` request bypassed them entirely.
  Because most chat clients stream by default, the spend cap was trivially evaded. The guard is
  now a shared `Proxy::apply_budget_guard` called from both the buffered and streaming paths —
  in `block` mode a streaming request over budget is rejected with 429 before the stream starts;
  in `local-only` mode it is redirected to the local model. 3 new streaming tests. (ADR-138)

### Fixed — Streaming pseudonymization parity (IMP-19, ADR-137)

- PII pseudonymization (`PASTURE_PSEUDONYMIZE=1`) was applied only on the buffered completion
  path; **streaming** (`"stream":true`) cloud requests sent raw PII to the provider, silently
  breaking the feature when combined with `PASTURE_ALLOW_SENSITIVE_CLOUD`. The streaming path
  now masks PII before the request leaves the machine and restores tokens in the streamed
  deltas. A new `pseudonymize::StreamRestorer` handles tokens split across SSE delta boundaries
  (it never flushes a partial `<…` token and never drops a held tail). Zero new dependencies;
  5 `StreamRestorer` unit tests + 2 proxy integration tests. (ADR-137)

### Added — Multi-provider cloud fallback chain (IMP-9 follow-up, ADR-136)

- `PASTURE_CLOUD_FALLBACK_PROVIDER` (`openai` or `anthropic`) configures a secondary cloud
  provider tried when the primary provider fails all retries. `PASTURE_CLOUD_FALLBACK_MODEL`
  sets the model on the fallback (defaults to the primary model). When both providers fail,
  the request falls back to local. When no fallback is configured, behaviour is identical to
  before (unchanged). API keys for each provider use their own env vars
  (`PASTURE_OPENAI_API_KEY`, `PASTURE_ANTHROPIC_API_KEY`). 4 new tests. (ADR-136)

### Changed — Output-length prediction sharpens budget/spike cost estimation (IMP-24)

- The budget/spike guard (`PASTURE_BUDGET_DAILY_TOKENS` / `PASTURE_SPIKE_FACTOR`) now estimates
  **input + predicted output** tokens instead of input alone. Cloud cost is driven mostly by
  output (priced 3–5× input), so the previous input-only estimate under-counted spend.
  `routing::estimate_output_tokens` predicts completion length from task type (code 3.0×,
  reason 4.0×, math 2.0×, translate 1.1×, summarize 0.3×, generic 1.5×), clamped to the client's
  `max_tokens` and a 4096 ceiling. Std-only heuristic — the proxy-model form (SSJF arXiv:2404.08509)
  remains deferred to avoid a dependency. 6 new tests. (ADR-135)

### Added — PII pseudonymization, OTel trace log, Anthropic cache hints, supply-chain CI (IMP-18, IMP-19, IMP-23, IMP-27)

- **IMP-19 Reversible PII pseudonymization** (`PASTURE_PSEUDONYMIZE=1`): cloud-bound messages
  have detected PII (email, IPv4, phone, API-key prefix) replaced with stable opaque tokens
  (`<EMAIL_1>`, `<IP_1>`, …) before leaving the machine; the cloud response has tokens swapped
  back. Identical values map to the same token across all messages in one request. The mapping
  lives in memory for the request lifetime only — never logged (I5). 9 new tests. (ADR-132)
- **IMP-23 OTel GenAI trace log** (`PASTURE_OTEL_LOG=<path>`): each completion appends one JSONL
  line in OpenTelemetry GenAI semantic convention format (OTel SemConv 1.28+). Fields: trace_id,
  span_id, start/end_time_unix_nano, gen_ai.system, gen_ai.request/response.model,
  gen_ai.usage.input/output_tokens, pasture.route. IDs generated from nanosecond time + atomic
  counter (no external RNG). No PII written (I5). Compatible with OTel Collector, Jaeger, Tempo.
  7 new tests. (ADR-133)
- **IMP-18 Anthropic prefix caching + system field separation** (`PASTURE_CACHE_CONTROL=1`):
  system messages are now sent in the Anthropic-required top-level `"system"` field (previously
  sent as conversation messages, which the API silently accepted but did not prefix-cache).
  With `PASTURE_CACHE_CONTROL=1`, the system block gains `"cache_control":{"type":"ephemeral"}`
  to enable Anthropic prompt-cache across requests. No-op for OpenAI targets. (ADR-131)
- **IMP-27 Supply-chain hardening**: `deny.toml` (cargo-deny) locks permitted licences and denies
  unlicensed/copyleft/yanked crates. `.github/workflows/ci.yml` gates every push/PR on:
  `cargo build --release`, `cargo test`, `clippy -D warnings`, `fmt --check`, and
  `cargo deny check`. (ADR-134)

### Added — Budget-aware routing: daily token cap + spike detection (IMP-26, IMP-21)

- Set `PASTURE_BUDGET_DAILY_TOKENS=<n>` to cap the cloud token spend per UTC day (prompt +
  completion tokens combined). When the running total reaches the cap, behaviour is controlled
  by `PASTURE_BUDGET_ACTION`: `local-only` (default) silently redirects the request to local;
  `warn` logs a warning but lets the request proceed; `block` rejects with HTTP 429. The running
  counter is seeded from the cost log at startup so a proxy restart does not reset today's spend.
- Spike detection: if `PASTURE_SPIKE_FACTOR=<n>` (default 50) is set and a single request
  estimates more than n × the running-request average, it is redirected to local regardless of the
  daily budget. Prevents accidental runaway token usage from outlier large prompts.
- `PASTURE_MAX_BODY_BYTES=<n>` (config key `max_body_bytes`): configures the per-request body size
  cap (default 16 MiB). Bodies larger than this yield HTTP 413. Previously hardcoded only.
- Privacy invariant preserved: sensitive content was already kept local before the budget check,
  so the guard never touches PII paths. 7 new tests (516 total). (ADR-129, ADR-130)

### Added — Prompt-injection guard: lexical detection with flag/block modes (IMP-20)

- Set `PASTURE_INJECTION_GUARD=flag` to detect and annotate potential prompt-injection
  attempts (role-switch and exfiltration patterns), or `=block` to reject them with a
  400 error. Default `off` (zero overhead). Detection uses ~30 lexical patterns (case-
  insensitive, std-only, no new dependencies); the label `role_switch` or `exfil_attempt`
  is logged to stderr in both modes and written to `x_pasture_injection_flag` in the
  response JSON for `flag` mode. Guard applies to buffered, streaming, and legacy
  completions. Privacy invariant: only the category label is recorded, never the prompt
  content. Grounded in PCFI (arXiv:2603.18433). 12 new tests; 509 total. (ADR-128)

### Added — Skill-profile routing: per-task-type route overrides (IMP-25)

- Set `PASTURE_SKILLS=code:local,summarize:cloud` (or `skills = …` in the config file)
  to pin detected task types to a specific backend. Recognised skills: `code` (fenced
  code block), `math` (≥4 math symbols), `reason` (step-by-step / chain-of-thought),
  `summarize`, `translate`. Skill routing is deterministic and config-driven; the
  override fires before generic hard signals but after privacy and local_only checks.
  Falls through to the token threshold when no skill matches or no rule is configured.
  `pasture config` prints the active skill table. Grounded in arXiv:2602.02386. 9 new
  tests added to routing.rs. (ADR-127)

### Changed — Fertility-based token estimation refinement (IMP-22)

- `estimate_tokens` now uses three buckets: ASCII whitespace contributes 0 tokens
  (previously counted as 0.25 tok/char alongside Latin text, over-estimating space-
  padded prompts), ASCII digits count at 0.5 tok/char (multi-digit numbers use 2-3
  chars per token in cl100k_base and LLaMA tokenisers), and Latin/punctuation remains
  0.25 tok/char. Dense scripts (CJK, Thai, Hangul, emoji) unchanged at 1.0 tok/char.
  The routing threshold comparison, cost-log token counts, and `pasture stats` are all
  more accurate for number-heavy and space-padded prompts. Grounded in arXiv:2509.05486
  "The Token Tax". 3 new tests. (ADR-126)

### Added — Embedding difficulty signal: known-hard prompts escalate pre-emptively (IMP-14)

- Set `PASTURE_HARD_PROMPTS=<file>` (one prompt per line) to name prompts your local model
  handles badly. A request embedding-similar to any of them (cosine ≥ `PASTURE_HARD_THRESHOLD`,
  default 0.85) escalates Local → Cloud before wasting a local attempt — catching hard prompts
  that read like plain prose and slip past the deterministic keyword heuristics. The embedding is
  computed once per request and shared with the semantic cache; centroids are embedded lazily via
  the local backend and a failure disables the signal (logged) rather than degrading requests.
  Never overrides privacy; requires a cloud backend; off by default. The `pasture config`
  printout now shows the semantic-cache and hard-prompts settings. This completes the
  COMPETITIVE.md backlog — IMP-8 through IMP-17 are all shipped. 10 new tests; 484 total.
  (ADR-125)

### Added — Error-grounded cascade calibration: `calibrate --error` (IMP-13, UCCI-style)

- `pasture calibrate --error --labels <f.jsonl> [--target E]` turns the cascade knob from an
  escalation-*rate* budget into a target-*accuracy* budget. It fits a monotone logprob → error-
  probability curve (Pool Adjacent Violators isotonic regression, std-only) on user-labelled
  answers (`{"logprob": -0.42, "correct": true}` per line; logprobs come from the cost log) and
  recommends the `PASTURE_CASCADE_LOGPROB` at which answers kept local have estimated error ≤ E
  (default 0.1). The printout shows each fitted error band with its sample count, so a thin label
  set is visibly thin; an unachievable budget is reported rather than papered over. Runtime
  behaviour is unchanged — the cascade still does one logprob comparison. EN/JA. Grounded in UCCI
  (arXiv:2605.18796) and the routing survey (2603.04445). 10 new tests; 474 total. (ADR-124)

### Added — Optional semantic cache via local `/v1/embeddings` (IMP-12)

- Set `PASTURE_SEMANTIC_CACHE=N` to hold up to N embedding–response pairs. On each non-sensitive
  request, the local backend's `/v1/embeddings` endpoint is queried once; cosine similarity is
  computed against stored entries; a response is returned on the first hit with similarity ≥
  `PASTURE_SEMANTIC_THRESHOLD` (default 0.92). On a real completion, the entry is stored for
  future near-duplicate queries. Silently skipped if the local backend is unavailable or the
  embeddings call fails. Off by default (0); sensitive content never cached (I5); embeddings stay
  on-machine (I3). `/v1/stats` and `/metrics` expose `semantic_cache_hits/misses/size/capacity`
  counters. No new dependencies. Grounded in GPTCache, arXiv:2603.03301/2402.01173/2411.05276,
  and the IMP-8 `/v1/embeddings` infra already in place. 10 new tests; 464 total. (ADR-123)

### Fixed — Truncated chunked cloud responses now error instead of silently losing data (IMP-dechunk-truncation-error)

- `dechunk()` accepted a chunked body whose declared chunk size exceeded the available bytes and
  returned the partial data with no signal — downstream code then saw corrupt JSON. It now returns
  `BackendError::Protocol` with declared-vs-actual byte counts; the cloud retry path handles it.
  1 test. (ADR-107)

### Fixed — `calibrate` threshold can no longer exceed the requested cloud-rate budget (IMP-calibrate-quantile-ceil)

- `calibrate_threshold` used `floor()` on the `(1-target)·n` quantile index; for non-integer
  products the achieved rate exceeded the target (n=3, target=0.5 → 0.667). `ceil()` guarantees
  achieved ≤ target. 1 test. (ADR-109)

### Changed — cli request construction reuses `chat_request` (IMP-refactor-chat-request-reuse)

- The non-cascade cloud/local paths reimplemented the `chat_request` helper's body inline. They
  now call it (the local path via `{ stream: true, ..chat_request(…) }`); `fast_request` uses
  `{ model, ..req.clone() }`. `prepend_system_prompt` stays explicit to avoid double-cloning
  `messages`. Behaviour identical. (ADR-122)

### Fixed — Intermittent `test_connection_close` failure eliminated (IMP-fix-connection-close-test-flake)

- The test closed the server socket with a pipelined second request unread, triggering a TCP RST
  that could discard the first response from the client's receive buffer (ECONNRESET, ~1-in-3
  flake). The client now consumes the first response and signals before the socket is dropped.
  Verified with 8 consecutive parallel full-suite runs. (ADR-121)

### Changed — `proxy.rs` test module moved to `src/proxy_tests.rs` (IMP-refactor-proxy-tests-split)

- `proxy.rs` was 4453 lines, 52% inline tests. The test module now lives in its own file via
  `#[path]` — same module tree and visibility, purely a physical move. (ADR-120)

### Changed — Code health: zero clippy warnings, duplication sweep, structural decomposition (ADR-108, 110–119)

- Removed the unreachable surrogate-pair error path in the JSON parser (`char::from_u32` cannot
  fail for a combined valid pair); the invariant is now documented at the call site. (ADR-108)
- `cargo clippy --all-targets` is clean again: merged the duplicate-arm `route_allowed_methods`
  branches (deny-level lint), fixed a `useless_conversion` regression, annotated the
  `Option`-returning `from_str` label parsers, applied mechanical test lints. (ADR-110)
- Duplication sweep — one source of truth for: backend HTTP wire format (`send_json_post`,
  ADR-111), doctor's model-list JSON traversal (`extract_string_field`, ADR-112) and model-list
  printing (`print_doctor_models`, ADR-114), system-message merge semantics
  (`prepend_system_prompt`, ADR-115), cloud TLS setup (`tls_send`, ADR-116, verified under
  `--features cloud`), and cost-log JSON number formatting (`format_number`, ADR-117 — the cost
  path also gains the negative-zero guard it was missing).
- Structural decomposition — cli `run()` is a thin dispatch (~248 → ~70 lines; `run_route`,
  `run_eval`, `run_stats`, `run_refer`, ADR-113); `handle_connection`'s endpoint dispatch is one
  line per route via `wr_result!`/`wr_err!` local macros (ADR-118); `run_completion`'s three
  completion strategies are named methods (`complete_cascade`, `complete_cloud_with_fallback`,
  `complete_direct` + `fast_request`, ADR-119). All behaviour-equivalent; 454 tests pass.

### Fixed — API keys embedded in JSON without surrounding whitespace are now detected (IMP-embedded-api-key-scan)

- `classify()` split on whitespace before checking for API-key prefixes. A credential in a JSON
  value with no spaces — `{"authorization":"sk-…"}` — was one token whose trimmed form still had
  `authorization":"` as a prefix, making `starts_with("sk-")` fail. A new `contains_embedded_api_key`
  scan finds any known prefix in the full text when preceded by a non-alphanumeric character and
  followed by ≥12 non-whitespace characters. This covers the most common real-world credential leak
  form (API key in an HTTP request body, curl example, or LLM tool-call output). 1 test. (ADR-101)

### Fixed — Thai and Devanagari prompts now correctly estimated as dense-script (IMP-dense-script-thai-devanagari)

- Thai and Devanagari (Hindi) characters tokenize at ~1 char/token in cl100k_base, but were counted
  at 0.25 tok/char (Latin default), under-estimating a 400-char Thai prompt as 100 tokens instead
  of ~400. Long Thai/Hindi prompts that should escalate to cloud were kept local. Both scripts are
  now in the dense-script block alongside CJK/kana/emoji. 1 test. (ADR-105)

### Fixed — `doctor` IPv6 backend URL connection and Host header (IMP-doctor-ipv6-host)

- `TcpStream::connect` received a bracketed IPv6 host (`[::1]`) from `parse_base_url`; the `(&str,
  u16)` form of `ToSocketAddrs` expects a bare address. Brackets are now stripped before connecting.
  The Host header also omitted the port, violating RFC 7230 §5.4 for non-standard ports. Both fixed.
  1 test. (ADR-106)

### Fixed — Three latent defensive hardening fixes (ADR-102–104)

- **cost.rs** `logprob_summary`: quantile closure now uses `n.saturating_sub(1)` instead of `n-1`
  to make the `n>=1` invariant self-documenting and panic-safe if the guard is ever moved. (ADR-102)
- **json.rs** `utf8_len`: invalid UTF-8 lead bytes (continuation 0x80–0xBF, overlong 0xC0–0xC1,
  above-Unicode 0xF5–0xFF) now return 1 instead of 4, keeping the parser aligned on bad input
  rather than jumping 4 bytes before the `from_utf8` error. (ADR-103)
- **ratelimit.rs** `step()`: token subtraction now uses `(tokens - 1.0).max(0.0)` to prevent an
  IEEE 754 residue (`-2.2e-16`) from inflating `Retry-After` by one second. (ADR-104)

### Fixed — Quoted/parenthesised JWTs are now detected; shared delimiter-trim helper (IMP-jwt-punctuation-strip)

- After the API-key fix, `looks_like_jwt` still trimmed only `"`, `,`, `;`, so a JWT wrapped in
  parens (`(eyJ…)`) or backticks failed `starts_with("eyJ")` and could leak to the cloud. Both
  detectors now share a `trim_token_delimiters` helper covering the full delimiter set, fixing the
  gap and removing the duplication. 1 test. (ADR-100)

### Fixed — Quoted API keys are now detected by the privacy classifier (IMP-api-key-punctuation-strip)

- `looks_like_api_key` checked `starts_with(prefix)` on the raw token, so a credential quoted in
  prose or code (`my key is "sk-…"`, `(sk-…)`) had a leading quote/paren and was **not** flagged
  as sensitive — making the prompt eligible for cloud routing (a privacy leak). The token is now
  trimmed of surrounding punctuation before the check, matching `looks_like_jwt`. `-`/`_` inside
  prefixes are preserved. 1 test. (ADR-099)

### Fixed — TTL-expired cache entries now removed from the FIFO order deque (IMP-cache-ttl-ghost-entries)

- `ResponseCache::get()` removed TTL-expired entries from the map but not from the FIFO `order`
  VecDeque. Ghost keys accumulated indefinitely — the map was bounded at `cap`, but the deque
  was not. At 1 req/s with TTL=3600s and cap=512, the deque would grow ~87k entries/day.
  `get()` now calls `order.retain()` to remove the ghost entry on TTL expiry. 1 test. (ADR-098)

### Fixed — Header line count limit prevents header-flood DoS in `read_request` (IMP-header-count-limit)

- The header parsing loop had no bound on line count. A crafted request with 1 MiB of
  minimal `\r\n` pairs generates ~500k iterations, each calling `to_ascii_lowercase()`,
  pinning a worker thread. The loop now returns `ReadOutcome::Closed` on the 1001st header
  field (1000 lines is far above any legitimate request). Defence-in-depth alongside the
  1 MiB total header size cap and the ADR-026 recursion depth cap. 1 test. (ADR-097)

### Fixed — Access log `method` and `path` fields are now JSON-escaped (IMP-access-log-json-escape)

- A malicious client sending quotes or backslashes in the HTTP request line (method or path)
  could inject arbitrary JSON into the access log JSONL file, corrupting all downstream log
  consumers. `request_id` was already escaped; `method` and `path` now also go through
  `escape_string()`. OWASP A09:2021 Security Logging and Monitoring Failures. 1 test. (ADR-096)

### Fixed — `http_post` write timeout prevents indefinite block on overloaded backend (IMP-backend-write-timeout)

- `http_post` and `http_post_streaming` set a read timeout but no write timeout. A very large
  prompt sent to a heavily loaded local backend could fill the kernel socket buffer and block
  `write_all` indefinitely, pinning a worker thread. Both now also set a write timeout equal to
  `PASTURE_LOCAL_TIMEOUT` (default 120 s), returning `Transport` error if the write stalls.
  (ADR-095)

### Fixed — `PASTURE_AUTH_TOKEN` env var now trimmed, fixing 401 on all valid auth requests (IMP-auth-token-trim)

- A bearer token set via `PASTURE_AUTH_TOKEN=$(cat ~/.token)` includes a trailing newline.
  `proxy.rs` `auth_ok()` trims the client's bearer token from the `Authorization` header
  but compared against the untrimmed stored token — `constant_time_eq` failed on the length
  mismatch and rejected all valid requests with 401. The env var value is now trimmed before
  storing. Same root cause as ADR-082 (API key trim). Config-file path was already safe.
  1 test. (ADR-094)

### Fixed — `PASTURE_ALLOW_SENSITIVE_CLOUD=false` no longer silently enables cloud routing of PII (IMP-bool-env-var-value-check)

- `PASTURE_ALLOW_SENSITIVE_CLOUD`, `PASTURE_NO_NUDGE`, `PASTURE_CASCADE`,
  `PASTURE_LOCAL_ONLY`, and `PASTURE_INJECT_CONTEXT` were checked with `is_ok()` — any
  value (including `"false"` or `"0"`) enabled the flag. Setting
  `PASTURE_ALLOW_SENSITIVE_CLOUD=false` would silently route PII and credentials to the
  cloud, violating the privacy-first invariant. All five now require `1|true|yes` (or empty
  bare presence) and reject `false|0|no`. Consistent with config-file parsing. 2 tests.
  (ADR-093)

### Fixed — Slack App-Level Token (`xapp-`) and HuggingFace (`hf_`) credentials now detected as PII (IMP-privacy-xapp-hf-prefixes)

- Slack App-Level Tokens (Socket Mode, `xapp-` prefix) and HuggingFace access tokens (`hf_`)
  are full API credentials that were previously undetected by the privacy classifier and could
  be sent to the cloud. Both are now added to `KEY_PREFIXES`, flagged as `api_key` sensitive,
  and kept local. The minimum-suffix-length check prevents false positives on short tokens.
  1 test. (ADR-092)

### Fixed — `format_cost`/`format_logprob` produce valid JSON for non-finite values (IMP-cost-format-finite-guard)

- `format!("{:.6}", f64::INFINITY)` = `"inf"` — an invalid JSON literal that silently corrupts the
  cost log. Any `CostRecord` with a non-finite `cost_usd` or `logprob` (from a cloud backend bug)
  would write an invalid JSONL line that `parse_log_line` then silently drops (record lost, no error).
  Both formatters now return `"0"` for any non-finite input before calling `format!`. Closes the
  serialisation gap complementing the ADR-078 aggregation guard. 3 tests. (ADR-091)

### Fixed — CLI `pasture chat` cascade cloud failure now logged (IMP-cascade-cli-error-log)

- Same fix as the proxy cascade path (ADR-087): the CLI chat command's cascade
  fallback silently discarded cloud errors. Now logs matching stderr message. (ADR-089)

### Fixed — Zero compiler warnings in library and test builds (IMP-proxy-warnings)

- Removed redundant `Write` import inside `append_access_log`, unnecessary `mut self`
  in `with_cache_ttl` builder, and six redundant `TcpListener`/`TcpStream` imports in
  test functions. `cargo build` and `cargo test` now produce zero warnings. (ADR-090)

### Fixed — Emoji correctly counted in token estimation (IMP-emoji-token-count)

- Emoji (😀🎉🔥 etc.) were counted as Latin characters (1 token per 4 chars), which
  under-estimated emoji-heavy prompts by 4–12×, keeping them local when the cloud
  model was more appropriate. Emoji blocks (U+1F000–U+1FAFF) are now counted as
  1 token per character, matching the same rule as CJK. 1 test (ADR-088).

### Fixed — Cascade cloud failures now logged to stderr (IMP-cascade-cloud-error-log)

- When the cascade path attempted cloud and it failed, the error was silently
  discarded. The proxy returned a local answer with no visibility into the failure.
  Now logs `pasture: cascade cloud failed (...); using local answer`, matching the
  existing non-cascade fallback log. (ADR-087)

### Fixed — `calibrate_logprob_threshold` drops NaN/inf before sorting (IMP-logprob-nan-filter)

- NaN values in the logprob input silently broke the sort (NaN compared equal to
  everything), producing wrong quantile thresholds. Non-finite values are now
  filtered out before sorting; all-NaN input returns the safe `(0.0, 0.0)` default.
  1 test (ADR-086).

### Fixed — `pasture doctor` no longer reports a non-Ollama server as reachable (IMP-doctor-status-check)

- A proxy or other HTTP service on the Ollama port returned non-200 responses;
  `tcp_get` returned the body regardless of status, so `probe_ollama` reported
  "Ollama running (no models)" when Ollama wasn't running. Now only 200 OK is
  treated as success. 1 test (ADR-085).

### Added — Configurable local backend timeout (`PASTURE_LOCAL_TIMEOUT`) (IMP-local-timeout)

- Set `PASTURE_LOCAL_TIMEOUT=<secs>` (or `local_timeout = <secs>` in the config file)
  to control how long the proxy waits for the local model before treating it as a
  Transport error. Default 120 s is unchanged; raise for 70B+ models on slow CPU
  (`local_timeout = 600`); lower to fail fast and trigger the cascade sooner.
  Both Ollama and OpenAI-compatible backends (LM Studio / vLLM / llama.cpp) respect
  the setting. 1 test (ADR-084).

### Fixed — API key trailing-whitespace strips before auth headers (IMP-api-key-trim)

- API keys set via `export KEY=$(cat ~/.api_key)` or shell substitution (which appends
  a trailing newline) were passed verbatim into `Authorization` / `x-api-key` headers,
  causing silent 401 failures. Keys are now trimmed before use. `PASTURE_DONATE_URL`
  was fixed by the same pattern. 3 tests (ADR-082).

### Fixed — Non-HTTP referral URLs rejected in `pasture refer` (IMP-referral-url-scheme)

- A bare affiliate code like `PASTURE_REF_openrouter=mycode123` was displayed as a
  clickable link instead of being treated as unconfigured. `referral_url` now requires
  an `http://` or `https://` prefix; non-URL values fall back to the provider's
  homepage. 2 tests (ADR-083).

### Changed — More reasoning/format routing markers (IMP-routing-markers)

- The router now escalates more genuinely-hard short prompts to the strong model:
  added `chain-of-thought` / `show your work` / `show your reasoning` /
  `walk me through` / `理由を説明` (reasoning) and `as xml` / `csv format` /
  `write a test` / `unit test` / `shell script` / `bash script` / `dockerfile` /
  `単体テスト` (format). Markers are specific to avoid false escalations; the curated
  18-case routing regression still scores 100%. 1 test (ADR-081).

### Added — `--json` output for `eval` and `stats` (IMP-cli-json-output)

- `pasture eval --json` prints `{total,correct,accuracy,cloud_rate,…,threshold}` and
  `pasture stats --json` prints the cost aggregates as compact JSON, for CI gating and
  dashboards (pipe to `jq`). `stats --json` emits valid zero-filled JSON even with no
  log. Human output is unchanged without the flag. 4 tests (ADR-080).

### Fixed — Parse UTF-16 surrogate-pair `\u` escapes / emoji (IMP-json-surrogate-pairs)

- The JSON parser now combines surrogate-pair escapes like `😀` into the
  correct character (😀). Previously these were rejected with a 400, which broke
  Python clients using `json.dumps` (whose default `ensure_ascii=True` emits
  surrogate escapes for emoji and other non-BMP characters). Literal UTF-8 emoji
  still works; lone surrogates are rejected. 2 tests (ADR-077).

### Fixed — Non-finite cost/logprob log lines no longer poison reports (IMP-cost-finite-guard)

- A corrupt cost-log line (e.g. `cost_usd` parsing to infinity, or a `NaN` logprob)
  no longer turns the entire `stats` / `calibrate` summary into NaN/inf — non-finite
  values are skipped during aggregation. 1 test (ADR-078).

### Added — `Server` response header (IMP-server-header)

- All responses now include `Server: pasture/<version>`, matching nginx / LiteLLM /
  Ollama, so the proxy is identifiable and its version detectable. 1 test (ADR-079).

### Added — Detect more credential formats (IMP-detect-cred-variants)

- The privacy classifier now flags GitHub `ghu_` / `ghs_` / `ghr_` tokens and AWS STS
  `ASIA…` temporary credentials (in addition to the existing `ghp_` / `AKIA`). These
  are kept local, never cached, and never sent to the cloud unless
  `PASTURE_ALLOW_SENSITIVE_CLOUD` is set. Labels only, never values. 1 test (ADR-075).

### Fixed — Config-file parity for `no_nudge` and `allow_sensitive_cloud` (IMP-config-file-parity)

- These two flags were settable via `PASTURE_NO_NUDGE` / `PASTURE_ALLOW_SENSITIVE_CLOUD`
  but were silently ignored when set in a config file. They now work from the config
  file too (`no_nudge = true`, `allow_sensitive_cloud = yes`). Defaults remain `false`.
  1 test (ADR-076).

### Changed — Error responses now include OpenAI `param`/`code` fields (IMP-error-envelope-fields)

- Every error body is now full OpenAI shape `{"error":{"message","type","param","code"}}`
  (`param`/`code` null when unknown) instead of `{message,type}` only, so strict SDK
  deserializers no longer fail. Rate-limit `429` responses carry
  `"code":"rate_limit_exceeded"` and auth `401` responses `"code":"invalid_api_key"`
  so SDK retry/branch logic works. Purely additive (no decision changed). Std-only;
  4 tests (ADR-074).

### Changed — `X-Request-ID` now present on every response (IMP-request-id-gen)

- When the client does not send an `X-Request-ID`, Pasture now mints a unique
  `req_…` id server-side (instead of omitting the header), so every response is
  traceable and every access-log line has a correlation id. A client-supplied id
  is still echoed unchanged. Matches OpenAI / LiteLLM. No PII (clock + counter
  only). Std-only; 2 tests (ADR-073).

### Added — `X-RateLimit-*` response headers (IMP-ratelimit-headers)

- When `PASTURE_RATE_LIMIT` is set, every response now carries
  `X-RateLimit-Limit-Requests`, `X-RateLimit-Remaining-Requests`, and
  `X-RateLimit-Reset-Requests` (`<n>s`), so clients can self-throttle proactively
  instead of only reacting to a `429`. Matches OpenAI / Azure / Anthropic / LiteLLM.
  Only the request family is emitted (Pasture meters requests, not tokens). No
  headers and zero overhead when rate limiting is disabled. Std-only; 4 tests
  (ADR-072).

### Added — `Retry-After` header on 429 responses (IMP-retry-after)

- When the rate limiter rejects a request, the `429` response now includes a
  `Retry-After: <seconds>` header (RFC 7231 §7.1.3) computed from the token
  bucket's time-to-next-token, so clients back off exactly long enough instead of
  retrying immediately. Matches OpenAI / LiteLLM / nginx behaviour. Std-only;
  4 tests (ADR-071).

### Added — Machine approval gate for the self-improvement ledger (IMP-approval-gate)

- `pasture improvements --review` lists only the entries the machine gate cannot
  auto-approve, so human review concentrates on the high-risk/unverified minority
  instead of every entry. An entry auto-approves iff it is a complete causal
  record, cites grounding, shows verification evidence in `effect`, and is not
  high-risk. Each entry now shows a risk tier (low/medium/high) — set explicitly
  via an optional `"risk"` field or inferred from the change text (security /
  privacy / auth surfaces and retired entries are high). `improvements` (without
  `--review`) also prints an `auto-approved / needs review` summary. On the bundled
  ledger ~71% auto-approve. Std-only; 9 tests (ADR-070).

### Added — RouterBench-format external eval loader (IMP-routerbench-loader)

- `pasture eval --external <file.jsonl>` validates routing on any labelled JSONL
  file. Each line is `{"prompt":"…","expected":"local"|"cloud"}`. Blank lines and
  `//` comments are skipped. Missing files and malformed lines return a clear
  error message. The external path uses the same routing pipeline as the built-in
  18-case set (privacy classifier + routing engine + sensitivity override).
  Std-only (`BufRead` + the in-tree JSON parser); 5 tests (ADR-069).

### Changed — Cache key normalises leading/trailing whitespace (IMP-cache-key-norm)

- Prompts that differ only in leading/trailing whitespace now share a cache key.
  Avoids spurious misses caused by copy-paste artifacts or SDK padding. Internal
  whitespace is unchanged (code formatting preserved). Zero-allocation; 1 test
  (ADR-068).

### Added — Cache TTL eviction (IMP-cache-ttl)

- Set `PASTURE_CACHE_TTL=3600` (or `cache_ttl_secs =` in config) to expire cached
  responses after N seconds. Expired entries are evicted lazily on the next `get`
  and removed from the map so `cache_size` stays accurate. Default 0 = no TTL
  (entries live until FIFO eviction). Std-only; 4 tests (ADR-067).

### Added — Structured per-request access log (IMP-access-log)

- Set `PASTURE_ACCESS_LOG=/path/to/access.jsonl` (or `access_log =` in config) to
  enable an append-only per-request access log. Each line is a JSON object with:
  `ts` (Unix epoch ms), `method`, `path` (no query string), `status`, `ms`
  (elapsed), and optionally `request_id`. No prompt content, no auth tokens, no PII.
  Off by default. `std-only`; 4 tests (ADR-066).

### Changed — `/health` now includes `version` field (IMP-health-version)

- `GET /health` response body is now `{"status":"ok","version":"<ver>"}` (compile-time
  constant via `env!("CARGO_PKG_VERSION")`). Scripts can detect version mismatches
  without a separate version endpoint.

### Added — 501 stubs for `/v1/audio` and `/v1/images` (IMP-health-version)

- `POST /v1/audio/*` and `POST /v1/images/*` now return **501 Not Implemented**
  instead of 404. Clients that probe these endpoints unconditionally get a clear
  `not_supported` error rather than a confusing 404. Wrong method still returns 405
  with `Allow: POST, OPTIONS`. Status reason table gains `405`, `415`, `501`
  entries; 4 tests (ADR-065).

### Added — `X-Response-Time` response header (IMP-response-time)

- Every HTTP response now includes `X-Response-Time: <N>ms`, measuring elapsed
  time from request parse completion to response write. Covers all paths: success,
  error (4xx/5xx), HEAD, streaming SSE (time-to-first-byte), and OPTIONS preflight.
  Std-only (`std::time::Instant`); additive header, no config required; 3 tests
  (ADR-064).

### Added — Prometheus `/metrics` endpoint (IMP-metrics-prom)

- `GET /metrics` now returns Prometheus text exposition format v0.0.4 with:
  - `pasture_requests_total{route="local|cloud|cache"}` — request counters per backend
  - `pasture_tokens_total{type="prompt|completion"}` — cumulative token counters
  - `pasture_cache_hits_total`, `pasture_cache_misses_total` — cache counters
  - `pasture_cache_size`, `pasture_cache_capacity` — live cache occupancy gauges
  - `pasture_cloud_cost_usd_total` — cumulative cloud cost gauge
- Compatible with Prometheus scrape and Grafana dashboards. Wrong method returns 405.
  Std-only; 3 tests (ADR-063).

### Added — `POST /v1/moderations` stub (IMP-moderations)

- Many OpenAI SDK versions call `/v1/moderations` before or after completions.
  Pasture now accepts these requests and returns a well-formed all-categories-safe
  response (all `false` / score `0`) in OpenAI moderation format. Pasture does
  not run real content moderation — the stub prevents SDK breakage. 2 tests.

### Changed — Reject `n > 1` with 400 Bad Request (IMP-n-validation)

- Requesting `n > 1` (multiple completions) now returns **400** with a clear
  message. Previously the extra completions were silently not returned, violating
  the API contract. `n = 1` and absent `n` are unchanged. 3 tests.

### Added — `cache_size` and `cache_capacity` in `/v1/stats` (IMP-stats-cache-size)

- `GET /v1/stats` now includes `cache_size` (current entry count) and
  `cache_capacity` (configured maximum, 0 when disabled). Enables sizing
  `PASTURE_CACHE` without log parsing. 2 tests.

### Added — Model-pinned routing via `req.model` (IMP-model-pinning)

- Requests that specify a recognised model name are now routed to the matching
  backend without consulting the routing engine's heuristics:
  - `"model":"local"` or `"model":"<PASTURE_LOCAL_MODEL>"` → forced Local.
  - `"model":"cloud"` or `"model":"<PASTURE_CLOUD_MODEL>"` → forced Cloud.
- Privacy override still applies: sensitive content stays local even when
  `model:"cloud"` is requested.
- Unrecognised model names use normal routing (unchanged).
- 5 tests.

### Added — Live cache hit/miss counters in `/v1/stats` (IMP-cache-counters)

- `GET /v1/stats` now returns two new fields: `cache_hits` and `cache_misses`.
  These are live in-memory counters from the `ResponseCache` (incremented on
  every `get()` call), independent of the JSONL cost log. The existing
  `cache_rate` (log-derived) is unchanged. Useful for real-time tuning of the
  cache capacity without log parsing. 3 tests.

### Changed — 415 Unsupported Media Type for non-JSON POST bodies (IMP-content-type)

- POST requests with an explicit `Content-Type` that is not `application/json`
  now receive **415 Unsupported Media Type** (RFC 7231 §6.5.13) with a clear
  error message instead of a confusing 400 JSON parse error. Absent
  `Content-Type` (bare curl) is still accepted; `charset=utf-8` suffixes pass.
  3 tests.

### Added — Configurable system prompt via `PASTURE_SYSTEM_PROMPT` (IMP-system-prompt)

- Set `PASTURE_SYSTEM_PROMPT="You are a coding assistant"` (or `system_prompt =
  ...` in the config file) to prepend a persistent system prompt to every
  request. The configured prompt frames the outermost context: if the client
  request already has a `system` message, the two are merged (configured prompt
  first). Applied before `PASTURE_INJECT_CONTEXT` so ordering is:
  configured system → date/OS context → user messages.
- Empty string disables the feature. 4 tests.

### Added — `POST /v1/completions` legacy text-completion shim (IMP-legacy-completions)

- Older LLM clients (pre-chat OpenAI SDK, LM Studio, some LangChain versions)
  default to the deprecated `POST /v1/completions` endpoint. Pasture now accepts
  those requests: the `prompt` field (string or string array) is mapped to a
  single user message and routed through the same privacy / routing / cache
  pipeline. The response uses `"object":"text_completion"` with `choices[].text`
  and a `cmpl-` ID prefix, matching the OpenAI convention.
- Streaming not supported via this shim; use `POST /v1/chat/completions` with
  `"stream":true` instead.
- Wrong-method requests (`GET /v1/completions`) return 405. 6 tests.

### Added — `tool_choice` as a hard escalation signal (IMP-tool-choice)

- `tool_choice` values other than `"none"` (`"auto"`, `"required"`, or a named
  function object) now escalate the request to the stronger (cloud) model even
  when no `tools` array is present. `"tool_choice":"none"` explicitly opts out
  and does not escalate. Closes the SPEC.md deferred item. 4 tests.

### Changed — 405 Method Not Allowed for known routes (IMP-http-methods)

- Wrong-method requests on known paths now return **405 Method Not Allowed** with
  an `Allow:` header (RFC 7231 §6.5.5), not 404. Examples: `GET /v1/chat/completions`
  → 405; `DELETE /v1/models` → 405. Unknown paths still return 404. 3 tests.

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

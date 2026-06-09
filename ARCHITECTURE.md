# Architecture

Pasture is a single Rust binary (std-library only) following a
performance-first, minimal-dependency philosophy (Carmack / Pike).

> The normative API/routing contract is in **[SPEC.md](SPEC.md)**.
> Competitive landscape, arXiv grounding, and the forward improvement backlog
> (IMP-8 →) live in **[COMPETITIVE.md](COMPETITIVE.md)**.

## Module map

| Module | Responsibility |
|--------|----------------|
| `hardware` | Detect RAM / CPU / GPU. Pure parsers separated for testing. |
| `routing`  | The core IP: deterministic, hardware-adaptive decisions. |
| `backend`  | `Backend` trait + Mock, Ollama (HTTP), Cloud (stub). |
| `proxy`    | OpenAI-compatible handler + minimal HTTP/1.1 server. |
| `json`     | Dependency-free JSON parse/serialize (small scope). |
| `config`   | Defaults + `key = value` file + `PASTURE_*` env overrides. |
| `cost`     | JSONL cost records (no PII). |
| `cli`      | Hand-rolled argument parsing and command dispatch. |

## Decision records

- **ADR-001 Local backend = orchestrate Ollama (not embed llama.cpp).**
  Keeps the binary tiny and inherits GPU acceleration for free.
- **ADR-002 Deterministic routing first.** No ML router; ordered rules with a
  hardware-adaptive token threshold. Fast, predictable, testable.
- **ADR-003 Rust, std-only.** Single binary, cross-platform, zero deps.
- **ADR-004 Ship as proxy + thin CLI (no GUI).** The desktop GUI space is
  saturated; the consumer routing-proxy layer is open.
- **ADR-005 Cloud backend over TLS (RESOLVED in v0.6.0).** The std library has
  no HTTPS, so the cloud backend is gated behind an opt-in `cloud` feature that
  pins an MSRV-1.75-compatible TLS stack (native-tls / system OpenSSL). The
  default build stays zero-dependency. Request/response shaping and HTTP
  framing are pure and tested; verified against api.anthropic.com.

## Routing thresholds

| Hardware | Token threshold | Effect |
|----------|-----------------|--------|
| GPU >= 8 GB VRAM | 2000 | keep most work local |
| GPU present or RAM >= 16 GB | 800 | balanced |
| CPU-only, low RAM | 300 | escalate to cloud sooner |
- **ADR-006 Monetization = surface-only (donation link + referral links).**
  Pasture never handles cards or secrets. Donations go through a hosted Stripe
  Checkout (serverless Worker); referral URLs are the operator's own affiliate
  links from config. No user PII is collected (I5). The proxy layer is the
  natural place to surface cloud-provider referrals (COMPETITIVE.md §6).
- **ADR-007 Privacy-classification routing (IMP-3).** A std-only detector
  classifies prompt sensitivity (categories only, no values — I5). Sensitive
  prompts are forced local and override even `--cloud`; if no local backend is
  available the request errors rather than leaking to the cloud. Opt-out via
  `PASTURE_ALLOW_SENSITIVE_CLOUD`. Grounded in PRISM (arXiv:2511.22788) and the
  "sensitive data stays local" pattern in peer tools (COMPETITIVE.md).
- **ADR-008 Richer difficulty features (IMP-4).** The hard-signal step now
  considers reasoning-depth markers, strict-format/code-gen requests, math
  density, and multi-question prompts (EN + JA), in addition to code fences and
  token length. Still deterministic and label-only. Grounded in the routing
  survey (arXiv:2506.06579). Disabled together with code routing via
  `with_code_to_cloud(false)`. Note: this favours quality on hard tasks at some
  extra cloud usage; local-only setups are unaffected (they stay local).
- **ADR-009 SSE streaming (IMP-7).** The proxy detects `"stream": true` and
  streams `chat.completion.chunk` frames. The local backend reads Ollama's
  NDJSON line-by-line over the std-only socket and re-emits deltas; privacy and
  routing decisions apply identically to streaming and buffered paths. Pure
  frame/chunk builders are unit-tested; the socket loop is integration-level.
- **ADR-010 Feature-gated dependencies.** Anything requiring a non-std
  dependency (currently only TLS for cloud) lives behind a Cargo feature so the
  default artifact remains zero-dependency, small, and MSRV 1.75. Dependencies
  are pinned exactly for reproducibility (governance: release-approval).
- **ADR-011 Cascade routing (IMP-1).** Opt-in. Instead of committing up front,
  Pasture answers locally then escalates to the cloud only when the local answer
  is low-confidence (v0: response heuristics; no logprobs). Faithful to
  FrugalGPT. Constraints: never for sensitive content (privacy overrides), not
  for streaming requests, and a no-op without a cloud backend. Cloud failure
  falls back to the local answer rather than erroring. Future (IMP-2): replace
  heuristics with calibrated confidence.
- **ADR-012 Evaluation harness (IMP-5).** Routing quality is measured, not
  asserted: a labelled set checks correctness (regression) and a threshold
  sweep quantifies the cost/locality trade-off. This makes the deterministic
  rules tunable on evidence (§6.5) and answers the "brittle rules" critique
  from the routing survey. Fully offline and zero-dependency.
- **ADR-013 Response cache (IMP-6).** Opt-in exact-match cache keyed by a std
  hash of (model + messages). Hits skip the backend entirely (cost 0, route
  "cache"). Bounded FIFO eviction keeps memory predictable; zero-dependency.
  Sensitive prompts are never cached (privacy, I5). Cache and cascade compose:
  a cache miss may still trigger a cascade, and the result is then cached.
- **ADR-014 CI as the release gate.** The §8 gate is enforced in GitHub Actions
  across the default (zero-dep) and `cloud` feature sets, pinned to MSRV 1.75 so
  dependency drift (e.g. edition2024-only transitive crates) fails fast. Release
  artifacts are built per-platform on tag; publishing and code signing stay
  human-gated (release-approval).
- **ADR-015 Observability via the cost log (IMP-2 groundwork).** Every request
  already appends a PII-free JSONL record (route, model, tokens, cost). `stats`
  reads it back to report cloud rate, cache hit rate, and spend. This turns the
  hand-set threshold into something tunable on observed data: high cloud rate
  -> raise the threshold; high cache hit rate -> the cache is paying off. No
  network, no extra dependency.
- **ADR-016 Beginner-first onboarding.** The biggest barrier for new users is
  the pre-flight setup (Ollama running? model pulled? port free?). `pasture
  doctor` diagnoses each and prints the exact fix; the no-arg welcome and
  GETTING_STARTED.md complete the on-ramp. Diagnostics reuse the std-only TCP
  stack (zero new dependencies); JSON parsing is pure and tested.
- **ADR-017 i18n (I10).** A tiny zero-dependency catalog keyed by
  `namespace.component.key`, Japanese-first with English fallback and `{name}`
  interpolation. Language is auto-detected from the environment. A unit test
  enforces key parity so a translation can never silently go missing.
- **ADR-018 One-command run + bootstrap.** `pasture up` collapses the run path:
  check Ollama, auto-pull the model via the user's local `ollama` CLI, then
  serve. Shelling out to `ollama` is invoking a tool the user already has — not
  a library/network dependency — so the zero-dependency promise holds.
  `install.sh`/`install.ps1` reduce first-time setup to build + `pasture up`. `up`
  also auto-starts the Ollama daemon when possible, and startup prints a
  localised base_url connect banner so users know exactly what to point their
  app at.
- **ADR-019 Connection & model guidance (research-grounded).** Beginners stall
  at "which model?" and "how do I connect my app?". `models` recommends current
  (June 2026) models by RAM tier; `connect <app>` prints exact setup for Open
  WebUI / Continue / Cursor / SDK. Content is verified against current docs and
  localised. Note: Ollama itself is OpenAI-compatible, so pasture's distinct
  value is the routing/privacy/cascade/cache layer and one unified base_url.
- **ADR-020 OpenAI-compatible local backend (LM Studio et al.).** Ollama is not
  the only local runner; LM Studio, llama.cpp server, vLLM and LocalAI all
  expose an OpenAI `/v1` API. `OpenAiCompatBackend` (plain HTTP, reusing the
  always-compiled OpenAI shaping from `cloud`) lets pasture sit in front of any
  of them, selected by `PASTURE_LOCAL_BACKEND`. The default stays Ollama. This
  keeps the zero-dependency promise (no TLS for localhost) while broadening
  compatibility; `doctor`/`up` adapt their checks per backend.
- **ADR-021 Streaming for OpenAI-compatible backends.** Local OpenAI servers
  speak SSE; pasture now streams through them token-by-token (reusing the
  std-only line reader and a pure SSE parser), matching the Ollama streaming
  path. Cloud (HTTPS) streaming remains buffered for now. Privacy/routing still
  decide before any byte is sent; cascade stays non-streaming by design.
- **ADR-022 Script-aware token estimation.** Routing length thresholds assumed
  ~4 chars/token (Latin). Japanese/Chinese/Korean are far denser (~1
  token/char), so the old estimate kept long non-Latin prompts local. Estimation
  now weights CJK/Hangul at one token per character. Keeps the engine
  zero-dependency and deterministic; corrects a systematic bias for the
  Japanese-first audience.
- **ADR-023 Data-driven threshold calibration (IMP-2).** The literature shows the
  strongest routers are *learned* from preference or quality-gap labels
  (RouteLLM, Hybrid LLM); RouterBench/UCCI standardise the cost/quality view and
  emphasise calibration. A single local user has no such labels, so pasture keeps
  its deterministic heuristic but calibrates its one length knob against the
  user's real, PII-free token-size log: `calibrate_threshold` picks the
  (1 - target) quantile of logged prompt sizes. Output is advisory
  (`PASTURE_THRESHOLD=...`); the engine still adds content/privacy escalation on
  top, so the figure is a lower bound. Stays zero-dependency and offline.
- **ADR-024 Privacy classification hardening.** A privacy-first router's worst
  failure is leaking PII to the cloud, so the classifier is reviewed for false
  negatives. Closed two gaps with near-zero false-positive cost: Japanese
  domestic phone numbers (the `+`-only rule missed them — critical for a JA-first
  audience) and JWT/bearer tokens (`eyJ` + three base64url segments). Labels
  only, never the matched value. The bias is deliberately toward over- rather
  than under-classifying: a false positive keeps a prompt local (cheap); a false
  negative leaks data (unacceptable).
- **ADR-025 Cloud SSE streaming.** Streaming now covers all routes. The risky,
  hard-to-verify part is the SSE assembly (header split, chunked framing, per-
  provider delta parsing), so it is factored into `read_sse_body<R: Read>` —
  pure, transport-free, unit-tested offline over cursors. The feature-gated TLS
  path is a thin wrapper that hands the TLS stream to that function, and was
  verified live against Anthropic (error path). Latency/routing/privacy still
  decide before any byte leaves; the cloud streaming code is compiled only under
  `--features cloud`, preserving the zero-dependency default.
- **ADR-026 Parser recursion bound.** The proxy parses untrusted client request
  bodies with the hand-rolled JSON parser, whose recursive descent had no depth
  limit — a malicious deeply-nested document could overflow the stack and abort
  the long-running server (remote DoS). A depth cap (128, far above any real
  chat payload) converts this into a graceful parse error. Stays zero-dependency.
- **ADR-027 Cascade confidence from mean log-probability.** The cascade's
  escalation signal was a text heuristic (refusal/uncertainty markers). arXiv
  2605.02241 shows that a small model's mean token log-probability is a strong,
  *training-free* confidence signal for local->cloud routing, matching or
  beating supervised routers (RouteLLM) in-distribution. When the local backend
  exposes logprobs (OpenAI-compatible/LM Studio), `should_escalate` uses the mean
  logprob against `PASTURE_CASCADE_LOGPROB`; otherwise it falls back to the
  heuristic. Keeps the engine zero-dependency, offline, and label-free; the
  threshold is env-tunable because the paper finds the optimal cutoff
  model-dependent. Privacy still wins: sensitive prompts never cascade.
- **ADR-028 Cascade-threshold calibration from logged confidence.** v0.23.0's
  mean-logprob escalation used a hand-set threshold; the literature (UCCI,
  "Is Escalation Worth It?") shows such thresholds must be calibrated per
  workload, but rigorous calibration needs correctness labels the single local
  user lacks. We instead record the local mean logprob (a PII-free number) to
  the cost log and let `calibrate --logprob` recommend the threshold at the
  target-escalation quantile of that distribution. Honest scope: this controls
  the escalation *rate* (budget), not correctness-calibrated optimality. Stays
  zero-dependency, offline, label-free.
- **ADR-029 Concurrent request handling.** The proxy served connections serially
  (one `for stream in incoming()` loop), capping throughput. It now uses a
  bounded std-only worker pool: N = available_parallelism (clamped 2..=32)
  threads share an `Arc<Proxy>` and pull connections from a bounded
  `sync_channel`. Bounded (not unbounded `spawn`) so a connection flood applies
  backpressure instead of exhausting threads/memory. The cache mutex already
  made shared state safe; backends are `Send + Sync`. Stays zero-dependency.
- **ADR-030 `/v1/models` endpoint (IMP-8).** Many OpenAI-compatible clients probe
  `GET /v1/models` on connect and fail or hide the server when it 404s. The proxy
  now answers with an OpenAI-shaped list of the configured local (and cloud) model
  ids. Purely additive — no routing behaviour changes — and std-only. Grounded in
  the peer/API-surface gap analysis (COMPETITIVE.md, RESEARCH.md cat.7).
- **ADR-031 Tool/function-calling as a hard signal (IMP-10).** A request carrying a
  non-empty `tools`/`functions` array means the client expects reliable tool use,
  which the stronger (cloud) model handles best. `parse_request` detects it and
  `decide_full` treats it as a hard signal alongside the content-based ones — gated
  by the same `code_to_cloud` rule, and still overridden by privacy (sensitive stays
  local). The fields are passed through unchanged. Deterministic and std-only.
- **ADR-032 Cloud retry + local fallback (IMP-9).** A single transient cloud error
  (timeout, connection reset, provider 5xx) previously failed the request or silently
  dropped to a weaker local answer only on the cascade path. The cloud backend now
  classifies 5xx as `Transport` (retryable) vs 4xx as `Protocol` (not); a pure
  `complete_with_retry` retries retryable failures with exponential backoff
  (`PASTURE_CLOUD_RETRY`, default 2), and on final failure falls back to local when a
  local backend exists rather than erroring. The retry decision is unit-tested with a
  flaky mock (no sleeps); std-only, and the cloud HTTPS path stays feature-gated.
- **ADR-033 Spec conformance hardening (SPEC.md §3.5/§7).** Writing the formal spec
  surfaced three conformance gaps, now closed: (1) error bodies use the OpenAI
  envelope `{"error":{"message,type}}` instead of a flat string, via a single
  `build_error_response` + `ProxyError::kind`; (2) chat responses and stream chunks
  carry the OpenAI `created` timestamp; (3) `read_request` returns a `ReadOutcome`
  that rejects an oversized body (`MAX_BODY_BYTES`, 16 MiB) with `413` instead of an
  unbounded read — closing a remote-DoS vector beyond the existing header/recursion
  caps (IMP-21, partial). All std-only; covered by socket round-trip tests.
- **ADR-084 Configurable local backend timeout (IMP-local-timeout).** The 120 s
  hardcoded read timeout for local inference calls (Ollama, LM Studio, vLLM) served
  all model sizes equally poorly: a 70B model on a slow CPU can take several minutes
  per token, while a 7B model on a fast GPU should fail within 10–30 s so the cascade
  can escalate. `Config` gains `local_timeout_secs` (default 120); the value is exposed
  as `PASTURE_LOCAL_TIMEOUT` env var and `local_timeout =` config key.
  `OllamaBackend` and `OpenAiCompatBackend` store the timeout as a `Duration` field
  set at construction time; `make_local_backend` forwards the config value.
  All `http_post` and `http_post_streaming` calls now use `self.timeout` — no hardcoded
  constant remains. Existing deployments see no behaviour change (default preserved).
  Std-only; 1 test.
- **ADR-083 Reject non-HTTP referral URLs (IMP-referral-url-scheme).** A misconfigured
  `PASTURE_REF_<KEY>` like `mycode123` (a bare affiliate code, not a URL) was silently
  treated as a configured referral URL and displayed verbatim in `pasture refer` output,
  producing a non-navigable link. `referral_url` now validates the scheme after trimming:
  `None` unless the value starts with `http://` or `https://` (case-folded).
  FTP and other schemes are also rejected. Matches the existing blank-guard contract and
  the `donate_url` validation pattern. Std-only; 2 tests.
- **ADR-088 Emoji counted as dense tokens in `estimate_tokens` (IMP-emoji-token-count).**
  Emoji (U+1F000–U+1FAFF) were counted as Latin characters (1 token per 4 chars)
  in `is_dense_script`, under-estimating emoji-heavy prompts by 4–12×. Common
  tokenizers (GPT-4 cl100k, LLaMA-3 BPE) assign 1–3 tokens per emoji. The
  Emoticons / Misc Symbols and Pictographs blocks are added to `is_dense_script`'s
  match, matching the same 1-token-per-char heuristic used for CJK. Conservative:
  over-estimating is safer than under-estimating for routing quality. The curated
  18-case eval still passes 100%. Additive range extension, std-only; 1 test.
- **ADR-087 Log cascade cloud failure (IMP-cascade-cloud-error-log).**
  The cascade path silently discarded cloud errors (`Err(_)`) when falling back to
  the local answer. The non-cascade cloud route already logged
  `"pasture: cloud failed; falling back to local"` at line 703. The cascade arm now
  logs the same pattern (`Err(e)` + `eprintln!`), making cloud outages visible
  during cascade usage. No change to request/response behavior.
- **ADR-086 Defensive NaN filter in `calibrate_logprob_threshold` (IMP-logprob-nan-filter).**
  `partial_cmp` returns `None` for NaN, and the prior `unwrap_or(Equal)` silently
  treated NaN as equal to every value, breaking the sort invariant and producing
  wrong quantile thresholds. Although the normal call path filters non-finite values
  via ADR-078, the function had no contract enforcement. Finite-only values are now
  filtered with `.is_finite()` before sorting; all-NaN input returns the safe
  `(0.0, 0.0)` default; `sort_by` uses `.expect("filtered to finite")` to document
  the invariant. Std-only; 1 test.
- **ADR-090 Eliminate all compiler warnings (IMP-proxy-warnings).** Three categories
  of pre-existing warnings silently accumulated in `proxy.rs`: (1) `use std::io::Write`
  inside `append_access_log` (already imported at module level); (2) `mut self` in
  `with_cache_ttl` (builder consumes self by value — `mut` not needed); (3) six
  redundant `TcpListener`/`TcpStream` imports in individual test functions (already
  in scope via `use super::*`). All removed. Zero-warning build from both
  `cargo build` and `cargo test`.
- **ADR-089 Log cascade cloud failure in CLI `chat` command (IMP-cascade-cli-error-log).**
  The CLI `pasture chat` cascade path had the same silent `Err(_)` discard that ADR-087
  fixed in the proxy. Added `eprintln!("pasture: cascade cloud failed (…); using local
  answer")` matching the proxy pattern, so cloud outages during cascade are visible
  regardless of the client path used.
- **ADR-085 `doctor::tcp_get` validates HTTP status (IMP-doctor-status-check).**
  A non-Ollama HTTP service on the Ollama port (nginx, a proxy, another app) returns
  a 200 or error body for unknown paths; `tcp_get` returned the body regardless of
  status, so `probe_ollama` set `reachable: true` with an empty model list — the
  doctor printed "Ollama is running (no models downloaded)" when Ollama was not
  running. The function now parses the first response line and returns `None` on any
  non-200 status. A mock-TCP-server test verifies the new behavior. Std-only; 1 test.
- **ADR-091 Guard `format_cost`/`format_logprob` against non-finite values (IMP-cost-format-finite-guard).**
  `format!("{:.6}", f64::INFINITY)` = `"inf"` and `format!("{:.4}", f64::NAN)` = `"NaN"` — both are
  invalid JSON numbers. A `CostRecord` carrying a non-finite `cost_usd` or `logprob` (propagated
  from a cloud backend bug) would write an invalid JSONL line, silently corrupting the cost log;
  `parse_log_line` would then silently drop that record (the in-tree parser correctly rejects `inf`/`NaN`
  literals). Both functions now return `"0"` on any non-finite input, before the `format!` call.
  Complements ADR-078 (which guards the read/aggregate path); closes the serialisation path.
  Grounded in RFC 8259 §6. Std-only; 3 tests.
- **ADR-082 Trim API key and donate URL env vars (IMP-api-key-trim).** `api_key_from_env`
  checked `!k.trim().is_empty()` but returned the raw untrimmed value, so a key set via
  `export KEY=$(cat ~/.api_key)` — a common pattern that appends a trailing newline —
  embedded whitespace in `Authorization`/`x-api-key` HTTP headers, causing silent 401
  auth failures. `PASTURE_DONATE_URL` had the same pattern. Both now `.trim()` the value
  before storing or returning it. Zero behaviour change for keys without surrounding
  whitespace. Std-only; 3 tests.
- **ADR-081 More reasoning/format hard-signal markers (IMP-routing-markers).**
  Genuinely hard but short prompts (`write a unit test for foo`, `walk me through
  the proof`, `give me a bash script`) were under-routed to the weak local model:
  they fell below the length threshold and matched no existing marker — the same
  under-routing IMP-4 set out to fix. Added strong, *specific* markers to
  `REASONING_MARKERS` (`chain-of-thought`, `show your work`, `show your reasoning`,
  `walk me through`, `理由を説明`) and `FORMAT_MARKERS` (`as xml`, `csv format`,
  `write a test`, `unit test`, `shell script`, `bash script`, `dockerfile`,
  `単体テスト`). The markers are specific enough to avoid broad false escalations,
  and the change is **verified eval-safe** — the curated 18-case regression still
  scores 100% because no Local case matches a new marker. Std-only; 1 test.
- **ADR-080 Machine-readable `--json` for `eval` / `stats` (IMP-cli-json-output).**
  `eval` and `stats` were human-text only, so CI pipelines and dashboards had to
  scrape prose to gate on routing accuracy or cloud spend. `EvalReport::to_json`
  and `CostSummary::to_json` build compact, parser-validated JSON; `pasture eval
  --json` emits `{total,correct,accuracy,cloud_rate,false/missed_escalations,
  threshold}` and `pasture stats --json` the cost aggregates (zeros — still valid
  JSON — when the log is empty). Human output is unchanged without the flag. The
  builders are pure and unit-tested by round-tripping through the in-tree JSON
  parser. Std-only; 4 tests.
- **ADR-079 `Server` response header (IMP-server-header).** nginx, LiteLLM and
  Ollama all identify themselves with a `Server` header; Pasture sent none, so
  proxies/clients/debuggers could not identify the software or detect its version.
  Every response now carries `Server: pasture/<version>` via the shared `extra`
  header block, using a compile-time `concat!/env!("CARGO_PKG_VERSION")` constant
  (zero runtime cost). Additive, std-only; 1 test.
- **ADR-078 Ignore non-finite values in cost/logprob aggregation (IMP-cost-finite-guard).**
  A corrupt or hand-edited cost-log line — e.g. `"cost_usd":1e400` parses to `inf`,
  or `"logprob":NaN` — propagated through `summarize`'s sum and `logprob_summary`'s
  mean/percentiles, turning the whole `stats` / `calibrate` report into NaN/inf.
  `summarize` now adds `cost_usd` only when `is_finite()`, and `logprob_summary`
  filters non-finite logprobs before aggregating. One bad record can no longer poison
  the totals. Std-only; 1 test.
- **ADR-077 Parse UTF-16 surrogate-pair `\u` escapes (IMP-json-surrogate-pairs).**
  Python's `json.dumps` defaults to `ensure_ascii=True`, encoding non-BMP characters
  (emoji, CJK extensions) as surrogate-pair escapes such as `😀`. The
  hand-written JSON parser rejected these with "invalid unicode code point", so any
  Python client sending an emoji via default `json.dumps` got a `400`.
  `parse_unicode_escape` now returns the raw `u32` code unit, and `parse_string`
  combines a high surrogate (`D800–DBFF`) with a following low surrogate
  (`DC00–DFFF`) into the real scalar (`0x10000 + ((hi−D800)<<10) + (lo−DC00)`);
  lone or mismatched surrogates error cleanly. Literal UTF-8 emoji still works.
  Std-only; 2 tests.
- **ADR-076 Config-file parity for `no_nudge` / `allow_sensitive_cloud` (IMP-config-file-parity).**
  Both flags were settable via env (`PASTURE_NO_NUDGE`, `PASTURE_ALLOW_SENSITIVE_CLOUD`)
  but the config-file `apply()` match silently ignored them — a recognised key dropped
  on the floor. `apply()` now handles both with the standard `matches!(val,
  "1"|"true"|"yes")` boolean parse used by `cascade`/`local_only`/`inject_context`.
  Defaults stay `false`, so the privacy-preserving behaviour is unchanged unless
  explicitly enabled. Std-only; 1 test.
- **ADR-075 Detect `ghu_`/`ghs_`/`ghr_` and AWS STS `ASIA` credentials (IMP-detect-cred-variants).**
  The PII classifier already flags `ghp_` and `AKIA`; the GitHub user/server/refresh
  token variants (`ghu_`, `ghs_`, `ghr_`) and AWS STS temporary credentials (`ASIA…`)
  are equally sensitive but went undetected, so a leaked one could reach the cloud.
  Added to `KEY_PREFIXES`, gated by the same `prefix + 12` length check (no short
  false positives). Detection is label-only, never values (I3); the over-classification
  bias is intentional (a false positive merely keeps a prompt local). Std-only; 1 test.
- **ADR-074 OpenAI error envelope `param`/`code` fields (IMP-error-envelope-fields).**
  OpenAI's error object always includes `param` and `code` keys (null when unknown);
  strict SDK deserializers (`openai-python` `APIError.param`/`.code`, LiteLLM) read
  them, and SDK retry/branch logic keys on `code`. Pasture emitted only
  `{message,type}`, breaking strict clients and dropping the rate-limit/auth signal.
  `build_error_response` now emits `param:null,code:null` on every error body; a new
  `build_error_response_coded(message,type,code)` helper populates `code` where it is
  unambiguous, and the rate-limit/auth gate sets `rate_limit_exceeded` (429) and
  `invalid_api_key` (401). Purely additive — no routing or security *decision*
  changes, only the error body shape (the ledger entry carries an explicit
  `risk:low` because the gate's keyword inference flags the literal "api key"/"auth"
  in the code strings as a false positive). Std-only; 4 tests.
- **ADR-073 Generate `X-Request-ID` when the client omits one (IMP-request-id-gen).**
  OpenAI and LiteLLM return an `x-request-id` on *every* response — minted
  server-side when the caller doesn't supply one — so every call is traceable.
  Pasture (IMP-request-id) only *echoed* a client-supplied id and emitted nothing
  otherwise, leaving most responses untraceable and most access-log lines without
  a `request_id`. `next_request_id()` mints a unique `req_<clock><counter>` id
  (atomic counter, mirroring `next_completion_id()`); `handle_connection` now sets
  `request_id = Some(supplied‑or‑generated)` immediately after parsing, so the
  existing echo header and access-log path carry it unchanged. No PII (clock +
  counter only). Purely additive, std-only; 2 tests.
- **ADR-072 `X-RateLimit-*` response headers (IMP-ratelimit-headers).** Pasture
  signalled rate state only reactively (a `429` with `Retry-After`). OpenAI, Azure,
  Anthropic and LiteLLM all expose `X-RateLimit-*` on *every* response so clients
  self-throttle *before* hitting a `429` (confirmed by GitHub research:
  `BerriAI/litellm`'s `OPENAI_RESPONSE_HEADERS`). `RateLimiter::snapshot()` refills
  the bucket to now without consuming a token and returns `(limit, remaining,
  reset_secs)`; `Proxy::ratelimit_headers()` formats `X-RateLimit-Limit-Requests`,
  `-Remaining-Requests`, and `-Reset-Requests` (`<n>s`) and is folded into the
  per-request `extra` header block, so every response — including the `429`, which
  still also carries `Retry-After` — advertises the budget. Snapshotted before the
  gate consumes the token, so `remaining` includes the in-flight request. Only the
  *request* family is emitted: Pasture meters requests, not tokens, so token-family
  headers would mislead. The helper returns `""` when the limiter is disabled (the
  localhost default), so there is zero overhead and no header noise in the common
  case. Additive, std-only; 4 tests.
- **ADR-071 `Retry-After` on 429 rate-limit responses (IMP-retry-after).** The
  rate limiter (IMP-15) returned `429` with no `Retry-After`, so a client had to
  guess a backoff and could busy-retry against an empty bucket. `RateLimiter`
  gains `retry_after_secs()` — whole seconds until the next token (`ceil`, never
  below 1 once empty, 0 when a token is available); a zero-rate bucket advises a
  conservative 60s. `check_gate` now holds the bucket mutex across the `allow()`
  check and the estimate so both reflect the same state, and returns the value as
  a fourth tuple element (set only for `429`). `handle_connection` prepends
  `Retry-After: <secs>` to the `429` response's header block. OpenAI, LiteLLM and
  nginx all send this header. Additive, std-only; 4 tests (3 limiter-math, 1
  keep-alive roundtrip asserting the header on the second, rate-limited request).
- **ADR-070 Machine approval gate for the improvement ledger (IMP-approval-gate).**
  Recursive self-improvement is bottlenecked by *human approval cost*. The ledger
  validator (`is_valid`) only checked that causal fields were *present*, so a human
  still had to read every entry to judge which changes were safe — approval scaled
  linearly with ledger size. This ADR makes approval **machine-checkable** so the
  reviewer's attention concentrates on the minority that needs it (the framing:
  in `RSI = Search × Verification × Compression`, Verification substitutes for the
  reviewer on the safe set). Each `Improvement` gains a `Risk` tier
  (`Low`/`Medium`/`High`) — an explicit optional `"risk"` field, else inferred from
  the change text; `Status::Retired` and any security/privacy/auth surface infer
  `High`. `Improvement::approval()` returns `Approval::Auto` iff the record is
  complete, cites `grounding` (provenance), shows verification evidence in `effect`
  (a test count / "verified" / "eval"), **and** is not `High` risk; otherwise it
  returns `NeedsReview(reasons)` naming the exact failed checks. Inference is
  deliberately over-cautious — a false `High` only costs an unnecessary review,
  while a false `Low` could pass a privacy regression (the same asymmetry as the
  PII classifier, I3). `pasture improvements --review` prints only the
  non-auto-approved entries plus the auto-approval rate; on the bundled ledger
  34/48 auto-approve (71%) and the surfaced minority is exactly the security /
  privacy changes and the early prose-verified / deferred entries (the gate does
  not special-case its own entry, which names security terms). The gate runs against
  the live ledger at CI time via the existing bundled-ledger test. Std-only; 9 tests.
- **ADR-069 RouterBench-format external eval loader (IMP-routerbench-loader).**
  The built-in 18-case set is the offline regression gate; validating routing on
  a user's own labelled prompts (or a public benchmark such as RouterBench) needs
  an external loader without a new dependency. `OwnedEvalCase { prompt: String,
  expected: Route }` holds heap-allocated prompts for dynamically loaded data.
  `load_eval_cases(path)` reads a JSONL file line by line (one object per line:
  `{"prompt":"…","expected":"local"|"cloud"}`), skipping blank lines and `//`
  comments; each line is parsed with `crate::json::parse` (the existing in-tree
  parser). `run_eval_owned` runs `OwnedEvalCase` slices through the identical
  routing pipeline as `run_eval` — privacy classifier → `decide_with_sensitivity`
  → tally. `pasture eval --external <file>` triggers the external path; the
  built-in path is unchanged. JSONL format matches the project's existing log
  conventions. Std-only (`std::io::BufRead`); 5 tests.
- **ADR-068 Cache key whitespace normalisation (IMP-cache-key-norm).**
  `request_key()` now trims leading/trailing whitespace from each message's
  `content` before hashing. Prompts that differ only in leading/trailing spaces
  (copy-paste artefacts, SDK padding) now share a cache key. Internal whitespace
  is left intact (code and formatted content preserve it). `str::trim()` returns a
  slice — zero allocation; backward-compatible (no entries are invalidated, they
  simply become reachable by more keys). 1 test.
- **ADR-067 Cache TTL eviction (IMP-cache-ttl).** Without TTL, stale cached
  responses are served indefinitely. `ResponseCache` now stores
  `(CompletionResponse, Instant)` pairs. `get(&mut self)` lazily evicts entries
  whose `elapsed() > max_age`, removing them from the map and counting as misses
  so `len()` stays accurate. `with_max_age(secs)` builder + `set_max_age(&mut self)`
  mutating variant (used by `Proxy::with_cache_ttl`). Enabled via
  `PASTURE_CACHE_TTL=<secs>` or `cache_ttl_secs =` config key. 0 (default) = no TTL.
  Std-only (`std::time::Instant`); 4 tests.
- **ADR-066 Structured per-request access log (IMP-access-log).** Operators need
  per-request visibility without parsing cost JSONL or scraping `/metrics`.
  `Proxy` gains `access_log: Option<String>` and `with_access_log` builder.
  `append_access_log` writes a JSONL line before each response: `ts` (Unix ms),
  `method`, `path` (no query string), `status`, `ms`, optional `request_id`. No
  prompt content, no auth tokens, no PII. Enabled via `PASTURE_ACCESS_LOG=<path>`
  or `access_log = <path>` config key. `handle_connection` uses local `wr!` / `wrp!`
  / `wrh!` macros that combine status tracking, logging, and response writing into
  one call, replacing 20+ scattered `write_response` calls. Off by default;
  std-only; 4 tests.
- **ADR-065 `/health` version field + 501 stubs for audio/images (IMP-health-version).**
  `/health` now returns `{"status":"ok","version":"<ver>"}` using `concat!/env!` at
  compile time so version is always correct. `POST /v1/audio/*` and
  `POST /v1/images/*` return **501 Not Implemented** (not 404) with error type
  `not_supported`, because 404 misleads SDK clients that call these endpoints
  unconditionally. `route_allowed_methods` covers them (wrong method → 405).
  Status reason table (`write_response`) gains 405, 415, 501 entries. 4 tests.
- **ADR-064 `X-Response-Time` header (IMP-response-time).** Operators and SDK
  clients need per-request latency without parsing JSONL logs. `handle_connection`
  captures `std::time::Instant::now()` after request parsing; a `te()` closure
  appends `X-Response-Time: <N>ms\r\n` to every response's extra-header block —
  success, error, HEAD, SSE (time-to-first-byte), and OPTIONS paths. Std-only
  (`std::time::Instant`); additive/backward-compatible; 3 tests.
- **ADR-063 Prometheus `/metrics` endpoint (IMP-metrics-prom).** Operators need
  a live, pull-based metrics endpoint compatible with Prometheus / Grafana without
  parsing JSONL logs. `build_metrics_response` emits Prometheus text exposition
  format v0.0.4: `HELP`/`TYPE` comment lines followed by labelled counter and gauge
  metrics (`pasture_requests_total{route=…}`, `pasture_tokens_total{type=…}`,
  `pasture_cache_*`, `pasture_cloud_cost_usd_total`). `handle_metrics` reads the
  cost log + live cache state (same sources as `/v1/stats`). `write_plain_response`
  sends `Content-Type: text/plain; version=0.0.4`. `/metrics` added to
  `route_allowed_methods` and the dispatch table; wrong-method returns 405. Std-only;
  3 tests.
- **ADR-062 `cache_size` / `cache_capacity` in `/v1/stats` (IMP-stats-cache-size).**
  `ResponseCache` gains a `cap()` accessor. `handle_stats` reads both `len()` and
  `cap()` from the cache mutex. `build_stats_response` gains two extra parameters
  and emits `cache_size` (current entry count) and `cache_capacity` (maximum).
  Both are 0 when the cache is disabled. Enables capacity-based tuning of
  `PASTURE_CACHE` without parsing JSONL logs. 2 tests.
- **ADR-061 `POST /v1/moderations` stub (IMP-moderations).** Many OpenAI SDK
  versions call `/v1/moderations` unconditionally; returning 404 breaks them
  silently. `handle_moderations` accepts the `input` field (any value), ignores
  it, and `build_moderations_response` returns a well-formed all-categories-safe
  OpenAI moderation object. Dispatch arm added; 405 for wrong method. Pasture
  does not run real content moderation — the stub is explicitly labelled. 2 tests.
- **ADR-060 Reject `n > 1` with 400 Bad Request (IMP-n-validation).**
  `parse_request` now reads the `n` field. If `n` is present and not 1 (including
  negative values and zero), a `ProxyError::BadRequest` is returned with a message
  explaining the constraint. Silently returning 1 completion when `n:3` was
  requested violates the API contract; an explicit error is always better. 3 tests.
- **ADR-059 Model-pinned routing (IMP-model-pinning).** Clients that specify an
  explicit model name in the request (e.g. `"model":"gpt-4o-mini"`) expect to
  reach the corresponding backend regardless of what the routing engine would
  normally decide for a short prompt. `classify_and_decide` now checks `req.model`
  against the configured `local_model_name` and `cloud_model_name` (from
  `PASTURE_LOCAL_MODEL` / `PASTURE_CLOUD_MODEL`) and the sentinels `"local"` /
  `"cloud"`. When a match is found the route is forced via `decide_full(forced=…)`,
  which still respects the privacy override (sensitive content stays local even
  with `model:"cloud"`). `with_model_names` builder; wired in `cli.rs`. 5 tests.
- **ADR-058 415 Unsupported Media Type for non-JSON POST bodies (IMP-content-type).**
  `read_request` now parses and lowercases the `Content-Type` header. For POST
  requests, if `Content-Type` is present and does not start with
  `application/json`, the server returns 415 (RFC 7231 §6.5.13) with a clear
  error message. Absent `Content-Type` is still accepted (bare `curl` and minimal
  clients don't set it); `charset=utf-8` suffixes pass. 3 tests.
- **ADR-057 Live cache hit/miss counters in `/v1/stats` (IMP-cache-counters).**
  `ResponseCache` gains two `AtomicU64` fields: `hits` and `misses`. `get()`
  increments the appropriate counter on every lookup. `hits()` / `misses()`
  accessors return the current values. `handle_stats` reads the live counters
  via the `self.cache` mutex, and `build_stats_response` appends them as
  `cache_hits` and `cache_misses` to the JSON response. The existing
  `cache_rate` field still derives from the JSONL cost log; these counters
  provide real-time effectiveness data. Std-only (no new deps). 3 new tests.
- **ADR-056 Configurable system prompt (IMP-system-prompt).** Single-user PC
  assistant deployments need a persistent persona prompt (e.g. `"You are a
  coding assistant specialising in Rust"`) that frames every request without
  requiring the user to include it in every chat turn. `prepend_system_prompt`
  merges the configured prompt with any existing system message in the request
  (configured prompt first so it sets the outer frame). Applied first in
  `run_completion`, before `inject_context`, so the ordering is: configured
  system prompt → date/OS context → user messages. Set via `PASTURE_SYSTEM_PROMPT`
  env var or `system_prompt` config-file key. Empty string disables the feature.
  `with_system_prompt` builder method. 4 tests.
- **ADR-055 `POST /v1/completions` legacy shim (IMP-legacy-completions).**
  Many older LLM clients (pre-chat OpenAI SDK, LM Studio, older LangChain)
  default to `POST /v1/completions` (text-completion API, now deprecated) and
  silently break with a 404. `parse_legacy_completion` maps the `prompt` field
  (string or string array) to a single `user` message so the request flows
  through the same routing/privacy/cache pipeline. `build_legacy_completion_response`
  formats the output as `"object":"text_completion"` with `choices[].text` and
  a `cmpl-` ID prefix matching OpenAI convention. `handle_legacy_completion`
  wraps both. Route `/v1/completions` added to dispatch and `route_allowed_methods`
  (returns 405 for wrong method). Streaming not supported via this shim. 6 tests.
- **ADR-054 `tool_choice` as a hard escalation signal (IMP-tool-choice).**
  `parse_request` previously set `has_tools` only when a non-empty `tools` or
  `functions` array was present. Some clients send `tool_choice` without a
  `tools` array (e.g. `"tool_choice":"required"`) or use `"tool_choice":"auto"`
  to hint intent without listing tools. Any `tool_choice` value that isn't `"none"`
  now sets `has_tools=true`, routing those requests to the stronger model. The
  `"none"` value explicitly opts out and is not escalated. Closes the SPEC.md
  deferred item. 4 tests.
- **ADR-053 405 Method Not Allowed for known routes (IMP-http-methods).**
  The catch-all `else` branch previously returned 404 for any unmatched request,
  including wrong methods on known routes. `route_allowed_methods` now maps each
  known path prefix to its allowed methods; when a wrong method hits a known path
  the response is 405 with an `Allow:` header (RFC 7231 §6.5.5). Unknown paths
  still return 404. Std-only. 3 tests.
- **ADR-052 X-Request-ID echo (IMP-request-id).** Clients correlate async
  responses and distributed traces via a caller-supplied `X-Request-ID` header.
  `read_request` parses the header and strips `\r\n` from its value (CRLF-injection
  guard). `handle_connection` builds an `extra` block = CORS headers +
  `X-Request-ID: <value>\r\n` (or just CORS if absent) and passes it to every
  response path including SSE. Absent when the client omits the header. Purely
  additive, std-only; 3 socket round-trip tests. Matches OpenAI API and LiteLLM
  behaviour.
- **ADR-051 `HEAD /health` and `/v1/engines` alias (IMP-compat-methods).**
  (1) `HEAD /health`: accepted alongside `GET`; response has identical headers —
  including `Content-Length` set to the GET body length — but no body (RFC 7231
  §4.3.2). Required by monitoring tools that use HEAD for liveness probes.
  `write_head_response` helper keeps the logic DRY. 1 test.
  (2) `/v1/engines[/{id}]` aliased to `/v1/models[/{id}]` in the dispatch
  handler; the deprecated OpenAI "engines" path is still used by old SDK versions
  and some LLM clients; aliasing avoids 404 during model discovery. 2 tests.
- **ADR-050 HTTP/1.1 keep-alive connection reuse (IMP-keepalive).** The thread-pool
  server previously closed the TCP connection after every request (HTTP/1.0 style),
  forcing clients to re-connect for each call — one round-trip per request. The
  `handle_connection` loop now serves up to 100 sequential requests per connection:
  a per-connection `conn_buf: Vec<u8>` accumulates read-ahead bytes so that
  pipelined request bytes captured in the first `read()` aren't discarded; `drain`
  removes only the consumed bytes, preserving the rest for the next iteration.
  Connection semantics: HTTP/1.1 defaults to keep-alive; HTTP/1.0 to close;
  `Connection: close` from either side, any error response, or an SSE stream all
  terminate immediately. The slow-loris timeout and body cap apply per-request.
  `write_response` gains a `keep_alive: bool` param for the `Connection:` header.
  Std-only; 4 tests covering pipelining, close-on-demand, header presence, HTTP/1.0.
- **ADR-049 `logprobs: null` in choice objects (IMP-logprobs-field).** OpenAI always
  includes a `logprobs` key in each choice (null unless the client requested logprobs);
  Pasture omitted it, so strict client schema validators could reject the response.
  Both `build_openai_response` and `build_openai_chunk` now emit `logprobs:null` in the
  choice object (the usage chunk has an empty `choices` array, so it is unaffected).
  Purely additive, std-only; 2 tests (buffered choice, chunk choice).
- **ADR-048 `model` field in streaming chunks (IMP-chunk-model).** OpenAI includes the
  `model` name in every `chat.completion.chunk` and in the streaming usage chunk; Pasture
  omitted it, so clients logging or displaying the per-chunk model received nothing.
  `build_openai_chunk` and `build_openai_usage_chunk` now accept an explicit `model`
  parameter (after `id`); the streaming path passes `req.model` (stored before the
  closure to avoid borrow conflicts). All chunks of one stream share the same value.
  3 tests: delta chunk, usage chunk, cross-chunk consistency.
- **ADR-047 `system_fingerprint` on responses and stream chunks (IMP-fingerprint).**
  OpenAI clients may key caching invalidation, dedup, or change detection on the
  `system_fingerprint` field. Pasture omitted it entirely, breaking such clients.
  A deterministic `fp_pasture_XXXXXXXX` string is now computed per model name via
  FNV-1a (64-bit) truncated to 32 bits — identical model always yields the same
  fingerprint, different models yield different fingerprints, no I/O required, std-only.
  All chunks of one SSE stream share the fingerprint computed once at stream start
  (same-stream consistency that mirrors OpenAI's model-snapshot semantics).
  4 tests: determinism + prefix, response field, chunk field, stream consistency.
- **ADR-046 Per-connection socket timeout (IMP-timeout).** `read_request` had no
  timeout, so a slow or dead client could hold one of the bounded worker threads
  (2..=32) indefinitely — a handful of such connections exhaust the pool and deny
  service (slow-loris). `serve` now sets a per-connection read **and** write timeout
  (`PASTURE_REQUEST_TIMEOUT`, default 30s; 0 disables), and a read that times out maps to
  a `408` response (`ReadOutcome::TimedOut`) rather than propagating, freeing the worker.
  A natural continuation of the existing DoS caps (body 16 MiB, header 1 MiB, parser
  depth 128, rate-limit). std-only `TcpStream` timeouts; 3 tests including a fast 50 ms
  timeout round-trip against an incomplete request.
- **ADR-045 `GET /v1/models/{id}` (IMP-model-retrieve).** The OpenAI SDK's
  `models.retrieve(id)` and some client validation flows hit the single-model endpoint;
  Pasture served only the list, and `/v1/models/{id}` fell through to it via
  `starts_with`. The dispatch now splits the sub-path: a bare `/v1/models` lists, while
  `/v1/models/{id}` returns the model object when the id is configured (OpenAI shape) or
  a `404` envelope otherwise. Query strings and trailing slashes are tolerated. Purely
  additive, std-only; `build_model_response` is a pure builder, 5 tests.
- **ADR-044 Unique completion ids (IMP-completion-id).** Every Pasture response used a
  constant `id:"pasture"`, but OpenAI returns a unique `chatcmpl-…` per completion that
  logging, tracing, and de-duplication tooling keys on — a constant id silently breaks
  them. Responses now carry a unique id (process-global atomic counter + wall-clock
  prefix); for streaming, one id is generated per stream and threaded through every chunk
  (`build_openai_chunk` / `build_openai_usage_chunk` take the id) so all chunks of a
  response share it, matching OpenAI. std-only, no allocation on the hot path beyond the
  id string; unit-tested for uniqueness and per-stream consistency.
- **ADR-043 Structured-output passthrough (IMP-response-format).** OpenAI's
  `response_format` (JSON mode and `json_schema` structured outputs) is supported by
  every backend Pasture fronts (OpenAI, vLLM, LM Studio, Ollama), but the proxy parsed
  only `messages`/`model`/`stream`/tools/sampling and silently dropped it, so a JSON-mode
  request returned free-form text. It is now captured (as a raw `JsonValue`) and
  forwarded: verbatim for OpenAI/OpenAI-compat, and translated for Ollama, whose control
  lives in a top-level `format` field (`{"type":"json_object"}` → `"json"`;
  `{"type":"json_schema",…}` → the embedded schema). This required a JSON **serializer**
  (`JsonValue::to_json_string`) — the hand-rolled lib could parse but not round-trip; the
  serializer emits `BTreeMap`-sorted keys so output is deterministic and reusable for the
  cache key, which now includes `response_format`. std-only; 9 tests (serializer
  round-trip, per-backend emission, parse, cache distinction).
- **ADR-042 Streaming token usage (IMP-stream-usage).** OpenAI's streaming API emits a
  final chunk carrying `usage` when the client sets `stream_options.include_usage`;
  peers (LiteLLM, vLLM) do too, but Pasture's SSE path reported no token counts, so
  clients tracking cost/length on streamed responses got nothing. The proxy now parses
  `stream_options.include_usage` (without touching `CompletionRequest`, to avoid
  rippling through its constructors) and, when set, emits one extra chunk with an empty
  `choices` array and a `usage` object (prompt/completion/total) before `data: [DONE]`.
  Off unless requested, so the default stream is byte-for-byte unchanged. The backend's
  returned `CompletionResponse` already carries the counts; `build_openai_usage_chunk`
  is a pure builder, unit-tested, plus stream round-trips with and without the flag.
- **ADR-041 CORS / OPTIONS preflight (IMP-cors).** Browser-based clients (Open WebUI
  web, custom dashboards) cannot call the proxy without CORS preflight handling, which
  every peer (Ollama via `OLLAMA_ORIGINS`, LM Studio, LiteLLM) provides. `PASTURE_CORS_ORIGINS`
  (a comma-separated allow-list, or `*`) opts in: `OPTIONS` is answered with `204` and the
  `Access-Control-Allow-*` preflight headers *before* the auth/rate-limit gate (preflight
  is credential-free), and `Access-Control-Allow-Origin` is reflected on every response
  (buffered and SSE) so the browser can read both success and error bodies; a specific
  (non-`*`) origin also gets `Vary: Origin`. Off by default — a localhost server with
  permissive CORS is reachable by any website the user visits, so this is opt-in (the
  same lesson Ollama learned). The `Origin` header is captured in `read_request`;
  `CorsPolicy` and the header builders are pure and unit-tested; deterministic, std-only.
- **ADR-040 Optional auth + rate-limit (IMP-15).** The proxy binds `127.0.0.1` by
  default (I5), but users do expose it (`PASTURE_LISTEN_ADDR=0.0.0.0:…`), and there
  was no gate. Two opt-in, std-only protections now guard `/v1/*` (with `/health`
  always exempt for liveness probes): `PASTURE_AUTH_TOKEN` requires an
  `Authorization: Bearer <token>` header, compared in constant time; and
  `PASTURE_RATE_LIMIT` (requests/min) applies a global token-bucket
  (`ratelimit::RateLimiter`, continuous refill, burst = the budget). Both are
  evaluated by a single pure `check_gate` before route dispatch, returning `401`/`429`
  in the OpenAI error envelope. The limiter is split from the wall clock (`step`)
  so it is unit-tested deterministically without sleeps. `serve` warns when bound to
  a non-localhost address without a token. Off by default — the zero-config
  single-user localhost path is unchanged. Grounded in COMPETITIVE.md IMP-15 and
  peer-gateway auth (LiteLLM/Portkey).
- **ADR-039 Sampling-parameter passthrough (IMP-sampling).** Every peer gateway
  (LiteLLM, OpenRouter, Ollama, LM Studio, vLLM) honours the client's sampling
  parameters; Pasture parsed only `messages`/`model`/`stream`/tools and silently
  dropped `temperature`, `top_p`, `max_tokens`, `stop`, `seed`, and the penalties —
  so a client requesting `temperature:0` for deterministic output, or `max_tokens`
  for a cost cap, was ignored. A new `SamplingParams` on `CompletionRequest` is
  filled by `parse_request` (non-finite numbers rejected; `stop` accepts string or
  array; `max_completion_tokens` aliases `max_tokens`) and serialised per backend:
  OpenAI/OpenAI-compat as top-level fields, Ollama under `options` with the length
  cap renamed `num_predict`, Anthropic honouring the client `max_tokens` (instead of
  the hardcoded 1024) plus `temperature`/`top_p`/`stop_sequences`. The cache key now
  includes the sampling params (hashing each `f64` by bit pattern) so a `temperature:0`
  response is never served to a `temperature:1` request. std-only; 11 tests across
  parse/build/cache.
- **ADR-038 Live metrics endpoint (IMP-metrics, IMP-16).** Observability was
  CLI-only (`stats` reads the cost log). Peer gateways expose a live metrics view;
  the proxy now answers `GET /v1/stats` with a JSON snapshot of the same PII-free
  counters (`total`, per-route counts, cloud/cache rates, token totals, spend),
  reusing `cost::summarize` over `cost::read_log`. Purely additive and read-only —
  no routing behaviour changes; a missing cost log reads as all-zeros. std-only;
  localhost-default, so no auth is implied (I5). `build_stats_response` is a pure
  builder, unit-tested alongside a socket round-trip. JSON-format (not Prometheus)
  to avoid a new format contract; the `object` field is `pasture.stats`.
- **ADR-037 Self-improvement ledger (IMP-13).** Pasture's improvement history
  lived only as human prose across `CHANGELOG.md`, this file, and `COMPETITIVE.md`.
  Framed by the recursive-self-improvement argument that the durable asset is a
  *verified improvement history* (Compression in `RSI = Search × Verification ×
  Compression`) rather than the model, the history is now a machine-readable asset:
  `IMPROVEMENTS.jsonl` records each change as a causal entry (change / reason /
  effect / status / grounding), parsed by `src/improve.rs` with the crate's own
  zero-dependency JSON reader and surfaced via `pasture improvements`. A
  compile-time test (`include_str!` + validator) asserts every bundled entry parses
  and is a valid, explainable improvement, so the record cannot rot silently — the
  Verifier applied to the asset itself. Deliberately small and in-philosophy: one
  data file, one std-only parser, one CLI surface, zero new dependencies. The
  larger RSI vision (P2P compute, learned routers, evolution engines, self-
  generating infra, content logging) is rejected as antithetical to I1–I5 / ADR-002
  / ADR-004; see `SELF_IMPROVEMENT.md` for the full mapping and anti-goals.
- **ADR-036 Privacy/PII detection hardening (10 categories).** The original 7-category
  classifier missed three common leakage vectors: PEM private keys pasted from key
  files, database/service URLs with embedded credentials (`postgres://user:pass@host`),
  and `.env` file contents or shell session pastes containing `SECRET=value` assignments.
  Three new detection functions (`contains_pem_key`, `contains_url_credential`,
  `contains_env_secret`) close these gaps. The keyword list is expanded with 14 EN and
  10 JA terms covering financial (IBAN, routing number, bank account), government ID
  (driver's license, date of birth, 運転免許, 在留カード, 生年月日), and credential
  vocabulary (bearer token, refresh token). The `KEY_PREFIXES` list gains 9 vendor
  prefixes (Stripe, SendGrid, Google OAuth, npm, DigitalOcean, HashiCorp Vault,
  Cloudflare). All functions return category labels only — never matched values (I3/I5).
  Design bias remains toward over-classification: a false positive keeps a prompt local
  (cheap); a false negative leaks data (unacceptable). 26 new tests; std-only.
- **ADR-035 GPU-less / local-only PC enhancement (IMP-new).** Three features for
  no-GPU / air-gapped machines that together turn Pasture into a capable PC assistant
  without any cloud dependency: (1) `PASTURE_LOCAL_ONLY` — routes all traffic local
  regardless of hard signals or token length; the routing engine's privacy rules still
  apply, so sensitive content is still handled correctly. (2) `PASTURE_LOCAL_FAST_MODEL`
  — dual-local routing: simple prompts (no hard signals, < `fast_threshold` tokens)
  use a tiny fast model (e.g. Phi-3-mini, Qwen2.5-1.5B), harder ones use the main
  local model; grounded in the two-tier local cascade idea from RESEARCH.md cat.1/cat.8.
  (3) `PASTURE_INJECT_CONTEXT` — prepends a system message with the current UTC date
  and OS name so lightweight models can answer date/time and system questions correctly;
  existing system messages are merged, not duplicated. All three features are std-only,
  zero-dependency, and off by default; `pasture models` gains a CPU-only ultra-light
  model tier (Phi-3-mini/Gemma-2-2B/Qwen2.5-1.5B/TinyLlama) with setup tips.
- **ADR-034 `POST /v1/embeddings` pass-through (IMP-8 completion).** Several clients
  (including LlamaIndex, LangChain, and semantic-cache implementations) call
  `/v1/embeddings` before or alongside chat. The proxy now routes this to the local
  backend only — Ollama `/api/embed` or the OpenAI-compat `/v1/embeddings` sibling —
  and returns the standard OpenAI embeddings shape. Cloud escalation is not applied
  (embeddings are local-only; grounded in IMP-12 semantic-cache infra, RESEARCH.md
  cat.3). A new `Backend::embeddings` trait method (default = `Unsupported`) keeps
  the `MockBackend` deterministic (char-count vectors); `OllamaBackend` and
  `OpenAiCompatBackend` implement it. `handle_embeddings`, `parse_embeddings_request`,
  `build_embeddings_response`, and `fmt_float_array` (finite-safe) live in `proxy.rs`.
  Std-only, zero new dependencies. SPEC.md §3.2b now normative for this endpoint.

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

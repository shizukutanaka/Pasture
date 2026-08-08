# Competitive & Research Analysis

> Status: living document. This is the file the `ARCHITECTURE.md` ADRs refer to
> (e.g. ADR-006 "COMPETITIVE.md §6", ADR-007). It maps Pasture against peer tools
> and recent literature, and tracks the **forward improvement backlog (IMP-8 →)**.
> IMP-1…IMP-7 are already shipped — see `CHANGELOG.md` and the ADRs.
>
> The upstream, category-by-category source survey behind this backlog (10 product
> areas × ~10 arXiv/GitHub sources, plus the new candidates IMP-18 → IMP-27) lives
> in **[RESEARCH.md](RESEARCH.md)**.

This analysis answers one question: *given everything similar software and recent
arXiv work do, what should Pasture do next — without betraying what makes it
different?*

---

## 1. Positioning

Pasture is a **zero-config, single-binary, std-only, single-user** routing proxy.
For each OpenAI-compatible request it decides — deterministically and from the
machine's *actual* hardware — whether to answer **local** (free, private, offline)
or escalate to **cloud** (hard tasks). That niche is deliberately narrow.

The peer field splits into two camps, and Pasture sits between them:

- **Developer-infra gateways** (LiteLLM, Portkey, OpenRouter): rich multi-provider
  routing, auth, rate-limits, dashboards — but they are *services you operate*
  (servers, config, often Redis / a DB). They assume a team and a deployment.
- **Research routers** (RouteLLM, vLLM Semantic Router, R2-Router): strong
  cost/quality routing, but **trained** on preference/quality labels and usually
  needing a GPU and a Python stack. They assume a dataset.

Pasture's wedge is the individual on one machine who wants *automatic* local↔cloud
routing with **no servers, no training data, no dependencies, and privacy by
default**. Every improvement below is judged against whether it keeps that wedge.

---

## 2. Peer landscape

Legend: ✅ yes · ➖ partial / opt-in · ❌ no · — N/A

| Capability | **Pasture** | LiteLLM | RouteLLM | vLLM Semantic Router | GPTCache | Portkey | OpenRouter |
|---|---|---|---|---|---|---|---|
| Routing type | deterministic + hw-adaptive + cascade | manual/config | **learned** (pref labels) | **embedding** classifier | — (cache only) | rules + fallback | marketplace |
| Needs training data | ❌ (label-free) | ❌ | ✅ | ✅ | ➖ (embeddings) | ❌ | ❌ |
| Local-first / offline | ✅ | ➖ | ➖ | ➖ | ➖ | ❌ (SaaS) | ❌ (SaaS) |
| Privacy / PII-aware routing | ✅ (forced-local) | ❌ | ❌ | ➖ | ❌ | ➖ (guardrails) | ❌ |
| `/v1/chat/completions` | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ |
| `/v1/models` list | ❌ **(gap)** | ✅ | ➖ | ✅ | — | ✅ | ✅ |
| `/v1/embeddings` | ❌ **(gap)** | ✅ | ❌ | ✅ | — | ✅ | ✅ |
| Tool / function calling | ❌ **(gap)** | ✅ | ➖ | ➖ | — | ✅ | ✅ |
| Streaming (SSE) | ✅ (all routes) | ✅ | ➖ | ✅ | — | ✅ | ✅ |
| Fallback / retry on error | ➖ (cloud→local only) | ✅ | ❌ | ➖ | — | ✅ | ✅ |
| Exact-match cache | ✅ | ➖ | ❌ | ➖ | ✅ | ✅ | ➖ |
| **Semantic** cache | ❌ **(gap)** | ➖ | ❌ | ➖ | ✅ | ✅ | ❌ |
| Auth / API keys (proxy side) | ❌ **(gap)** | ✅ | — | ➖ | — | ✅ | ✅ |
| Rate-limit / quota | ❌ **(gap)** | ✅ | — | ➖ | — | ✅ | ✅ |
| Live metrics endpoint | ➖ (`stats` from JSONL) | ✅ | ❌ | ✅ | ➖ | ✅ | ✅ |
| Web dashboard / UI | ✅ (embedded, zero-asset) | ✅ | ❌ | ➖ | ❌ | ✅ (SaaS) | ✅ (SaaS) |
| Cost logging | ✅ (PII-free JSONL) | ✅ | ➖ | ➖ | — | ✅ | ✅ |
| Dependencies / footprint | **zero-dep, 1 binary** | Python + deps | Python + ML | Python + vLLM | Python + vector DB | SaaS/agent | SaaS |

**Reading of the table.** Pasture already leads on the things its peers mostly
*don't* do (hardware-adaptive + label-free routing, PII-forced-local, zero-dep,
all-route streaming). The honest gaps cluster in two places:
**(a) OpenAI API-surface parity** (`/v1/models`, `/v1/embeddings`, tool-calling) and
**(b) gateway-operational maturity** (retry/fallback, semantic cache, auth,
rate-limit, live metrics). The backlog below targets exactly those, in priority
order, each constrained to keep the zero-dep default and the single-user/privacy
philosophy intact.

---

## 3. arXiv grounding (new references)

These motivate the backlog. References Pasture already cites — FrugalGPT
(2305.05176), the routing survey (2506.06579), PRISM (2511.22788), mean-logprob
confidence (2605.02241), UCCI (2605.18796), and 2502.04428 / 2605.06350 — are
reused below without re-introduction.

| Ref | Work | Relevance to Pasture |
|---|---|---|
| **arXiv:2603.04445** | *Dynamic Model Routing and Cascading for Efficient LLM Inference: A Survey* | Taxonomy of routing paradigms — difficulty, preference, **clustering/embedding**, **uncertainty quantification**, RL, multimodal, cascade. Frames what Pasture covers (difficulty + cascade) vs. doesn't (clustering, calibrated uncertainty) → IMP-13, IMP-14, IMP-17. |
| **arXiv:2605.18796** | *UCCI: Calibrated Uncertainty for Cost-Optimal Cascade Routing* | Maps token-margin uncertainty → per-query **error probability** via isotonic regression (calibration-first). Already cited by ADR-028 for *rate* calibration; the **error-probability** step is the unrealised part → IMP-13. |
| **arXiv:2603.03301** | *From Exact Hits to Close Enough: Semantic Caching for LLM Embeddings* | Cosine/L2 similarity cache; threshold tuning (~0.92 start) with false-positive monitoring → IMP-12. |
| **arXiv:2402.01173** | *Efficient Prompt Caching via Embedding Similarity* | Theory + method for embedding-similarity cache hits → IMP-12. |
| **arXiv:2411.05276** | *GPT Semantic Cache* | Reports ~60–69% API-call reduction at high accuracy with a similarity cache → IMP-12 value case. |
| **arXiv:2403.12031** | *RouterBench* | Standard cost/quality routing benchmark + format → IMP-17 (external eval), and a public yardstick for the `eval` harness. |
| **arXiv:2602.02823** | *R2-Router* | Reasoning-aware routing — a peer data point on difficulty estimation; informs IMP-10/IMP-14 framing. |

---

## 4. Improvement backlog (IMP-8 → IMP-17)

Each item: **what / why / peer / arXiv / effort / risk / zero-dep default?**
"Zero-dep default? ✅" means the feature is std-only or opt-in such that the
**default** build remains zero-dependency.

### Tier 1 — Low-risk API parity, fits the philosophy (std-only)

#### IMP-8 — `/v1/models` and `/v1/embeddings` endpoints  — ✅ SHIPPED
- **What:** `GET /v1/models` (return the configured local + cloud model ids in
  OpenAI list shape) is **implemented** (ADR-030; `build_models_response` +
  `Proxy::with_models`). `POST /v1/embeddings` (pass through to the local backend's
  embeddings endpoint) remains the follow-up.
- **Why:** Many OpenAI clients probe `/v1/models` on connect and **fail or hide
  Pasture** when it 404s; `/v1/embeddings` is the most-requested non-chat call.
  Pure API-surface parity, no routing logic.
- **Peer:** LiteLLM, Portkey, OpenRouter, vLLM Router (table-stakes).
- **arXiv:** — (compatibility, not research).
- **Effort:** S · **Risk:** low · **Zero-dep default?** ✅ (std-only; embeddings
  proxy reuses the existing local-HTTP path).

#### IMP-9 — Cloud transient-error retry + provider fallback chain  — ✅ **FULLY SHIPPED (ADR-032, ADR-136)**
- **What:** **Implemented** (ADR-032 + ADR-136). On a cloud 5xx/timeout/connection error, retry
  with bounded exponential backoff (`PASTURE_CLOUD_RETRY`, default 2). When the primary exhausts
  retries, try a secondary cloud provider (`PASTURE_CLOUD_FALLBACK_PROVIDER`, e.g. `anthropic` as
  fallback when OpenAI is down) before falling back to local. 4xx errors are non-retryable.
- **Peer:** LiteLLM / Portkey failover & retries.
- **arXiv:** — (reliability).
- **Effort:** S–M · **Risk:** low · **Zero-dep default?** ✅ (std-only).

#### IMP-10 — Tool / function-calling awareness  — ✅ SHIPPED
- **What:** **Implemented** (ADR-031). A non-empty `tools`/`functions` array is
  detected in `parse_request` and treated as a **hard signal** (escalate to cloud,
  where tool-use is reliable) via `RoutingEngine::decide_full`; the fields pass
  through unchanged. Privacy still wins (sensitive stays local) and the signal is
  gated by the `code_to_cloud` rule.
- **Why:** These fields were previously unparsed, so a tool-calling request could be
  routed to a small local model that ignores them → broken agentic clients.
- **Peer:** LiteLLM, Portkey, OpenRouter.
- **arXiv:** 2603.04445 (difficulty paradigm); 2602.02823 (reasoning-aware).
- **Effort:** S · **Risk:** low–med (must pass fields through faithfully) ·
  **Zero-dep default?** ✅ (reuses the std-only JSON parser).

#### IMP-11 — Cache-key normalization (near-exact hits)  — ✅ SHIPPED
- **What:** Before hashing the cache key in `cache.rs`, canonicalize messages
  (trim/collapse whitespace, normalize case for the match key, drop trailing
  punctuation). Still deterministic and exact-on-normalized-form.
- **Why:** Cheap hit-rate lift toward the semantic-cache benefit **without any
  embedding dependency**; a stepping-stone to IMP-12. Sensitive prompts stay
  never-cached (existing `privacy.rs` rule is unchanged).
- **Peer:** GPTCache (motivation); LiteLLM/Portkey caches.
- **arXiv:** 2603.03301 (the "exact → close-enough" framing).
- **Effort:** S · **Risk:** low (normalization must be conservative to avoid
  collapsing semantically-distinct prompts) · **Zero-dep default?** ✅.

### Tier 2 — Research-grounded routing/cache upgrades (opt-in)

#### IMP-12 — Optional semantic cache via local embeddings  — ✅ SHIPPED (ADR-123)
- **What:** Opt-in (`PASTURE_SEMANTIC_CACHE=…`) cache that embeds the prompt via the
  **local backend's `/v1/embeddings`** and counts a hit when cosine similarity ≥ a
  tunable threshold (default ~0.92). Log near-miss distances so the user can monitor
  the false-positive rate and tune the threshold. **Off by default.**
- **Why:** GPT Semantic Cache reports ~60–69% call reduction at high accuracy
  (2411.05276); this is the single biggest cloud-cost lever after exact-match.
  Crucially it can be done **with no new crate** — it reuses the OpenAI-compat local
  HTTP path, so the **default** build stays zero-dependency. Sensitive prompts are
  never cached (privacy override, I5).
- **Peer:** GPTCache, Portkey semantic cache.
- **arXiv:** 2603.03301, 2402.01173, 2411.05276.
- **Effort:** M · **Risk:** med (bad hits return a wrong cached answer — mitigated by
  a conservative default threshold + FP monitoring + opt-in) · **Zero-dep default?**
  ✅ (opt-in; uses local embeddings, no TLS/no vector-DB crate).

#### IMP-13 — Calibrated-uncertainty escalation (complete the UCCI step)  — ✅ SHIPPED (ADR-124)
- **What:** Extend `calibrate --logprob` (ADR-028) from a target-*rate* quantile to a
  **monotone (isotonic-style, std-only) map from local mean-logprob → estimated error
  probability**, fit on the 18-case `eval.rs` set plus any user-supplied labels.
  The cascade knob then becomes "escalate when P(local is wrong) > budget" — a
  *target-accuracy* control, not just a *budget* control.
- **Why:** ADR-028 is explicit that today's calibration controls the escalation
  *rate*, **not** correctness-calibrated optimality, because labels are scarce. UCCI
  shows the missing piece is the uncertainty→error-probability mapping; pooled isotonic
  regression is computable in std Rust and degrades gracefully with few labels.
- **Peer:** RouteLLM (learned, label-heavy) — Pasture's label-light variant.
- **arXiv:** 2605.18796 (UCCI), 2603.04445 (uncertainty paradigm), building on
  2605.02241.
- **Effort:** M · **Risk:** med (must stay honest about label scarcity; ship as
  advisory like the existing calibrate output) · **Zero-dep default?** ✅ (offline,
  std-only math).

#### IMP-14 — Optional embedding/clustering difficulty signal  — ✅ SHIPPED (ADR-125)
- **What:** Using the same local-embeddings infra as IMP-12, add an optional routing
  input: distance from the prompt embedding to a small set of "known-hard" centroids
  (derived from the eval set / user history) contributes to the escalation decision
  alongside the deterministic heuristic.
- **Why:** The survey's clustering paradigm and embedding routers (vLLM Semantic
  Router) outperform pure keyword heuristics on difficulty estimation; this captures
  some of that gain while keeping the deterministic rules as the always-on baseline.
- **Peer:** vLLM Semantic Router, RouteLLM (clustering/MF).
- **arXiv:** 2603.04445 (clustering), 2602.02823.
- **Effort:** M–L · **Risk:** med (non-determinism creeps in — gate strictly behind
  opt-in and keep deterministic path default) · **Zero-dep default?** ✅ (opt-in).

### Tier 3 — Deployment maturity (opt-in, off by default)

#### IMP-15 — Optional bearer-token auth + token-bucket rate-limit  — ✅ SHIPPED
- **What:** When the proxy is bound to a non-localhost address, optionally require a
  bearer token (`PASTURE_PROXY_TOKEN`) and apply a std-only token-bucket rate-limit.
  Localhost default behaviour is unchanged (trusted single user).
- **Why:** Already named as future work (`CHANGELOG.md:24`). It's the prerequisite for
  *any* safe shared/LAN exposure, which users will inevitably try.
- **Peer:** LiteLLM, Portkey, OpenRouter.
- **arXiv:** — (deployment security).
- **Effort:** M · **Risk:** med (security-sensitive — constant-time token compare,
  fail-closed) · **Zero-dep default?** ✅ (std-only; off by default).

#### IMP-16 — Live metrics endpoint  — ✅ SHIPPED
- **What:** `GET /metrics` (Prometheus text) or `GET /v1/stats` (JSON) exposing the
  same counters the cost log already accumulates (route split, cloud rate, cache hit
  rate, tokens, spend, cascade-confidence distribution).
- **Why:** Today insight requires `pasture stats` re-parsing the JSONL after the fact;
  a live endpoint enables dashboards/alerts without any new storage.
- **Peer:** LiteLLM, Portkey, vLLM Router.
- **arXiv:** — (observability).
- **Effort:** S–M · **Risk:** low · **Zero-dep default?** ✅ (std-only; reuses
  `cost.rs` aggregation).

#### IMP-17 — RouterBench-format external eval loader  — ✅ SHIPPED

---

## 4b. Extended improvement backlog (IMP-18 → IMP-27, from RESEARCH.md)

> Source: `RESEARCH.md` Round 1–3 deep-dives (10 product areas × ~10 arXiv/GitHub sources).
> All items preserve the zero-dependency default build.

| IMP | Title | Status |
|-----|-------|--------|
| **IMP-18** | Prefix-preserving request shaping + provider prompt-cache activation | ✅ **SHIPPED (ADR-131 — `PASTURE_CACHE_CONTROL`)** |
| **IMP-19** | Reversible pseudonymization send mode (machine→cloud with PII masked) | ✅ **SHIPPED (ADR-132 — `PASTURE_PSEUDONYMIZE`)** |
| **IMP-20** | Lightweight prompt-injection guard (lexical, deterministic) | ✅ **SHIPPED (ADR-128)** |
| **IMP-21** | Latency DoS hardening (body size/connection time/output length limits) | ✅ **SHIPPED (ADR-130 — `PASTURE_MAX_BODY_BYTES` configurable)** |
| **IMP-22** | Fertility-based token estimation (whitespace=0, digits=0.5 tok/char) | ✅ **SHIPPED (ADR-126)** |
| **IMP-23** | OpenTelemetry GenAI–compliant optional metrics/trace export | ✅ **SHIPPED (ADR-133 — `PASTURE_OTEL_LOG`)** |
| **IMP-24** | Output-length prediction for cost estimation | ✅ **SHIPPED (ADR-135 — std-only heuristic; proxy-model form remains deferred)** |
| **IMP-25** | Skill-profile routing: task-type → route override table | ✅ **SHIPPED (ADR-127)** |
| **IMP-26** | Budget-aware dynamic threshold (cost velocity + daily cap) | ✅ **SHIPPED (ADR-129 — `PASTURE_BUDGET_DAILY_TOKENS`)** |
| **IMP-27** | Supply-chain hardening: cargo-deny/SBOM/cosign in CI | ✅ **SHIPPED (ADR-134 — `deny.toml` + `.github/workflows/ci.yml`)** |
- **What:** Let `eval` optionally load a larger labelled set in RouterBench
  CSV/JSONL format, while keeping the built-in 18-case set as the offline regression.
- **Why:** Validates routing quality against a public, comparable benchmark and turns
  the threshold sweep into evidence others can reproduce — answering the "brittle
  rules" critique with numbers (ADR-012).
- **Peer:** RouteLLM eval framework.
- **arXiv:** 2403.12031 (RouterBench), 2603.04445 (evaluation §).
- **Effort:** M · **Risk:** low · **Zero-dep default?** ✅ (offline file read).

---

## 4c. Extended improvement backlog (IMP-28 → IMP-35, from IMPROVEMENTS.jsonl)

> Source: a continuous Socratic feature-interaction audit run across this
> project's session history — each item started from a concrete "what if two
> shipped features compose incorrectly?" question, not a peer/arXiv survey.
> Grounding for each item is the specific ADR(s) it produced; see
> `IMPROVEMENTS.jsonl` for full change/reason/effect/risk detail per entry.
> All items preserve the zero-dependency default build.

| IMP | Title | Status |
|-----|-------|--------|
| **IMP-28** | Input-side PII category visibility in `/v1/stats` | ✅ **SHIPPED (ADR-218 — `PASTURE_INPUT_PII_SCAN`)** |
| **IMP-29** | Routing decision audit log (JSONL, PII-free) | ✅ **SHIPPED (ADR-216/230 — `PASTURE_DECISION_LOG`)** |
| **IMP-30** | Local-backend health tracking (Healthy/Degraded/Down) | ✅ **SHIPPED (ADR-215/219 — always-on, exposed via `/v1/stats`)** |
| **IMP-31** | *(reserved — not used this round; see ARCHITECTURE.md ADR-145 for the pre-existing IMP-31 array-content item)* | — |
| **IMP-32** | *(reserved — pre-existing incremental-metrics-cache item, ADR-151)* | — |
| **IMP-33** | Output-side PII category visibility (detection-only, never mutates the response) | ✅ **SHIPPED (ADR-217/220 — `PASTURE_OUTPUT_PII_SCAN`)** |
| **IMP-34** | Local-backend circuit breaker (half-open probe, acts on IMP-30's health data) | ✅ **SHIPPED (ADR-221/223/224 — `PASTURE_HEALTH_COOLDOWN_SECS`)** |
| **IMP-35** | Cloud-backend health tracking + circuit breaker, symmetric to IMP-30/34 | ✅ **SHIPPED (ADR-227 — reuses `PASTURE_HEALTH_COOLDOWN_SECS`)** |

**Notable fixes found by the same audit, filed as ADRs rather than new IMP numbers**
(each closes a real inconsistency between two already-shipped items):

| ADR | What it fixed |
|-----|----------------|
| ADR-222 | `local_health_last_error` was captured but never exposed via `/v1/stats` |
| ADR-225 | Injection guard's `block` mode had zero observability (not even stderr) |
| ADR-226 | Streaming path cached un-restored (masked) `tool_calls` when pseudonymize was active |
| ADR-228 | Skill-profile overrides silently bypassed the `has_tools` capability signal |
| ADR-229 | `with_cache_ttl` silently no-op'd on a cache created after it (builder order) |
| ADR-230 | `decision_log` recorded the pre-budget-guard route, not the route actually served |

**Methodological note:** IMP-31 and IMP-32 are intentionally left blank above —
those numbers were already claimed by pre-existing ADRs (ADR-145 and ADR-151
respectively, both predating this audit). Kept here as a visible gap rather
than silently reusing the numbers, so a future contributor grepping for
"IMP-31" or "IMP-32" finds the real pre-existing items instead of a phantom
second definition.

---

## 4d. 2026 research refresh (IMP-36 → , from external survey)

> Source: a mid-2026 web survey across three axes — (a) LLM routing/cascade/
> confidence/semantic-cache **research**, (b) LLM-gateway **product** landscape
> (LiteLLM, Portkey, OpenRouter, Cloudflare/Kong AI Gateway, vLLM Semantic
> Router), and (c) **practitioner** signal (r/LocalLLaMA, KubeCon/engineering
> blogs). Unlike §4c (an internal feature-interaction audit), these come from
> outside the codebase. **Citation caveat:** several sources were reached via
> search-result synthesis, not full-text fetch (arxiv.org/huggingface.co egress
> was blocked during the survey); arXiv IDs below should be spot-checked against
> the PDF before being treated as authoritative. All items preserve the
> zero-dependency default build.

**Shipped this round:**

| IMP | Title | Status |
|-----|-------|--------|
| **IMP-36** | Embedded zero-dependency web dashboard at `GET /dashboard` | ✅ **SHIPPED (ADR-235)** — single self-contained HTML page, polls `/v1/stats`, no build step / no external assets |
| **IMP-37** | `estimated_savings_usd` — money saved by local routing, priced at the configured cloud rate | ✅ **SHIPPED (ADR-236)** — `/v1/stats`, `/metrics`, and the dashboard |
| **IMP-39** | OTel GenAI semantic-convention metric aliases on `/metrics` | ✅ **SHIPPED (ADR-237)** — `gen_ai_client_token_usage_total{gen_ai_token_type=…}` and `gen_ai_requests_total{gen_ai_provider_name=…}`, additive alongside the existing `pasture_*` series |
| **IMP-41** | Time-sensitive cache bypass (exact-match + semantic, buffered + streaming) | ✅ **SHIPPED (ADR-238)** — narrower than the original "category-aware TTL" idea: a deterministic EN+JA marker set ("today"/"latest"/"現在"/"最新"/…) skips BOTH read and write on BOTH caches for prompts whose correct answer changes over time. Per-category TTL variance (long TTL for code-explanation, etc.) remains unshipped. |
| **IMP-40** | Multi-step task signal (sequencing-aware routing) | ✅ **SHIPPED (ADR-242)** — a new `multi_step` hard signal escalates short prompts that enumerate ≥3 sequential steps ("first…then…finally", or a numbered list) — the multi-step tasks small local models handle worse (2026 SLM data) that the reasoning markers and length threshold both miss. The riskier inverse ("keep long JSON-extraction local by weakening the `format` signal") was **deliberately not taken**: it would change established escalation behaviour; only the safe, additive escalate-direction slice shipped. |
| **IMP-38** | `POST /v1/responses` compatibility shim | ✅ **SHIPPED (ADR-241)** — translates the OpenAI Responses surface (`input`→messages, `instructions`→system, `max_output_tokens`→`max_tokens`) through the same pipeline and returns an `object:"response"`. Text-only, non-streaming; `stream:true` and `tools` are rejected with a 400 pointing at `/v1/chat/completions` rather than silently dropped. Streaming + tool use are documented follow-ups. |
| **IMP-42** | `calibrate --sweep` threshold report | ✅ **SHIPPED (ADR-239)** — `pasture calibrate --sweep` (and `--sweep --logprob`) prints the recommended `PASTURE_THRESHOLD` / `PASTURE_CASCADE_LOGPROB` at several target rates at once, turning RouteLLM's "no universal threshold, calibrate on your own traffic" into the whole curve instead of one `--target` point. Thin loop over the existing per-target math; EN/JA. |
| **IMP-43** | Checksum-validated IBAN recognizer (Presidio/PII-Shield family) | ✅ **SHIPPED (ADR-240)** — bare IBAN *values* (no keyword) are now caught by an ISO 7064 MOD-97-10 span pre-pass → classified sensitive (kept local) and masked as `<IBAN_n>` before any cloud call, run before card masking so a short all-digit IBAN isn't split into a `<CARD>`. **Note:** the rest of IMP-43's original scope was already shipped — within-request consistent placeholders (`token_for` reuse), SSE-boundary de-pseudonymization (`StreamRestorer`, ADR-205), and the Luhn card recognizer (ADR-196). Cross-request/session placeholder consistency remains out of scope (stateless proxy). |

**Found already-implemented by the survey (no work needed — recorded so the
question is not re-opened):** cache-key correctness for `tools`/`response_format`
(already hashed, ADR-177/186); credential/API-key detection in cloud-bound text
(already in `privacy.rs`: `sk-`, `ghp_`, `AKIA`, `AIza`, JWT, URL creds);
sensitive prompts already bypass the semantic cache entirely (embedding is only
computed when `!sensitive`); tool-capability routing signal (`has_tools`,
ADR-228).

**Candidate backlog (not yet built — priority order, each judged against the
zero-dep/single-user/privacy wedge):**

| IMP | Candidate | So-what / grounding |
|-----|-----------|---------------------|
| **IMP-44** | Declarative fallback chain + model-suffix routing (`model:local`, `auto:cheap`) | Every 2025-era gateway converged on declarative fallback chains and OpenRouter-style model-suffix intent. Parse suffixes off the incoming `model`; make the backend fallback list config-driven with per-hop timeout — composes with existing circuit breakers. **Assessed & deferred (2026-07):** model-suffix needs a `route_hint` field on `CompletionRequest` (25 explicit constructors → high churn) for niche single-user value; the fallback chain overlaps the shipped cloud→local + secondary-provider fallback (ADR-136). Revisit only if a concrete user need appears. |

## 4e. Strengths / weaknesses audit (2026-07, post-IMP-44 backlog)

> Source: first-hand assessment after shipping IMP-36→43 and IMP-38/40 this
> cycle (ADR-235–242). Unlike §4a–4d (feature backlogs), this is a candid
> state-of-the-codebase read to guide what a *future* contributor should tackle.

**Strengths (the moat — do not erode):**
- **Zero-dependency single binary, std-only default.** No peer gateway ships
  this. It is the whole wedge; every change is judged against invariant I1.
- **Verification culture.** 867 tests + `eval` harness + a SPEC drift-guard test
  (`test_spec_documents_every_stats_response_field`) + i18n catalog-parity test.
  Features are routinely verified end-to-end against a fake NDJSON Ollama, not
  just unit-tested.
- **Verified improvement ledger.** `IMPROVEMENTS.jsonl` (222 entries) with a
  fixed schema makes the improvement history a queryable asset (SELF_IMPROVEMENT.md).
- **Layered privacy.** keyword + checksum-validated *value* recognizers (Luhn,
  My Number, IBAN/MOD-97) + reversible pseudonymization + output scan, all
  std-only, EN+JA. Forced-local on any sensitive hit.

**Weaknesses (honest gaps a contributor could close):**
- **(W1) Agent conventions were tribal knowledge.** No `CLAUDE.md` existed, so
  each session re-derived the toolchain workaround and the IMP→ADR→ledger ritual.
  *Closed by the CLAUDE.md added alongside this section.*
- **(W2) Toolchain pin is offline-hostile.** `rust-toolchain.toml` pins 1.75.0,
  which cannot be fetched in a sandbox; every build here uses `rustup run stable`.
- **(W3) `/v1/responses` is text-only.** Streaming (`response.output_text.delta`
  SSE) and tool forwarding are rejected with a 400 (ADR-241).
- **(W4) No history store.** The dashboard and `/v1/stats` are point-in-time;
  there is no time-series, so no trend/sparkline is possible without one.
- **(W5) `proxy.rs` is ~4k lines.** The single-file HTTP/routing/dispatch module
  is large enough that finding call sites is slow; a module split would help.
- **(W6) No live-model quality eval.** Tests use MockBackend / fake Ollama; there
  is no harness measuring real local-vs-cloud answer quality on a task set.

**Improvement candidates (IMP-45→, priority order, each keeps the wedge):**

| IMP | Candidate | So-what / grounding |
|-----|-----------|---------------------|
| **IMP-45** | Streaming `POST /v1/responses` (SSE Responses events) | Closes W3; completes the ADR-241 shim. Emit `response.created` / `response.output_text.delta` / `response.completed`. Invasive (dual SSE protocol in `stream_chat_to_socket`) → Opus-scale. |
| **IMP-46** | Semantic-cache lexical second-gate | ✅ **SHIPPED (ADR-243)** — opt-in `PASTURE_SEMANTIC_MIN_LEXICAL` (default 0 = off): a cosine hit must also clear a Jaccard token-set overlap floor, rejecting embedding false-positives that would serve a wrong cached answer while preserving genuine paraphrase hits. std-only, in `cache.rs`; each entry stores a sorted token-hash fingerprint. |
| **IMP-47** | Confidence-signal AUROC self-test | ✅ **SHIPPED (ADR-246)** — `calibrate --auroc --labels <f>` measures whether the cascade's confidence signal separates correct from incorrect answers *on this machine's model*, and says plainly when it does not. Grounded in the published per-model AUROC spread (~0.58 near-chance → ~0.84 useful) and the known collapse of verbalized confidence onto saturated values (average-rank ties ⇒ a constant signal scores exactly 0.5, not 1.0). **First mechanism in Pasture that validates a routing signal instead of just consuming it** — a partial answer to F4. The *verbalized-confidence extraction* half remains unshipped: the self-test is the load-bearing part and works on any (score, correct) pairs. |
| **IMP-48** | Lightweight daily-counter history for the dashboard | ✅ **SHIPPED (ADR-245)** — closes W4. Simpler than the original sketch: **no new history file**. `cost::daily_summaries` rolls the existing PII-free cost log up per UTC day, `GET /v1/history` serves the last 30 days (routes/tokens/spend/savings), and the dashboard draws a stacked bar per day in pure flexbox. One source of truth ⇒ nothing to drift. |
| **IMP-49** | Split `proxy.rs` into dispatch / handlers / response-builders | Closes W5: mechanical module extraction, no behaviour change — ideal Sonnet task with a large test net. |
| **IMP-50** | Anthropic prompt-cache params + cache-token pricing (cloud feature) | Research (Anthropic 5-min-TTL change): inject `cache_control`, price `cache_creation`/`cache_read` tokens correctly in the budget guard. Behind the `cloud` flag. |

### 4e-3. Security pass: prompt-injection surface (2026-08)

> Prompted by 2026 reporting placing prompt injection as OWASP's #1 AI threat,
> the first documented large-scale *indirect* injections in the wild, and the
> recurring finding that gateways inspecting only prompts/completions are blind
> to the tool-call layer where the exploit actually lives. Two real defects
> found in Pasture's own guard, both fixed.

| Finding | Verdict |
|---|---|
| **S1. Guard skipped tool-call arguments** | ✅ **FIXED (ADR-247)** — ADR-187 had already extended the *privacy* scan to `tool_calls_json` ("PII can live solely there"); the injection guard never got the same treatment and scanned content-only `routing_text()`. Since ADR-183 round-trips prior `tool_calls` every turn, a payload could ride in the arguments untouched — exactly the indirect-injection path. `guard_text()` now covers the same surface as `privacy_text()`. |
| **S2. Literal patterns defeated by one inserted word** | ✅ **FIXED (ADR-248)** — probing found **1 of 6** canonical phrasings detected: `"ignore previous instructions"` matched, but `"ignore all previous instructions"` — the most recognisable form of the attack — did not. Replaced with structural verb→scope→target matching: recall 1/6 → **9/9**, false positives **0/7**. |
| **S3. Lexical guards remain a first layer only** | ⏸️ **Unchanged, documented.** Homoglyphs, base64/encoding tricks, and multi-turn staged attacks are still out of reach for a std-only lexical guard. `guard.rs` says so in its header; do not oversell it. |

### 4e-2. First-principles pass on *excess* capability (2026-07)

> §4e above asks "what is missing?". This asks the inverse, from first
> principles: Pasture's irreducible job is **(1)** accept an OpenAI-compatible
> request, **(2)** route it deterministically and privately, **(3)** execute,
> **(4)** account. What exists that serves none of those — and is it harmless?

| Finding | Verdict |
|---|---|
| **F1. `/v1/moderations` claimed unverified safety** | ✅ **FIXED (ADR-244)** — it returned `flagged:false` + all-zero scores for text it never examined, while advertising OpenAI's real `text-moderation-stable`. A client gating display on that got a guarantee Pasture cannot back, contradicting the codebase's own refuse-don't-fake precedents (multimodal rejection; ADR-241 tools rejection). Now self-identifies via `model:"pasture-no-moderation"` + `x_pasture_moderated:false`, with `results[0]` unchanged so SDKs still parse. |
| **F2. Monetization / referral surfaces** | ⏸️ **Kept, recorded.** Serve none of (1)–(4), but are an explicit product decision (ADR-006, §6) and cause no incorrect behaviour. Not scope for a correctness pass. |
| **F3. Backlog doc sprawl (§4, 4b, 4c, 4d, 4e)** | ⏸️ **Kept, recorded.** Five append-only backlog sections is navigation friction, not a defect; consolidating would rewrite provenance a future contributor may need. |
| **F4. Routing decisions are never validated** | ◐ **Partially addressed (ADR-246)** — still the deepest gap (= W6): Pasture does not observe correctness on its own. But `calibrate --auroc` now lets an operator who *does* label some answers find out whether the cascade signal they are gating on has any predictive power at all, and refuses to endorse one that does not. Closing F4 fully still needs a correctness oracle. |

## 5. Anti-goals (deliberately *not* adopting)

Keeping the wedge means saying no to several common peer features:

- **Mandatory vector-DB / Redis / external store.** Would break zero-dep and
  single-binary. Semantic cache (IMP-12) stays in-process and opt-in.
- **Multi-tenant infra / org accounts / billing.** Pasture is single-user; this is
  the gateways' job, not Pasture's.
- **GPU-trained learned routers.** RouteLLM-style preference-trained routers need a
  dataset and a training stack. Pasture stays label-light and deterministic-by-default
  (IMP-13/14 add calibration/embedding signals *without* training a model).
- **Logging prompt content (even for analytics).** The cost log is PII-free by
  design (I5); nothing in this backlog records prompt text or matched PII values.
- **Becoming a 100-provider marketplace.** Pasture routes *local↔cloud*; broad
  provider catalogs are OpenRouter's domain. IMP-9's fallback chain stays small.

---

## 6. Monetization surfaces (referenced by ADR-006)

The proxy layer is the natural place to surface **cloud-provider referral links**
(the operator's own affiliate URLs from config) and a donation link — never handling
cards or secrets itself (hosted Stripe Checkout via `worker/`). No user PII is
collected. This is surface-only and orthogonal to the routing IP above.

---

## 7. Summary

Pasture already does the hard, differentiated things its peers skip. The shortest
path to "feels complete next to LiteLLM/RouteLLM" is **Tier 1** (IMP-8/9/10/11) —
small, std-only, philosophy-preserving parity fixes. **Tier 2** (IMP-12/13/14) is
where the recent literature pays off, all achievable opt-in without disturbing the
zero-dependency default. **Tier 3** (IMP-15/16/17) unlocks safe exposure and
reproducible evaluation. Every item above is gated so the default artifact stays a
single, zero-dependency, privacy-first binary.

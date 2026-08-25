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
| **IMP-50** | Anthropic prompt-cache params + cache-token pricing (cloud feature) | ◐ **PARTIALLY SHIPPED (ADR-254)** — injecting `cache_control` was already done (IMP-18), but the *accounting* half was not, and was **proven broken**: cached prompt tokens land in `cache_creation_input_tokens`/`cache_read_input_tokens` and were dropped entirely, undercounting a realistic cached request **1025×** and silently defeating `PASTURE_BUDGET_DAILY_TOKENS`. Both the buffered and streaming paths now sum all prompt-side fields. **Still open:** cached tokens are counted at face value, so their differing *prices* (cache writes cost more than base input, reads far less) are not modelled by the flat per-1M rate — that needs a richer price config. |

### 4e-7. Onboarding audit: the differentiator was Linux-only (2026-08)

> A read-only first-run audit asked what a *new user* hits. Most findings were
> bad messages in front of correct behaviour. One was the inverse.

| Finding | Verdict |
|---|---|
| **A7. Hardware detection was Linux-only and failed OPEN into "weakest machine"** | ✅ **FIXED (ADR-261)** — `detect_ram_mb` read only `/proc/meminfo`, and `detect()` collapsed failure to `ram_mb: 0` via `.unwrap_or(0)`. That 0 fell into the 300 (CPU-only) tier, so an M3 Max / 64 GB Windows box **silently escalated nearly everything to the paid cloud** — the exact inverse of *"GPU PC → keep more work local"*, on the product's only real differentiator. RAM is now detected on macOS (`sysctl`) and Windows (`wmic`), `ram_mb` is `Option<u64>` so "unknown" is representable, unknown leans **local** (800), and `hw`/`models`/`doctor` say so instead of printing `0 MB`. |
| **A1/A2. doctor guessed, and skipped a check** | ✅ **FIXED (ADR-265)** — one TCP probe emitted a single message carrying *both* the install and the start fix, so the user guessed; and the model check sat behind `if st.reachable`, so a bare machine reported **1** problem when it had **2** and only learned about the missing model on a second run. `on_path` now selects the right fix, and the model step always reports. Verified: bare machine now reads "2 item(s) need attention". |
| **A3. Accounting failed silently, and `stats` mislabelled it** | ✅ **FIXED (ADR-265)** — `read_log` maps NotFound to "no records" and a missing *directory* is also NotFound, so an unwritable cost log was indistinguishable from "no data yet": `stats` said "run some requests first" and would say it forever. Doctor now checks writability as a real step; `stats` reports the cause and exits 1. |
| **A4. Bad `PASTURE_*` settings vanished silently** | ✅ **FIXED (ADR-266)** — `with_env` parses with `if let Ok(..)` and no `else`, so malformed values were dropped in silence, and a misspelled name matched nothing at all. The audit's reproduction (bad port, bad threshold, `PASTURE_LOCAL_BAKEND`) produced **zero** warnings; all three are now reported on stderr. A completeness test ties the known-name list to the source scanner so it cannot drift. |
| **A6. `up`/`serve` claimed success, then died on a busy port** | ✅ **FIXED (ADR-262)** — the connect banner printed unconditionally and `serve` then failed with a raw `os error 98`; the actionable fix string already existed and only `doctor` reached it. `run_serve` now pre-checks via `doctor::port_available` and prints it, so `up` inherits the check too. |
| **A9. `up`'s Ollama auto-start swallowed "not installed"** | ✅ **FIXED (ADR-267)** — `let _ = Command::new("ollama").spawn()` discarded the error, so a machine with no `ollama` on PATH spawned nothing, slept **3 s** polling a port nobody was listening on, then printed the same message a wedged-but-installed Ollama gets. Two different problems, one fix string. `up` now pre-checks `doctor::on_path` and fails in **2 ms** naming the real cause; a spawn error is reported with its OS message instead of discarded; the "did not come up" text now says *start it*, not *install it*. Verified E2E on all three branches (absent / present-but-not-serving / non-executable) in EN and JA. |
| **A8. Dangling `pasture refer` reference** | ✅ **FIXED (ADR-262)** — residue from this session's own ADR-258 deletion; the binary advertised a command that answers `unknown command`. |
| **A12. Two dead CI dirs + two docs for one excuse** | ✅ **FIXED (ADR-263)** — self-inflicted: `ci/ci.yml` predated this session (ADR-134 hit the same permission wall), and my ADR-259 added a second without noticing. Consolidated into one workflow, keeping the older file's unique `cargo deny` supply-chain job. **CI is still inactive** — it needs the documented one-command `git mv` from someone whose token has `workflows` permission, so the badge still 404s until then. |
| **A5. Config-file layer was test-only** | ✅ **FIXED (ADR-264)** — SPEC §8 promised *defaults → file → env* and the parser had existed since ADR-190, but nothing ever read a file: zero non-test callers. Questioned rather than deleted — 53 env vars make a persistent file genuinely useful, and wiring it up cost ~15 lines vs deleting ~150 tested ones. `PASTURE_CONFIG` (else `~/.config/pasture/config`) is now read before env, so the document is true. |

| **A11. Nothing pinned any of it** | ✅ **FIXED (ADR-268)** — every fix above was verified once, by hand, against a release build; `run_doctor`/`run_up`/`run_stats`/`run_models` print and spawn, so no unit test reached them and all seven were regressions waiting to happen. `tests/cli_onboarding.rs` runs the real binary in a **cleared environment** (empty `PATH`, throwaway `HOME`, no inherited `PASTURE_*`, Ollama port pointed at a closed one) and asserts on exit code and output. Each of the 7 tests was **mutation-checked**: the corresponding fix was reverted in turn and the intended test — and only it — failed. |

### 4e-6. Closing the loop on routing validation (2026-08)

> F4/W6 — "routing decisions are never validated" — keeps blocking other work
> (it is why ADR-256 shipped opt-in). This tick attacked the workflow gap rather
> than the (unaffordable) quality-oracle gap.

| Finding | Verdict |
|---|---|
| **V1. The AUROC self-test had no way to get labels** | ✅ **FIXED (ADR-257)** — ADR-246 shipped `calibrate --auroc --labels <f>` and ADR-124 `--error --labels <f>`, both consuming `{"logprob","correct"}` JSONL that **nothing in Pasture produced**; the user had to hand-write it. Worse, it could not be reconstructed afterwards: the cost log keeps `logprob` but deliberately no prompt/answer text (I3), so there is nothing to review. `pasture label` captures verdicts at request time and writes only score+verdict, preserving I3. |
| **V2. The default backend cannot score confidence** | ⏸️ **Surfaced, not hidden.** `complete_scored` returns no logprob on the trait default, so **Ollama — the default local backend — yields none**, making `--auroc`/`--error` unusable there. `label` now detects this, writes zero rows rather than junk, and names the fix (point `PASTURE_LOCAL_BACKEND` at LM Studio / llama.cpp / vLLM). A real limitation of the cascade-confidence feature line, now documented instead of failing mysteriously. |

### 4e-5. Task-shape routing revisited (2026-08)

> 2026 SLM reporting draws a sharper line than Pasture's `format` signal does:
> *"classification, routing, structured extraction, reformatting and
> short-context QA almost always work on small models; multi-step reasoning,
> long-context synthesis and open-ended writing usually need a bigger one."*

| Finding | Verdict |
|---|---|
| **T1. `format` conflated reformatting with code generation** | ◐ **ADDRESSED, opt-in (ADR-256)** — one marker list held both `as json` / `csv format` / `markdown table` (reformatting, which small models handle reliably) and `write a function` / `dockerfile` / `sql query` (synthesis, which they don't). A bare "give me that as JSON" was escalated to cloud — Pasture spending money on the class it exists to keep local. The lists are now split, and `PASTURE_STRUCTURED_LOCAL=1` stops the structured half escalating. **Deliberately opt-in, not the default:** the evidence is external benchmark reporting and Pasture has no live-model quality harness (W6) to confirm the trade-off locally, so it does not silently re-route existing deployments. Flipping the default should wait on W6. |

### 4e-4. Peer-software compatibility pass (2026-08)

> Angle: what do the tools Pasture actually sits between — Ollama locally,
> Anthropic/OpenAI upstream — now do that Pasture does not keep up with?
> Both findings are **accounting** bugs: the functional paths were correct, but
> the numbers Pasture reports about them were not.

| Finding | Verdict |
|---|---|
| **P1. Anthropic cached prompt tokens dropped** | ✅ **FIXED (ADR-254)** — with `PASTURE_CACHE_CONTROL=1` (Pasture's own IMP-18 feature) Anthropic bills most of the prompt under `cache_creation_input_tokens`/`cache_read_input_tokens`; Pasture read `input_tokens` alone. Proven **1025×** undercount, silently defeating the token-denominated `PASTURE_BUDGET_DAILY_TOKENS`. Enabling the cost-*saving* feature disabled the cost-*control* feature. |
| **P2. Ollama's real token counts ignored** | ✅ **FIXED (ADR-255)** — Ollama returns `prompt_eval_count`/`eval_count` on every response and Pasture used neither, always estimating from text length. Harmless-ish for plain models; **~250× wrong for a thinking model**, whose reasoning goes to `message.thinking` and never appears in `content`. Now uses the reported counts, estimating only as fallback. |
| **P3. Thinking-model content handling** | ✅ **Verified correct, no change needed.** Tested against a fake Ollama emitting `message.thinking`: the reasoning trace is correctly excluded from the answer on *both* the buffered and streaming paths. Recorded so the question is not re-opened. |

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
| **S3. Invisible-character / homoglyph evasion** | ✅ **FIXED (ADR-249)** — probing found **5 of 8** obfuscations bypassed the guard: one zero-width space inside a keyword (or a soft hyphen, full-width letters, Cyrillic lookalikes) reduced it to `Allow` for text the model reads normally. Research reports this class defeating commercial guardrails outright, with "normalize before filtering" as the stated mitigation. `normalize_for_guard()` now strips invisible/bidi/tag/variation-selector characters and folds full-width + homoglyphs: **11/11** obfuscated variants caught, benign and CJK unaffected. |
| **S4. Encoded payloads** | ✅ **MOSTLY FIXED (ADR-252)** — the literature's prescribed defence is *decode-and-rescreen*: canonicalise/decode before filtering and judge intent on the decoded text. base64 (both alphabets, padded or not), hex, and ROT13 runs are now decoded and re-screened with the existing matchers under a new `encoded_payload` label. Probe: **5/5** encoded attacks caught, **6/6** benign encodings (prose, JSON, SHA-256 hex, UUID, tokens) still Allow — a flag requires the *decoded* text to match, so looking encoded is never enough and the false-positive rate cannot rise. Work is bounded (256 KiB budget / 64 KiB per run). |
| **S7. Nested / compositional encodings** | ✅ **FIXED (ADR-253)** — the one-level limit named when ADR-252 shipped was **exploitable and proven so**: `base64(base64(x))`, `base64(base64(base64(x)))`, `hex(base64(x))` and `base64(rot13(x))` all returned `Allow`. Decoding is now iterative (breadth-first, each decoded string fed back through the extractors), bounded on three axes — depth (3), node count (64) and bytes (256 KiB). Probe: 5/5 nested/mixed variants caught, benign nested content still Allow, a 292 KiB six-level decode bomb terminates in ~8 ms. |
| **S8. Remaining evasions** | ⏸️ **Open, named honestly.** Leetspeak and in-word substitutions; encodings with no decoder here (Morse, base32, fictional ciphers the model learned but this scanner has not); layerings deeper than `MAX_DECODE_DEPTH`; mixed-language payloads; multi-turn staged attacks; novel phrasing. `guard.rs`'s "Known limitations" enumerates exactly this. |
| **S5. Privacy detection had the same blind spot** | ✅ **FIXED (ADR-250)** — confirmed and worse than predicted: a card number with a soft hyphen classified as `[]` (undetected → escapes to cloud) and with a zero-width space as `['my_number']` — actively **wrong**, the invisible char split the digit run into a 12-digit chunk that passed the My Number checksum. Email and IBAN likewise. **Not primarily adversarial**: PDFs insert soft hyphens at line breaks, so pasting your own card number could defeat **I2** with no attacker. Fixed by stripping invisibles in `normalize_for_detection` *and* running every detector on normalized text (only the numeric ones did before). |
| **S6. Pseudonymizer spans read raw text** | ✅ **FIXED (ADR-251)** — `spans_via_normalized()` runs each `*_spans` detector over the normalized form and maps results back to original byte offsets (with a no-op fast path). Obfuscated cards/IBANs are now masked and round-trip byte-for-byte. Also fixed a silent regression of ADR-213/214: **full-width** card numbers were classified sensitive but never masked, same root cause. |

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

## 6. Monetization surfaces — REMOVED (ADR-258, supersedes ADR-006)

Deleted in the first-principles pass: `monetize.rs`, the `donate` / `refer`
commands, the periodic donation nudge, the `worker/` Stripe Cloudflare Worker,
and their config knobs (`PASTURE_DONATE_URL`, `PASTURE_NO_NUDGE`,
`PASTURE_STATE`).

Three reasons, in order of weight:

1. **Misaligned incentive.** Referral revenue is earned when a user signs up
   for a *cloud* provider. Pasture's entire job is to send *less* work to the
   cloud. The monetization paid out precisely when the product failed at its
   purpose — an incentive pointing the wrong way, in a tool asking to be trusted
   with the user's prompts.
2. **Not a requirement from anyone.** It served none of the four irreducible
   jobs (accept, route, execute, account). It was a speculative revenue plan for
   a product with no users yet.
3. **It cost real complexity.** The nudge existed only to interrupt the user
   asking for money, and was the *sole* reason Pasture wrote a state file to
   disk. The Worker put 137 lines of JavaScript and a Stripe dependency inside a
   "zero-dependency, single-binary, std-only" project.

Reversible if wanted — it is one `git revert` away — but it should come back, if
at all, as something that does not pay more when the user routes to the cloud.

---

## 7. Summary

Pasture already does the hard, differentiated things its peers skip. The shortest
path to "feels complete next to LiteLLM/RouteLLM" is **Tier 1** (IMP-8/9/10/11) —
small, std-only, philosophy-preserving parity fixes. **Tier 2** (IMP-12/13/14) is
where the recent literature pays off, all achievable opt-in without disturbing the
zero-dependency default. **Tier 3** (IMP-15/16/17) unlocks safe exposure and
reproducible evaluation. Every item above is gated so the default artifact stays a
single, zero-dependency, privacy-first binary.

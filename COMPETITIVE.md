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

#### IMP-9 — Cloud transient-error retry + provider fallback chain  — retry+fallback ✅ SHIPPED
- **What:** **Implemented** (ADR-032). On a cloud 5xx/timeout/connection error, retry
  with bounded exponential backoff (`PASTURE_CLOUD_RETRY`, default 2), then fall back
  to the local backend when available. 5xx is now classified retryable; 4xx is not.
  The **multi-provider** fallback chain (ordered cloud providers) remains the follow-up.
- **Why:** Today a single transient cloud hiccup silently degrades a cloud-routed
  request to the weaker local model (`cascade`/`backend` paths), which is invisible
  to the user and hurts quality. Robustness, not new routing.
- **Peer:** LiteLLM / Portkey failover & retries.
- **arXiv:** — (reliability).
- **Effort:** S–M · **Risk:** low · **Zero-dep default?** ✅ (std-only timers/loops).

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
- **What:** Let `eval` optionally load a larger labelled set in RouterBench
  CSV/JSONL format, while keeping the built-in 18-case set as the offline regression.
- **Why:** Validates routing quality against a public, comparable benchmark and turns
  the threshold sweep into evidence others can reproduce — answering the "brittle
  rules" critique with numbers (ADR-012).
- **Peer:** RouteLLM eval framework.
- **arXiv:** 2403.12031 (RouterBench), 2603.04445 (evaluation §).
- **Effort:** M · **Risk:** low · **Zero-dep default?** ✅ (offline file read).

---

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

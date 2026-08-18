# Pasture

> New to this? See **[GETTING_STARTED.md](GETTING_STARTED.md)** for a zero-to-running beginner guide (Japanese). Or just run `pasture doctor`.

[![CI](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml/badge.svg)](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml)

**OpenAI-compatible local/cloud LLM routing proxy with hardware-adaptive thresholds. Zero dependencies.**

Pasture sits between your existing AI client (IDE plugin, chat app, Claude Code, your own scripts) and the models. It exposes an OpenAI-compatible endpoint, and for each request it decides — deterministically and based on your actual hardware — whether to answer with a **local** model (free, private, offline-capable) or escalate to a **cloud** API (for hard tasks). You point your client's `base_url` at Pasture and keep your existing workflow.

It is built as a single Rust binary using only the standard library. No async runtime, no web framework, no serialization crate.

## Why

Most desktop AI clients make *you* pick the model. Routing layers that automate the choice (RouteLLM, LiteLLM) are developer infrastructure that needs code and servers. Pasture aims for the gap: **zero-config, single-binary routing for individuals**, that adapts to whether your PC has a GPU or not.

- **GPU-less PC** → small local model, low threshold, escalate to cloud sooner.
- **GPU PC** → larger local model, high threshold, keep more work local.

The same prompt that goes to the cloud on a laptop stays local on a workstation — automatically.

## Install

Requires Rust 1.75+ to build.

```sh
git clone https://github.com/shizukutanaka/pasture
cd pasture
cargo build --release                 # zero-dependency, local-only
cargo build --release --features cloud # adds OpenAI/Anthropic over HTTPS
# binary at target/release/pasture
```

Cloud is opt-in. With `--features cloud`, set a BYOK key:
`PASTURE_OPENAI_API_KEY` or `PASTURE_ANTHROPIC_API_KEY` (and `PASTURE_CLOUD_PROVIDER`, `PASTURE_CLOUD_MODEL`). Keys are read from the environment and never logged.

## Usage

```sh
pasture hw                 # show detected hardware (RAM / CPU / GPU)
pasture route "your text"  # dry-run: where would this route, and why
pasture chat  "your text"  # one-shot through the router (needs Ollama running)
pasture serve              # start the OpenAI-compatible proxy
pasture up                 # one command: ensure model, then start the proxy
pasture connect cursor     # exact setup to point an app at the proxy
pasture connect lmstudio   # use LM Studio (or any OpenAI server) as the engine
pasture calibrate          # recommend PASTURE_THRESHOLD from your own usage
pasture label --prompts p.txt   # answer prompts locally and mark each right/wrong -> labels.jsonl
pasture calibrate --auroc --labels labels.jsonl  # self-test: does the confidence signal predict correctness?
pasture models             # recommended local models for your machine
pasture doctor             # check your setup and how to fix problems
pasture eval               # measure routing accuracy + threshold sweep
pasture stats              # summarize the cost log (routes, tokens, spend)
pasture improvements       # show the self-improvement ledger (verified change history)
```

Force a route with `--local` or `--cloud`. Enable cascade (answer locally, escalate to cloud only when the local answer is weak) with `PASTURE_CASCADE=1` (requires the `cloud` feature and a key). Enable an exact-match response cache with `PASTURE_CACHE=<size>` to avoid paying for repeated identical prompts, and a semantic cache with `PASTURE_SEMANTIC_CACHE=<size>` to also serve paraphrased repeats (cosine similarity via the local backend's embeddings; threshold `PASTURE_SEMANTIC_THRESHOLD`, default 0.92). List prompts your local model handles badly in a file and set `PASTURE_HARD_PROMPTS=<file>` to escalate anything embedding-similar to them (threshold `PASTURE_HARD_THRESHOLD`, default 0.85). Pin task types to backends with `PASTURE_SKILLS=code:local,summarize:cloud` (comma-separated `skill:route`; recognised: `code`, `math`, `reason`, `summarize`, `translate`). Enable prompt-injection detection with `PASTURE_INJECTION_GUARD=flag` (annotate) or `=block` (reject 400). Cap daily cloud token spend with `PASTURE_BUDGET_DAILY_TOKENS=<n>` (action via `PASTURE_BUDGET_ACTION`: `local-only`/`warn`/`block`); spike detection redirects outlier requests with `PASTURE_SPIKE_FACTOR=<n>` (default 50). Mask PII in cloud-bound requests with `PASTURE_PSEUDONYMIZE=1` (reversible; tokens swapped back in the response). Enable Anthropic prefix caching with `PASTURE_CACHE_CONTROL=1`. Write OTel GenAI trace records with `PASTURE_OTEL_LOG=<path>`. Configure a secondary cloud provider for failover with `PASTURE_CLOUD_FALLBACK_PROVIDER=anthropic` (tried after the primary exhausts retries, before falling back to local). Set the listen address for the proxy with `--addr host:port`.

Point any OpenAI-compatible client at the proxy:

```
base_url = http://127.0.0.1:8645/v1
```

### Endpoints

- `POST /v1/chat/completions` — routed chat completion. Supports `"stream": true` (SSE),
  including `stream_options.include_usage` for a final token-usage chunk.
  Requests carrying `tools`/`functions` are treated as a hard signal and escalate to cloud.
  Sampling parameters (`temperature`, `top_p`, `max_tokens`, `stop`, `seed`, penalties)
  and `response_format` (JSON mode / structured outputs) are forwarded to the backend.
- `POST /v1/embeddings` — pass-through to the local backend (Ollama or OpenAI-compat);
  returns the standard OpenAI embeddings shape. Local-only (no cloud escalation).
- `POST /v1/responses` — OpenAI Responses API shim (IMP-38): translates `input`→messages and routes through the same pipeline, returning an `object:"response"` reply. Text-only, non-streaming (tools/streaming rejected with a pointer to `/v1/chat/completions`).
- `POST /v1/moderations` — compatibility stub only; **Pasture performs no moderation**. It exists so SDKs that call this endpoint unconditionally don't 404. The reply sets `x_pasture_moderated: false` and `model: "pasture-no-moderation"` — `flagged:false` means "not checked", never "checked and safe", so do not gate anything on it.
- `GET /v1/models` — list the configured local (and cloud) model ids, OpenAI-compatible.
- `GET /v1/models/{id}` — retrieve one configured model (or 404), OpenAI-compatible.
- `GET /v1/stats` — live JSON snapshot of the cost-log counters (routes, rates, tokens, spend).
- `GET /v1/history` — per-day rollups (last 30 days) of routes, tokens, spend, and estimated savings, so you can see the trend rather than just the current moment.
- `GET /health` — liveness check.
- `GET /dashboard` (or `GET /`) — embedded web dashboard: a single self-contained page (no build step, no external assets) that polls `/v1/stats` and shows the local/cloud split, spend vs. budget, cache hit rates, and backend health. Open `http://127.0.0.1:8645/dashboard` in a browser.

The response includes an `x_pasture_route` field telling you which way the request went.
Errors use the OpenAI envelope `{"error":{"message","type"}}`. The full API/routing
contract is specified in **[SPEC.md](SPEC.md)**.

## Configuration

Defaults are sensible; override via environment variables:

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_LISTEN_ADDR` | `127.0.0.1:8645` | proxy listen address |
| `PASTURE_OLLAMA_HOST` | `127.0.0.1` | local Ollama host |
| `PASTURE_OLLAMA_PORT` | `11434` | local Ollama port |
| `PASTURE_LOCAL_MODEL` | `llama3.2` | local model name |
| `PASTURE_COST_LOG` | `pasture-cost.jsonl` | cost log path |
| `PASTURE_CLOUD_RETRY` | `2` | retry transient cloud failures (5xx/timeouts) this many times, then fall back to local |
| `PASTURE_LOCAL_ONLY` | _(off)_ | when set, all traffic routes local; cloud backend disabled (GPU-less / air-gapped use) |
| `PASTURE_LOCAL_FAST_MODEL` | _(off)_ | second local model for simple short queries; e.g. `phi3:mini` for greetings, `llama3.2` for harder |
| `PASTURE_FAST_THRESHOLD` | `50` | estimated-token threshold below which the fast model is selected |
| `PASTURE_INJECT_CONTEXT` | _(off)_ | when set, prepends a system message with the current date and OS — boosts lightweight models as PC assistants |
| `PASTURE_AUTH_TOKEN` | _(off)_ | require `Authorization: Bearer <token>` on `/v1/*` (for exposed, non-localhost deployments); `/health` stays open |
| `PASTURE_RATE_LIMIT` | `0` | cap `/v1/*` to this many requests per minute (global; 0 = unlimited) |
| `PASTURE_REQUEST_TIMEOUT` | `30` | per-connection read/write timeout in seconds (slow-loris guard; 0 = none) |
| `PASTURE_CORS_ORIGINS` | _(off)_ | CORS allow-list for browser clients: comma-separated origins, or `*` (off by default — a localhost server with `*` is reachable by any website) |
| `PASTURE_SKILLS` | _(off)_ | skill-profile routing: `code:local,summarize:cloud` pins detected task types (`code`, `math`, `reason`, `summarize`, `translate`) to a specific backend; checked before generic hard signals |
| `PASTURE_INJECTION_GUARD` | `off` | prompt-injection guard: `flag` detects role-switch/exfiltration patterns and annotates the response; `block` rejects with 400; `off` (default) has zero overhead |
| `PASTURE_BUDGET_DAILY_TOKENS` | `0` | daily cloud token budget (prompt + completion combined, UTC day); 0 = disabled |
| `PASTURE_BUDGET_ACTION` | `local-only` | action when daily budget is exceeded: `local-only` (silently redirect to local), `warn` (log + proceed), `block` (return 429) |
| `PASTURE_SPIKE_FACTOR` | `50` | redirect a single request to local when it estimates more than N × the running average tokens (0 = disabled) |
| `PASTURE_CLOUD_PRICE_PER_1M` | _(off)_ | cloud price in USD per 1M tokens as `<input>,<output>` (e.g. `2.50,10.00`); makes `cloud_cost_usd` / `pasture_cloud_cost_usd_total` report real dollars instead of 0 |
| `PASTURE_MAX_BODY_BYTES` | `16777216` | maximum request body size in bytes; bodies larger than this are rejected with 413 |
| `PASTURE_CACHE_CONTROL` | _(off)_ | set to `1` to add `cache_control` hints on Anthropic system messages (prompt prefix caching); no-op for OpenAI |
| `PASTURE_PSEUDONYMIZE` | _(off)_ | set to `1` to replace detected PII (email, IPv4, phone, API-key prefix) with stable opaque tokens in cloud-bound requests, then restore in the response; applies to both buffered and streaming responses; mapping never logged |
| `PASTURE_OTEL_LOG` | _(off)_ | path to a JSONL file for OpenTelemetry GenAI semantic convention trace records; appended per request; off when unset |
| `PASTURE_CLOUD_FALLBACK_PROVIDER` | _(off)_ | secondary cloud provider tried when the primary fails all retries (`openai` or `anthropic`); off when unset |
| `PASTURE_CLOUD_FALLBACK_MODEL` | _(same as primary)_ | model to use on the fallback provider; defaults to `PASTURE_CLOUD_MODEL` when empty |

The local backend talks to [Ollama](https://ollama.com) over plain HTTP. GPU acceleration (CUDA/Metal/ROCm) is handled by Ollama; Pasture writes no GPU code itself.

## Status

Pasture is at v0.28.0. Working today: hardware detection, deterministic hardware-adaptive routing, the OpenAI-compatible proxy, the local Ollama backend, and a fully implemented multi-provider cloud backend (OpenAI/Anthropic, streaming, tool calls, retry + fallback-provider failover, per-backend health tracking with a circuit breaker on both the local and cloud side). Also shipped: exact-match and semantic response caching, a FrugalGPT-style local-first cascade, reversible pseudonymization for cloud-bound PII, a lightweight prompt-injection guard, budget/spike-aware routing, structured JSONL cost + routing-decision audit logs, optional OpenTelemetry GenAI tracing. See `ARCHITECTURE.md` for the full ADR history and `SPEC.md` for the complete configuration/API reference.

## Privacy

Pasture records no personal data. The cost log contains only timestamps, route, model name, token counts, and cost. Local requests never leave your machine. Prompts that look sensitive (emails, keys, card numbers, etc.) are detected and kept local automatically; only category labels are ever logged, never the values.

## How Pasture compares

See **[COMPETITIVE.md](COMPETITIVE.md)** for how Pasture compares to peer tools
(LiteLLM, RouteLLM, vLLM Semantic Router, GPTCache, Portkey, OpenRouter), the
recent arXiv work behind its routing/cascade/cache design, and the prioritized
improvement backlog.

The verified change history is tracked as a machine-readable asset in
**[IMPROVEMENTS.jsonl](IMPROVEMENTS.jsonl)** (`pasture improvements`).
**[SELF_IMPROVEMENT.md](SELF_IMPROVEMENT.md)** explains the approach — why the
durable asset is a verified improvement record, not the model — and what is
deliberately out of scope.

## License

MIT. See `LICENSE`.

## Language

UI is available in English and Japanese. Select with `PASTURE_LANG=ja` or `PASTURE_LANG=en` (auto-detected from your locale otherwise).

## Local backends

Default is Ollama. To use LM Studio (or any OpenAI-compatible local server such as llama.cpp/vLLM): start its server, then run with `PASTURE_LOCAL_BACKEND=lmstudio` (endpoint via `PASTURE_LOCAL_OPENAI_URL`, default `http://127.0.0.1:1234/v1`).

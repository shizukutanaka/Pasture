# Pasture

> New to this? See **[GETTING_STARTED.md](GETTING_STARTED.md)** for a zero-to-running beginner guide (Japanese). Or just run `pasture doctor`.

**OpenAI-compatible local/cloud LLM routing proxy with hardware-adaptive thresholds. Zero dependencies.**

Pasture sits between your existing AI client (IDE plugin, chat app, Claude Code, your own scripts) and the models. It exposes an OpenAI-compatible endpoint, and for each request it decides — deterministically and based on your actual hardware — whether to answer with a **local** model (free, private, offline-capable) or escalate to a **cloud** API (for hard tasks). You point your client's `base_url` at Pasture and keep your existing workflow.

It is built as a single Rust binary using only the standard library. No async runtime, no web framework, no serialization crate.

## Why

Most desktop AI clients make *you* pick the model. Routing layers that automate the choice (RouteLLM, LiteLLM) are developer infrastructure that needs code and servers. Pasture aims for the gap: **zero-config, single-binary routing for individuals**, that adapts to whether your PC has a GPU or not.

- **GPU-less PC** → small local model, low threshold, escalate to cloud sooner.
- **GPU PC** → larger local model, high threshold, keep more work local.

The same prompt that goes to the cloud on a laptop stays local on a workstation — automatically.

> **Detection scope (ADR-261).** Total RAM is detected on Linux (`/proc/meminfo`),
> macOS (`sysctl hw.memsize`) and Windows (`wmic`). Discrete-GPU VRAM is detected
> via `nvidia-smi` only — Apple Silicon and AMD tier by RAM instead. If detection
> fails, Pasture says so and leans **local** rather than assuming a weak machine;
> set `PASTURE_RAM_MB` or `PASTURE_THRESHOLD` to make routing exact.

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
pasture eval               # routing accuracy: regression set + held-out set (exits 1 on regression)
pasture stats              # summarize the cost log (routes, tokens, spend)
pasture improvements       # show the self-improvement ledger (verified change history)
```

Force a route on the CLI with `--local` or `--cloud`; set the proxy's listen
address with `--addr host:port`. Everything else is a setting — see
[Configuration](#configuration) below for the complete list.

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

Defaults are sensible and Pasture runs with none of these set. Every setting is
listed here — this table is the complete set, enforced by a test against the
code (`test_readme_documents_every_setting`). Each can also be written in the
config file as `key = value`, using the lower-case name with the prefix dropped
(`local_model = llama3.2`); environment variables win over the file.

**Where things run**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_CONFIG` | `~/.config/pasture/config` | path to the optional `key = value` config file; a missing file is not an error |
| `PASTURE_LISTEN_ADDR` | `127.0.0.1:8645` | proxy listen address |
| `PASTURE_LOCAL_BACKEND` | `ollama` | `ollama`, or `lmstudio`/`openai` for any OpenAI-compatible local server |
| `PASTURE_OLLAMA_HOST` | `127.0.0.1` | local Ollama host |
| `PASTURE_OLLAMA_PORT` | `11434` | local Ollama port |
| `PASTURE_LOCAL_OPENAI_URL` | `http://127.0.0.1:1234/v1` | base URL when the local backend is OpenAI-compatible |
| `PASTURE_LOCAL_MODEL` | `llama3.2` | local model name |
| `PASTURE_LOCAL_FAST_MODEL` | _(off)_ | second local model for simple short queries; e.g. `phi3:mini` for greetings, `llama3.2` for harder |
| `PASTURE_LOCAL_TIMEOUT` | `120` | per-request local-backend read timeout in seconds |
| `PASTURE_LANG` | auto | force CLI language: `ja` or `en` |

**Routing — which way a request goes**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_THRESHOLD` | _(from hardware)_ | estimated-token length above which a request escalates to cloud; overrides the hardware-derived value |
| `PASTURE_FAST_THRESHOLD` | `50` | estimated-token threshold below which the fast local model is selected |
| `PASTURE_LOCAL_ONLY` | _(off)_ | route all traffic local; cloud backend disabled (GPU-less / air-gapped use) |
| `PASTURE_SKILLS` | _(off)_ | pin detected task types to a backend: `code:local,summarize:cloud` (`code`, `math`, `reason`, `summarize`, `translate`); checked before generic hard signals |
| `PASTURE_STRUCTURED_LOCAL` | _(off)_ | stop pure structured-output markers (`as json`, `csv format`, `markdown table`, …) forcing a cloud escalation; code-generation markers still escalate |
| `PASTURE_HARD_PROMPTS` | _(off)_ | file of prompts your local model handles badly, one per line; anything embedding-similar escalates to cloud |
| `PASTURE_HARD_THRESHOLD` | `0.85` | cosine similarity at which a prompt counts as "near a known-hard prompt" |
| `PASTURE_CASCADE` | _(off)_ | answer locally first and escalate only when the local answer is weak (needs the `cloud` feature and a key) |
| `PASTURE_CASCADE_LOGPROB` | `-1.0` | mean-logprob threshold below which a cascade answer is escalated; works with Ollama ≥ 0.12.11 (older builds fall back to a text heuristic; `pasture doctor` tells you which you have) |
| `PASTURE_HEALTH_COOLDOWN_SECS` | `30` | circuit-breaker cooldown, both directions: while a backend is Down its traffic is redirected to the other; 0 disables both breakers |
| `PASTURE_INJECT_CONTEXT` | _(off)_ | prepend a system message with the current date and OS — boosts lightweight models as PC assistants |
| `PASTURE_SYSTEM_PROMPT` | _(off)_ | system prompt prepended to every proxied request |

**Privacy**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_ALLOW_SENSITIVE_CLOUD` | _(off)_ | **turns off the privacy invariant.** By default a prompt classified sensitive never leaves the machine; set this and it can be sent to the cloud like any other. |
| `PASTURE_PSEUDONYMIZE` | _(off)_ | replace detected PII (email, IPv4, phone, API-key prefix) with stable opaque tokens in cloud-bound requests, then restore them in the response; works on buffered and streaming replies; the mapping is never logged |
| `PASTURE_INPUT_PII_SCAN` | _(off)_ | tally which PII categories trigger local-only routing; exposed on `/v1/stats` |
| `PASTURE_OUTPUT_PII_SCAN` | _(off)_ | tally PII categories seen in response text; detection-only, exposed on `/v1/stats` |
| `PASTURE_INJECTION_GUARD` | `off` | prompt-injection guard: `flag` detects role-switch/exfiltration patterns and annotates the response, `block` rejects with 400, `off` has zero overhead. Heuristic — see [Privacy](#privacy). |

**Cost control**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_BUDGET_DAILY_TOKENS` | `0` | daily cloud token budget (prompt + completion combined, UTC day); 0 = disabled |
| `PASTURE_BUDGET_ACTION` | `local-only` | action when the daily budget is exceeded: `local-only` (silently redirect), `warn` (log + proceed), `block` (return 429) |
| `PASTURE_SPIKE_FACTOR` | `50` | redirect a single request to local when it estimates more than N × the running average tokens (0 = disabled) |
| `PASTURE_CLOUD_PRICE_PER_1M` | `0,0` | cloud price in USD per 1M tokens as `<input>,<output>` (e.g. `2.50,10.00`); makes reported spend real dollars instead of 0 |

**Caching**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_CACHE` | `0` | exact-match response cache capacity; 0 = disabled |
| `PASTURE_CACHE_TTL` | `0` | cached-entry TTL in seconds; 0 = no TTL (FIFO eviction only) |
| `PASTURE_SEMANTIC_CACHE` | `0` | semantic cache capacity — also serves paraphrased repeats, via the local backend's embeddings; 0 = disabled |
| `PASTURE_SEMANTIC_THRESHOLD` | `0.92` | cosine-similarity threshold for a semantic-cache hit |
| `PASTURE_SEMANTIC_MIN_LEXICAL` | `0.0` | lexical second gate (Jaccard token overlap) a cosine hit must also clear; 0 disables |
| `PASTURE_CACHE_CONTROL` | _(off)_ | add `cache_control` hints on Anthropic system messages (prompt prefix caching); no-op for OpenAI |

**Cloud backend**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_CLOUD_PROVIDER` | _(none)_ | `openai` or `anthropic` |
| `PASTURE_CLOUD_MODEL` | `gpt-4o-mini` | cloud model id |
| `PASTURE_OPENAI_API_KEY` | _(none)_ | BYOK; never logged |
| `PASTURE_ANTHROPIC_API_KEY` | _(none)_ | BYOK; never logged |
| `PASTURE_CLOUD_RETRY` | `2` | retry transient cloud failures (5xx/timeouts) this many times, then fall back to local |
| `PASTURE_CLOUD_FALLBACK_PROVIDER` | _(off)_ | secondary provider tried when the primary fails all retries (`openai` or `anthropic`) |
| `PASTURE_CLOUD_FALLBACK_MODEL` | _(same as primary)_ | model to use on the fallback provider |

**Serving and exposure**

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_AUTH_TOKEN` | _(off)_ | require `Authorization: Bearer <token>` on `/v1/*` (for exposed, non-localhost deployments); `/health` stays open |
| `PASTURE_RATE_LIMIT` | `0` | cap `/v1/*` to this many requests per minute (global; 0 = unlimited) |
| `PASTURE_REQUEST_TIMEOUT` | `30` | per-connection read/write timeout in seconds (slow-loris guard; 0 = none) |
| `PASTURE_MAX_BODY_BYTES` | `16777216` | maximum request body size; larger bodies are rejected with 413 |
| `PASTURE_CORS_ORIGINS` | _(off)_ | CORS allow-list for browser clients: comma-separated origins, or `*` (off by default — a localhost server with `*` is reachable by any website) |

**Records** — all JSONL, all PII-free (I3)

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_COST_LOG` | `pasture-cost.jsonl` | cost log path; the source for `pasture stats`, `/v1/stats` and `/v1/history` |
| `PASTURE_DECISION_LOG` | _(off)_ | routing decision audit log: signals, threshold, route, reason |
| `PASTURE_ACCESS_LOG` | _(off)_ | per-request access log |
| `PASTURE_OTEL_LOG` | _(off)_ | OpenTelemetry GenAI semantic-convention trace records, one span per request; importable via the OTel Collector file receiver |

**Hardware overrides** — only needed when auto-detection cannot see your machine

| Variable | Default | Meaning |
|----------|---------|---------|
| `PASTURE_RAM_MB` | _(auto)_ | total RAM in MB; use when detection is unavailable on your OS |
| `PASTURE_GPU_VRAM_MB` | _(auto)_ | total GPU VRAM in MB; discrete-GPU detection is NVIDIA-only, so Apple Silicon and AMD tier by RAM unless this is set |

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

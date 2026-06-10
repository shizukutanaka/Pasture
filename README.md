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
pasture models             # recommended local models for your machine
pasture doctor             # check your setup and how to fix problems
pasture eval               # measure routing accuracy + threshold sweep
pasture stats              # summarize the cost log (routes, tokens, spend)
pasture improvements       # show the self-improvement ledger (verified change history)
```

Force a route with `--local` or `--cloud`. Enable cascade (answer locally, escalate to cloud only when the local answer is weak) with `PASTURE_CASCADE=1` (requires the `cloud` feature and a key). Enable an exact-match response cache with `PASTURE_CACHE=<size>` to avoid paying for repeated identical prompts, and a semantic cache with `PASTURE_SEMANTIC_CACHE=<size>` to also serve paraphrased repeats (cosine similarity via the local backend's embeddings; threshold `PASTURE_SEMANTIC_THRESHOLD`, default 0.92). List prompts your local model handles badly in a file and set `PASTURE_HARD_PROMPTS=<file>` to escalate anything embedding-similar to them (threshold `PASTURE_HARD_THRESHOLD`, default 0.85). Set the listen address for the proxy with `--addr host:port`.

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
- `GET /v1/models` — list the configured local (and cloud) model ids, OpenAI-compatible.
- `GET /v1/models/{id}` — retrieve one configured model (or 404), OpenAI-compatible.
- `GET /v1/stats` — live JSON snapshot of the cost-log counters (routes, rates, tokens, spend).
- `GET /health` — liveness check.

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
| `PASTURE_LOCAL_MODEL` | `llama3` | local model name |
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

The local backend talks to [Ollama](https://ollama.com) over plain HTTP. GPU acceleration (CUDA/Metal/ROCm) is handled by Ollama; Pasture writes no GPU code itself.

## Status

This is an early release (v0.9.0). Working today: hardware detection, deterministic hardware-adaptive routing, the OpenAI-compatible proxy, the local Ollama backend, structured JSONL cost logging, plus monetization surfaces (`donate` / `refer`) and a serverless Stripe donation Worker (`worker/`). The cloud backend is stubbed pending a vetted TLS dependency (see `ARCHITECTURE.md`, ADR-005).

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

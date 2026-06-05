# Security Policy

## Reporting a vulnerability

Please report security issues **privately**, not in public issues.

Use GitHub's private vulnerability reporting for this repository
(**Security → Report a vulnerability**). Include:

- affected version (`pasture version`) and platform,
- a description and, if possible, a minimal reproduction,
- the impact you observed.

We aim to acknowledge reports within a few days. Please give us reasonable time
to release a fix before any public disclosure.

## Scope and design

Pasture is a local-first routing proxy. A few properties are relevant to
security review:

- **No telemetry, no PII collection.** Prompt and response *content* is never
  written to logs. The cost log records only routes, token counts, an optional
  per-answer mean log-probability (a number), and timestamps.
- **Keys are BYOK and never logged.** Cloud API keys are read from environment
  variables (`PASTURE_OPENAI_API_KEY`, `PASTURE_ANTHROPIC_API_KEY`) and are
  never printed (`pasture config` shows only whether a key is set).
- **Privacy gating.** Prompts detected as sensitive (emails, IPs, card numbers,
  phone numbers, API keys/JWTs, and keyword/markers) are kept on the local model
  and are never sent to a cloud backend — even under a forced `--cloud` — unless
  the operator explicitly sets `PASTURE_ALLOW_SENSITIVE_CLOUD`.
- **Default bind is `127.0.0.1`.** The proxy is not network-exposed by default.
  If you bind it to a routable address, note that it currently has **no
  authentication or rate limiting** — put it behind your own auth/proxy.
- **Untrusted input.** The proxy parses client request bodies with a
  dependency-free JSON parser that bounds nesting depth to prevent stack
  exhaustion.

## Supported versions

This project is pre-1.0; only the latest released version receives fixes.

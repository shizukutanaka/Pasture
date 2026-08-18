# CLAUDE.md — agent guide for Pasture

Instructions for Claude (any model) working in this repo. Read this first; it
encodes conventions that are otherwise re-derived every session.

## What Pasture is

A **zero-dependency, single-binary, std-only, single-user** OpenAI-compatible
LLM routing proxy. For each request it decides — deterministically, from the
machine's actual hardware — whether to answer **local** (Ollama, free/private)
or escalate to **cloud** (OpenAI/Anthropic, behind the opt-in `cloud` feature).
The durable asset is the *verified improvement history*, not any one feature
(see `SELF_IMPROVEMENT.md`).

## Invariants — never break these (SPEC.md §1)

- **I1 Zero-dependency default.** The default build links no non-std crates. Any
  crate goes behind an opt-in Cargo feature (as `cloud` does for TLS) + an ADR.
- **I2 Privacy-first.** A prompt classified sensitive MUST NOT leave the machine.
- **I3 No PII in logs.** Logs and the cost record contain only category labels,
  never prompt/response content or secret values.
- **I4 Determinism.** Routing is a pure function of (prompt text, config, hardware).
- **I5 Localhost default.** Binds `127.0.0.1` unless explicitly configured.

If a change would weaken one of these, stop and reconsider the approach.

## Environment gotchas (this sandbox)

- **Build/test with `rustup run stable cargo …`.** `rust-toolchain.toml` pins
  1.75.0, which cannot be downloaded here — plain `cargo` fails. `stable` is the
  installed fallback; the code stays MSRV-1.75 compatible regardless.
- **Clippy is clean (ADR-259).** The 9 long-standing `doc list item` warnings in
  `guard.rs`/`privacy.rs` were fixed, and CI now runs `clippy --all-targets
  -D warnings` on stable. Any warning you see is yours; fix it.
- **Git proxy allows pushing the working branch only.** Tag pushes 403; there is
  no `create_release`/`create_tag` MCP tool. Publishing = pushing to the branch.
- **End-to-end HTTP testing** without a real Ollama: run a fake NDJSON backend in
  the scratchpad and point Pasture at it:
  ```python
  # fake_ollama.py — /api/chat (stream + non-stream) and /api/embed
  # stream:true → two NDJSON lines (a content chunk, then a done frame).
  ```
  Then `PASTURE_OLLAMA_PORT=<port> ./target/release/pasture serve --addr 127.0.0.1:<p>`
  and `curl` it. `pasture route "<text>" --json` shows a routing decision with no
  backend at all — the fastest E2E check.

## How to make a change (the ritual)

One improvement = one commit. For each:

1. **Claim an IMP-N** (next free number; see `COMPETITIVE.md` §4e for the live
   backlog and `IMPROVEMENTS.jsonl` for the last ADR number used).
2. **Implement**, mirroring an existing pattern rather than inventing a mechanism
   (e.g. a new PII value recognizer copies the credit-card/IBAN span pre-pass:
   `privacy.rs` `*_spans` + checksum, wire into `classify` and `pseudonymize.rs`).
3. **Test:** unit tests next to the code + a proxy/integration test where it
   crosses the wire. Then **verify end-to-end** (curl against a running release
   build, or `pasture route --json`). State the observed result.
4. **Sync the docs the drift-guards check:** `SPEC.md` (a new `/v1/stats` field
   MUST be added to §3.2c or `test_spec_documents_every_stats_response_field`
   fails; new i18n keys MUST be added to **both** EN and JA tables or
   `test_catalogs_have_same_keys` fails), plus `README.md` for a new endpoint and
   `COMPETITIVE.md` to move the item to shipped.
5. **Append an `IMPROVEMENTS.jsonl` entry** — one JSON line, schema:
   `{id, title, change, reason, effect, status, grounding, risk}`. `id` is
   `ADR-NNN-slug`; `effect` states the test count delta and the E2E result.
6. **Gate + commit + push:**
   ```
   rustup run stable cargo fmt
   rustup run stable cargo test          # 867+ pass, 0 fail
   rustup run stable cargo clippy --all-targets   # no NEW warnings
   git add -A && git commit && git push -u origin <branch>
   ```
   Commit trailer convention: `Co-Authored-By:` + `Claude-Session:` lines (match
   recent history).

## Verification gates (what CI enforces — ADR-014)

`cargo test` · `cargo clippy --all-targets` · `cargo fmt --check` · the `eval`
harness. All must be green before a push. Never push a red tree.

## Role split — Opus vs Sonnet

Both models read this file. The difference is **which kind of task each takes**.

### Opus — design, large surface, behaviour-changing, risky

Take items that need judgment, cross many call sites, or alter established
behaviour. **Plan before touching code.** If you change existing behaviour, write
the guarding/inverse test first. Current Opus-scale items (COMPETITIVE.md §4e):
- **IMP-45** streaming `/v1/responses` — a *second* SSE event protocol threaded
  through `stream_chat_to_socket` (the most delicate function in the repo).
- **IMP-44 model-suffix** — needs a `route_hint` field on `CompletionRequest`
  (25 explicit constructors across backend/cache/cli/cloud/proxy/tests).
- Anything that changes a routing signal's *direction* (e.g. IMP-40's deferred
  "keep long extraction local" half) — it moves existing tests; justify it.

### Sonnet — additive, pattern-following, mechanical, well-fenced

Take items where an existing pattern is copied and the test net is large.
**Mimic, don't invent.** Current Sonnet-scale items:
- **IMP-46** semantic-cache lexical second-gate (contained in `cache.rs`).
- **IMP-48** daily-counter history file for the dashboard (append-only JSONL).
- **IMP-49** split `proxy.rs` into modules — pure mechanical extraction, zero
  behaviour change, guarded by 867 tests.
- New PII value recognizers, new i18n keys, doc sync, test-coverage backfill,
  ledger hygiene — all copy an established ADR pattern.

When unsure which side a task falls on: if it changes existing behaviour or the
shape of a core struct, it's Opus; if it adds a new thing beside existing ones,
it's Sonnet.

## Map of the repo

- `src/proxy.rs` — HTTP server, request dispatch, routing glue, response builders (large).
- `src/routing.rs` — the decision engine + hard-signal detectors (`hard_signals`, `is_multi_step`, `is_time_sensitive`).
- `src/privacy.rs` — sensitivity classifier + PII value `*_spans` detectors + checksums.
- `src/pseudonymize.rs` — reversible `<TOKEN_n>` masking + `StreamRestorer` (SSE-boundary safe).
- `src/cache.rs` — exact-match + semantic caches; `src/calibrate.rs` — threshold tooling.
- `src/cloud.rs` — cloud backends (feature-gated); `src/backend.rs` — `CompletionRequest`, Ollama.
- `src/i18n.rs` — EN/JA catalogs (keep them key-parallel).
- **Docs:** `SPEC.md` (contract), `ARCHITECTURE.md` (ADRs), `COMPETITIVE.md`
  (backlog + §4e audit), `IMPROVEMENTS.jsonl` (ledger), `SELF_IMPROVEMENT.md` (why the ledger).

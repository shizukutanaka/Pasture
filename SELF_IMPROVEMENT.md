# Self-improvement in Pasture

This note maps the "recursive self-improvement (RSI) ecosystem" idea — a
self-improving knowledge-production system whose real asset is *accumulated,
verified improvement history* rather than any single model — onto what Pasture
actually is, and is honest about what belongs here and what does not.

> **Thesis adopted (from the RSI framing):**
> `RSI = Search × Verification × Compression`, and the durable asset of a
> long-lived system is its *verified improvement history*, not its model. Models
> are replaceable; **Traces, Skills, Benchmarks, Principles, and Verification
> data are not.**

Pasture is a single-binary, zero-dependency, std-only, single-user routing proxy.
That is the opposite of a distributed agent civilization. So the value of the RSI
framing here is **not** to grow Pasture into one — it is to recognize that Pasture
already implements the small, load-bearing kernel of that framing, and to make
that kernel a first-class, machine-readable asset.

## The "important 5", at Pasture's scale

The RSI argument is that the scarce capability is not *generating* improvements
(open LLMs do that) but *accumulating, verifying, and reusing* them. Its five
load-bearing components already have concrete, tiny analogues in this repo:

| RSI component | Pasture mechanism (already present) |
|---|---|
| **Trace Store** | The PII-free JSONL **cost log** — one record per request (route, model, tokens, cost, optional logprob). `stats` reads it back. |
| **Hidden Benchmark** | The **`eval`** harness — a labelled set (correctness) + threshold sweep (cost/locality), fully offline. |
| **Verifier** | **`cargo test`** + `eval` + `cargo fmt --check`, enforced as the CI release gate (ADR-014). |
| **Skill Compiler** | The **IMP-N → ADR → code** pipeline: each improvement is mined into a rule/feature, recorded as an ADR, and shipped behind a flag. Done by hand, deliberately. |
| **Governance** | **ADRs** + invariants (I1–I5) + the CI gate + human-gated release/signing. |

The one thing that was *missing*: this improvement history lived only as human
prose, scattered across `CHANGELOG.md`, `ARCHITECTURE.md`, and `COMPETITIVE.md`.
It was not a queryable, self-verifying asset.

## The kernel we added: a self-improvement ledger (IMP-13)

`IMPROVEMENTS.jsonl` makes the history an asset:

- **Improvement Explainability (#136).** Each entry is a *causal* record —
  `change` (what), `reason` (why), `effect` (measured or verified outcome) — not
  just a result. Cause → effect, not correlation.
- **Lifecycle Management (#148) + Half-Life (#164).** Each entry has a `status`
  ∈ {`shipped`, `deferred`, `retired`}, so superseded improvements can be marked
  retired rather than silently lingering.
- **Compression / the real asset (#151, #165).** The ledger *is* the Compression
  layer: the distilled, reusable record of what worked and why.
- **Verification of the asset itself (#175).** `src/improve.rs` parses the ledger
  with Pasture's own zero-dependency JSON reader, and a compile-time test
  (`test_bundled_ledger_is_valid`, via `include_str!`) asserts every entry parses
  and is a valid, explainable improvement. The improvement record cannot rot
  silently — CI checks it like any other code.

Read it with:

```sh
pasture improvements            # summarize + list the verified change history
pasture improvements path.jsonl # a specific ledger
```

This is the honest, in-philosophy size of "build the accumulation mechanism, not
more theory": one small data asset, one std-only parser, one CLI surface, one
self-check — and zero new dependencies.

## Anti-goals (explicitly rejected, to protect what Pasture is)

Most of the larger RSI vision is **out of scope by design**. Cargo-culting it
would break the invariants that make Pasture worth using. Concretely rejected:

- **P2P / distributed compute, collective memory fabric, trust graphs, network
  effects across nodes.** Pasture is single-user and localhost-default (I5).
  These require a networked multi-node substrate Pasture does not have and should
  not grow.
- **Learned / ML routers, agent-Darwinism, evolution engines, principle-mining
  models.** ADR-002 is *deterministic routing, no ML router*: fast, predictable,
  testable, offline. A learned router would need labels a single local user
  doesn't have and would make routing non-deterministic (violates I4).
- **Self-generating infrastructure (new DBs/caches/transport), autonomous
  refactoring of itself, an "AI research organization".** ADR-004 ships Pasture
  as a *thin proxy + CLI*, not a platform. Any non-std need must sit behind a
  Cargo feature (I1, ADR-010); a vector DB / Redis dependency is an explicit
  anti-goal in `COMPETITIVE.md`.
- **Logging prompt/response content to build a corpus.** Directly violates I2/I3:
  sensitive content never leaves the machine; logs hold only category labels,
  counts, route, model, cost, and confidence numbers — never values.

The deferred, *in-philosophy* research items (opt-in semantic cache via local
embeddings, calibrated-uncertainty escalation, auth/rate-limit for exposed
deployments, a live metrics endpoint) live in `COMPETITIVE.md` / `RESEARCH.md`
and as `deferred` rows in the ledger — to be pulled only if they preserve the
zero-dependency default.

## Why this is the right altitude

A 10%-better model that keeps no memory repeats its mistakes. A modest model with
an accumulating, **verified** improvement record compounds. For a tool whose whole
premise is "zero-config, single-binary, runs on the user's own hardware," the
realistic path is not a bigger brain — it is a disciplined, machine-checkable
record of what has actually been shown to work. That is the only part of the RSI
vision that fits in a zero-dependency binary, and it is the part that matters.

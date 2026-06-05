# Release Checklist — Pasture

Maps the current state to the project's release gate (CLAUDE.md §8) and marks
what still needs a **human decision** before public release. Automatable gates
run in CI on every push/PR; irreversible publishing steps are gated on explicit
human approval (release-approval skill, Class C).

Legend: ✅ done & enforced · ⚠️ partial · ⬜ pending human action

## §8 Release gate

| Gate | Status | Evidence / note |
|------|--------|-----------------|
| Linter warnings zero | ✅ | CI `clippy --all-targets -- -D warnings`, both feature sets |
| Formatting | ✅ | CI `cargo fmt --all -- --check` |
| All tests pass (coverage) | ✅ | 187 tests, default + `cloud`; CI matrix `["", "cloud"]` |
| Vulnerability scan CRIT/HIGH zero | ✅ | CI `cargo audit` (zero external deps by default; `cloud` pins native-tls/openssl) |
| Secret scan | ✅ | CI `gitleaks` job (I4); `.gitignore` excludes `.env`, `*.jsonl` |
| Cross-review (AI ×2 independent) | ⚠️ | Multiple single-AI review passes shipped (parser DoS, JP-phone/JWT privacy, CJK tokens, error surfacing). A second independent AI reviewer has not run. |
| Docs final (README/LICENSE/CHANGELOG/Help) | ✅ | All present; README + GETTING_STARTED (JA) + `pasture help`/`doctor`/`config` |
| Signed build + clean-install verify | ⬜ | Needs maintainer: code signing (Authenticode / Developer ID / GPG+Sigstore) + clean-install test |
| Beta test (real Ollama/keys, crash-free, flows) | ⬜ | Needs maintainer environment: real Ollama, real LM Studio, real cloud keys |
| Rollback procedure attached | ✅ | No DB migrations; behaviour reversible via env flags (`PASTURE_THRESHOLD`, `PASTURE_CASCADE`, `PASTURE_CACHE`, `PASTURE_ALLOW_SENSITIVE_CLOUD`); `cloud` is opt-in |

## Build / distribution (§8)

| Item | Status | Note |
|------|--------|------|
| SemVer | ✅ | currently 0.x; breaking changes flagged in CHANGELOG (e.g. 0.21.0 rename) |
| CHANGELOG format | ✅ | Keep-a-Changelog style, dated entries |
| Conventional Commits | ⚠️ | Followed in spirit; not CI-enforced |
| Env separation | ✅ | `.env.example` pattern; `.env` git-ignored; keys via env only (BYOK), never logged (I5) |
| `release.yml` (tag-triggered binaries) | ✅ | present; runs on tag push |

## Irreversible / external — HUMAN GO REQUIRED (release-approval Class C)

These are **not** performed automatically. Each needs explicit maintainer approval:

- ⬜ `git push origin main`
- ⬜ `git tag vX.Y.Z` + `git push --tags` (triggers `release.yml` binary build)
- ⬜ Create the GitHub repository `github.com/shizukutanaka/pasture` and first GitHub Release
- ⬜ Choose & apply final license confirmation (currently MIT, holder shizukutanaka 2026)
- ⬜ Donation worker: deploy Cloudflare Worker, switch Stripe from test (`sk_test_*`) to live — billing change, Class B/C
- ⬜ Affiliate/referral URLs: real sign-ups (`PASTURE_REF_*`)

## Pre-publish hygiene (done this pass)

- ✅ Removed stray runtime cost logs (`*-cost.jsonl`) that had leaked into the working tree
- ✅ Added `gitleaks` CI job
- ✅ Confirmed no API key value is ever printed (`pasture config` shows set/not-set only)

## Known limitations (document, not blockers)

- Cloud (HTTPS) streaming SSE assumes one SSE event per chunk (true for OpenAI/Anthropic).
- Threshold calibration (`pasture calibrate`) is a length-only proxy; learned routers do better but need labels a single local user lacks.
- UI languages: English + Japanese only (CJK/Hangul token estimation already supported).

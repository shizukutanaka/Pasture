# Contributing to Pasture

Thanks for your interest. Pasture optimizes for a small, auditable, dependency-
free codebase, so contributions are held to a few clear standards.

## Principles

- **Zero dependencies by default.** The default build uses only the Rust
  standard library. Anything needing a crate must sit behind an opt-in Cargo
  feature (as `cloud` does for TLS) and be justified in an ADR.
- **MSRV 1.75.0.** Pinned in `rust-toolchain.toml`; don't use newer APIs.
- **Simplicity and single responsibility.** One module, one job. Prefer
  deterministic, testable functions; keep I/O at the edges.
- **Privacy first.** Never log or transmit prompt/response content. Never print
  secrets. New routing or logging code must preserve this.

## Before you open a PR

Run the same gates CI runs, and make sure they're all green:

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --features cloud -- -D warnings
cargo test
cargo test --features cloud
```

- Add tests for new behaviour (pure logic should be unit-tested).
- Keep warnings at zero; justify any `#[allow(...)]` inline.
- Update `CHANGELOG.md` (Keep a Changelog style) and, for design decisions,
  add an ADR to `ARCHITECTURE.md`.

## Commits and versioning

- **Conventional Commits**: `feat:`, `fix:`, `refactor:`, `docs:`, `test:`,
  `chore:`, `build:`, `ci:`. Breaking changes use `feat!:` / `fix!:`.
- **SemVer** (currently 0.x). Flag breaking changes prominently in `CHANGELOG.md`.

## Scope

Good first contributions: new `connect <app>` recipes, additional UI language
catalogs (with native proofreading), additional privacy detectors, and routing
eval cases. Larger changes (new backends, routing strategy) — please open an
issue to discuss first.

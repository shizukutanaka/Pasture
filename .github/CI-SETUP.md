# Enabling CI (one manual step)

`ci-workflow.yml` in this directory is a complete, locally-verified CI workflow.
It is **not** at `.github/workflows/ci.yml` — where GitHub Actions would pick it
up — for one reason only: the automation token used to push this branch lacks the
`workflows` permission, so it is refused by GitHub with

```
refusing to allow a GitHub App to create or update workflow
`.github/workflows/ci.yml` without `workflows` permission
```

To activate it, run this locally (or move the file in the GitHub web UI):

```sh
mkdir -p .github/workflows
git mv .github/ci-workflow.yml .github/workflows/ci.yml

# Put the badge back — the test below will fail until you do.
sed -i '3a\
\
[![CI](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml/badge.svg)](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml)' README.md

git rm .github/CI-SETUP.md
cargo test --lib test_ci_badge_and_workflow_agree
git commit -m "ci: activate the CI workflow"
git push
```

**The badge is deliberately not in the README right now.** It used to be, and it
served a 404 — a green-looking claim that the gates run on every push, when no
workflow existed (ADR-269). `test_ci_badge_and_workflow_agree` in `src/config.rs`
now enforces the biconditional: badge without workflow is a lie, workflow without
badge means CI is live and the README is hiding it. Either way the test tells you
which side to fix.

The `sed` line above is what restores it; if you would rather do it by hand, paste
this under the "New to this?" line in `README.md`:

```markdown
[![CI](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml/badge.svg)](https://github.com/shizukutanaka/pasture/actions/workflows/ci.yml)
```

## What it enforces

The ADR-014 gates, on a `1.75.0` (MSRV) + `stable` matrix:

| Gate | Command |
|---|---|
| formatting | `cargo fmt --all -- --check` |
| lints | `cargo clippy --all-targets -- -D warnings` (stable; informational on 1.75) |
| tests | `cargo test --all-targets` |
| routing accuracy | `cargo run --release -- eval` |
| **invariant I1** | default dependency tree must contain exactly one crate |
| cloud path | separate job: `cargo build/test --features cloud` |
| supply chain | separate job: `cargo deny check` (config in `deny.toml`, IMP-27) |

Every gate was run locally before this was committed and each passes. Two caveats
stated honestly: the workflow has never executed on GitHub, so its YAML is
validated only by local reproduction of each step and may need a fixup commit;
and the `cloud` job could not be rehearsed in the authoring sandbox because
crates.io was blocked there, so it is written from the feature contract.

## History

An earlier attempt at this (ADR-134/IMP-27) hit the same `workflows` permission
wall and staged its workflow at `ci/ci.yml`. ADR-259 then added a second one
here without noticing, leaving two dead CI directories and two docs explaining
the same excuse. ADR-263 consolidated them: the older file's one unique
capability — the `cargo deny` supply-chain audit — was merged into this
workflow, and `ci/` was deleted.

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
git rm .github/CI-SETUP.md
git commit -m "ci: activate the CI workflow"
git push
```

The README's CI badge already points at `workflows/ci.yml`, so it starts
resolving as soon as the file is in place.

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

Every gate was run locally before this was committed and each passes. Two caveats
stated honestly: the workflow has never executed on GitHub, so its YAML is
validated only by local reproduction of each step and may need a fixup commit;
and the `cloud` job could not be rehearsed in the authoring sandbox because
crates.io was blocked there, so it is written from the feature contract.

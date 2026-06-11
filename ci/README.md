# CI workflow (IMP-27, ADR-134)

`ci/ci.yml` is the GitHub Actions workflow for Pasture: build + test +
clippy (`-D warnings`) + an informational rustfmt check, plus a
`cargo deny check` supply-chain audit (config in `deny.toml`).

## Why it lives here instead of `.github/workflows/`

This branch was pushed by an automated integration whose token lacks the
GitHub `workflows` permission, so a file under `.github/workflows/` is
rejected at push time. The workflow is therefore staged here, in a normal
path that can be committed and pushed.

## Activating it

Copy the file into the workflow directory and push with a token that has
the `workflow` scope (a normal user push works):

```sh
mkdir -p .github/workflows
git mv ci/ci.yml .github/workflows/ci.yml
git commit -m "Activate CI workflow (IMP-27, ADR-134)"
git push
```

GitHub Actions only discovers workflows under `.github/workflows/`, so CI
starts running on the next push once the file is moved there.

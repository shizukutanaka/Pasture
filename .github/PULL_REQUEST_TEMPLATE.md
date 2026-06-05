## Summary

What this change does and why.

## Checklist

- [ ] `cargo fmt --all` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean (and `--features cloud`)
- [ ] `cargo test` and `cargo test --features cloud` pass
- [ ] Tests added/updated for new behaviour
- [ ] `CHANGELOG.md` updated; ADR added if it's a design decision
- [ ] No new default dependencies; no logging of prompt/response content or secrets
- [ ] Conventional Commit title (e.g. `feat:`, `fix:`, `docs:`)

## Notes

Anything reviewers should know (trade-offs, follow-ups).

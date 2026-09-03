## Summary

<!-- What changed and why. Bullets are fine. Link any issue this closes. -->

## Test plan

<!-- Check only what you ACTUALLY ran. An unchecked box beats a false one. -->

- [ ] `just verify` (fmt-check · lint · test · msrv · lint-scripts · build)
- [ ] `just verify-full` (adds cargo-audit + cargo-deny — required if `Cargo.toml`/`Cargo.lock` changed)
- [ ] Exercised against the live crt.sh database (say which commands)
- [ ] `CHANGELOG.md` updated under `## [Unreleased]` (user-facing changes only)

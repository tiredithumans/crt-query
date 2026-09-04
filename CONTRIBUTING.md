# Contributing to crt-query

Thanks for taking an interest. This is a small, focused tool; the bar for
changes is that they keep it small and focused.

## Getting set up

```sh
git clone https://github.com/tiredithumans/crt-query
cd crt-query
just verify        # fmt-check · lint · test · msrv · lint-scripts · build
```

`rust-toolchain.toml` pins the toolchain, so rustup provisions the right Rust
version and components automatically on first build. [`just`](https://just.systems)
is the task runner — `just --list` shows everything available. You can run the
underlying `cargo` commands directly, but the recipes are what CI runs.

`lint-scripts` checks `install.sh`, `install.ps1` and
`packaging/homebrew/generate.sh`, so it needs
[`shellcheck`](https://www.shellcheck.net) and
[`pwsh`](https://aka.ms/powershell) on PATH (`brew install shellcheck
powershell`, or the equivalent for your package manager). Both are preinstalled
on the CI runner.

## Before opening a PR

```sh
just verify        # every offline gate, in CI order
just verify-full   # adds cargo-audit + cargo-deny (needs network)
```

Run `verify-full` whenever you touch `Cargo.toml` or `Cargo.lock`.

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`, `ci:`, `deps:`. If the change
is user-facing, add an entry under `## [Unreleased]` in `CHANGELOG.md`.

## Testing against crt.sh

**Every test in this repo is offline, and it needs to stay that way.** crt.sh is
a free public service running on donated infrastructure; it enforces connection
limits and statement timeouts and refuses connections under load regularly. A
test suite that queried it would be flaky for us and rude to them.

Test the pure logic — `to_rows`, `ExpiringRow::new`, the formatters, the clap
definition — and exercise the real database by hand when you need to:

```sh
just run search example.com --limit 20
```

## Things worth knowing before you change the SQL

These constraints are not style preferences; each one exists because the
alternative broke:

- **No server-side `ORDER BY` or `DISTINCT`.** The guest database enforces a
  statement timeout, so `LIMIT` has to be able to terminate early. Sorting and
  deduplication happen client-side over at most `--limit` rows.
- **Because of that, bound the window with a predicate.** `LIMIT` without one
  returns an arbitrary slice. Both `search` and `expiring` carry a validity
  bound for exactly this reason — see `IDENTITY_QUERY` in `src/queries/mod.rs`.
- **Unnamed prepared statements only** (`query_typed`). crt.sh sits behind a
  transaction-pooling pgbouncer, where named prepared statements fail.
- **Stay on the full-text index.** A bare `ILIKE` over the table hits the
  statement timeout.

## Security

Please do not open a public issue for a vulnerability — see
[SECURITY.md](.github/SECURITY.md).

## License

By contributing you agree that your contributions are licensed under the same
terms as the project: MIT OR Apache-2.0.

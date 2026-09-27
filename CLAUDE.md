# crt-query

A small Rust CLI that queries crt.sh's public, read-only PostgreSQL database
(`crt.sh:5432`, user `guest`) for certificate-transparency data: `search`,
`cert`, `expiring`, plus `cache`, `check-update` and `completions`. Read
[CONTRIBUTING.md](CONTRIBUTING.md) before changing anything; this file only
points at what matters most.

## Rules that are not negotiable

- **Tests are offline.** No test may ever contact crt.sh, a shared public
  service on donated infrastructure. A test that needs a failing connection
  points at `127.0.0.1` port `1`. Exercise the real database by hand only
  (`just run search example.com --limit 20`).
- **The SQL constraints** (details in CONTRIBUTING.md): no server-side
  `ORDER BY` or `DISTINCT`, so `LIMIT` can stop early; bound every `LIMIT`
  window with a validity predicate; unnamed prepared statements only
  (`query_typed`, because of the transaction-pooling pgbouncer); stay on the
  full-text index. Every statement has a golden snapshot in
  `src/queries/golden/`; re-blessing one means re-checking the columns its
  reader pulls out by name.
- **Machine contracts are pinned by tests.** Exit codes (`0` done, `1` failed,
  `2` clap usage error, `3` no such certificate, `4` `expiring
  --fail-on-expiring` found something), the `--json` key sets, CSV
  headers and header-only files for empty results, and the on-disk cache
  format (`tests/cache.rs` re-implements its filename digest on purpose). Change
  one only deliberately, with its test and the README.

## Gates

```sh
just verify        # fmt-check · lint · test · msrv · lint-scripts · doc
just verify-full   # adds cargo-audit + cargo-deny; run it when Cargo.toml or Cargo.lock changes
```

CI runs the same recipes (`.github/workflows/ci.yml`); `just --list` shows
them all. Clippy runs with `-D warnings`, rustdoc with `-D warnings`.

## Conventions

- Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`, `chore:`, `ci:`,
  `deps:`). User-facing changes get an entry under `## [Unreleased]` in
  `CHANGELOG.md`; the release workflow reads its `## [X.Y.Z] - YYYY-MM-DD`
  headers verbatim.
- Comments explain *why*, usually with the history of the bug that motivated
  the code. Match that style, in British spelling (behaviour, sanitise).
- Certificate text is attacker-chosen: table output goes through
  `display_safe`, CSV through `csv_safe` (`src/output/sanitise.rs`).
- Keep the dependency tree small; `deny.toml` bans any TLS stack on purpose.

## Where things live

- `src/main.rs` exit codes and dispatch · `src/cli.rs` clap definition ·
  `src/db.rs` connect/retry and error translation · `src/cache.rs` result
  cache · `src/queries/` the SQL statements and their readers · `src/output.rs` table/JSON/CSV.
- `install.sh`, `install.ps1`, `packaging/homebrew/generate.sh` and
  `.github/workflows/release.yml` make up the release channel.
- `.claude/skills/release` cuts a release; `.claude/skills/ship` lands a change
  on `main` through a PR.

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release automation reads the `## [X.Y.Z] - YYYY-MM-DD` headers below verbatim:
the `guard` job fails without one matching the tag, and the release notes are
extracted from the matching section. No `v` prefix, ASCII hyphen.

## [Unreleased]

## [0.2.0] - 2026-09-03

### Added

- `install.sh` and `install.ps1`: one-line installers for Linux/macOS and
  Windows. Each detects the target triple, resolves the newest release through
  `releases/latest/download` (so no version is hardcoded in the script or the
  README), verifies the archive against that release's `SHA256SUMS`, and
  installs. Re-running one is the upgrade path: it replaces the binary and
  clears the macOS quarantine attribute or the Windows mark-of-the-web that
  every fresh download arrives with. `--dir`/`-Dir` picks the destination and
  `--version`/`-Version` pins a release.
- Homebrew formula under `packaging/homebrew/`, so `brew upgrade` can carry the
  macOS/Linux cohort forward. It is generated from a release's `SHA256SUMS` by
  `just homebrew-formula`, which keeps its checksums from drifting away from
  the archives they name. The generator only emits the completions call for a
  release that actually has the `completions` subcommand: Homebrew builds those
  scripts by *running* the installed binary, so calling it against an older
  release aborts `brew install` outright rather than degrading — and no gate in
  this repo catches that, because none of them runs a released binary. It also
  leaves the `version` out of the formula, which Homebrew scans from the URLs
  and `brew audit --strict` rejects as a duplicate.
- `crt-query check-update` — reports whether a newer release exists, and exits
  `0` either way. Opt-in on purpose: it is the only subcommand that contacts
  anything other than crt.sh, and nothing checks in the background. `--json`
  gives `current`, `latest`, `update_available` and `release_url`.
- `crt-query completions <shell>` — completion scripts for bash, zsh, fish,
  PowerShell and elvish, generated from the same clap definition the parser
  uses, so they cannot drift from the flags they describe.
- A config file for connection settings, read from
  `$XDG_CONFIG_HOME/crt-query/config.toml` (`%APPDATA%\crt-query\config.toml`
  on Windows), so a custom `--db-url` need not be repeated on every run.
  Precedence is flag, then file, then built-in default. A missing file is fine;
  an unparseable or misspelled key fails the run rather than being ignored, so
  a typo cannot quietly redirect queries.
- `expiring` accepts several domains: `crt-query expiring a.example b.example`.
  One statement per domain — the statement timeout is what rules out folding
  them into a single query — so `--limit` applies per domain. Results merge
  into one report sorted by expiry, with a certificate covering two of the
  domains appearing once and carrying both matched identities. Repeated domains
  are collapsed case-insensitively rather than spending a second query on the
  same rows.
- A progress line on stderr (`querying crt.sh:5432 for "example.com"…`) before
  each statement, because the shared guest database is intermittently slow and
  a query that takes several seconds otherwise looks like a hang. Suppressed
  when stderr is not a terminal, so scheduled runs keep clean logs.

### Fixed

- Table output: the crt.sh ID, Issuer CA ID, Serial, and both timestamp
  columns no longer wrap — a serial cut mid-hex-digit or a timestamp split
  between date and time is harder to read, not easier. Issuer, Matched
  Identities, and Common Name now keep a minimum width instead of being
  squeezed to a character per line once those columns claim their space.

## [0.1.0] - 2026-09-02

Initial release.

### Added

- `search <query>` — find certificates by domain or identity, crt.sh style.
  `--valid-since <days>` (default 365) bounds the server-side window to
  certificates still valid within that period; `--all-history` removes the
  bound. The bound matters: the guest database's statement timeout rules out a
  server-side `ORDER BY`, so an unbounded `LIMIT` returns an arbitrary — in
  practice the oldest — slice of matches.
- `cert <id>` — full detail for one certificate by crt.sh ID, including subject,
  SANs and the SHA-256 fingerprint.
- `expiring <domain>` — certificates expiring within `--within <days>`
  (default 30), with `--since-expired <days>` (default 30) bounding how far back
  already-expired certificates are included. `--skip-expired` excludes them
  entirely and is exactly `--since-expired 0`.
- Three output formats: a terminal table by default, `--json` to stdout, and
  `--csv <path>` to a file. Both flags are global and work with every
  subcommand.
- Connection overrides: `--host`, `--port`, `--dbname`, `--user`, or a full
  `--db-url postgresql://…`.
- `--width <cols>` to control table width.

### Output guarantees

- Timestamps are UTC throughout. JSON carries an explicit `Z`; table columns are
  labelled `(UTC)`.
- `--json` always emits valid JSON on every exit path, including `[]` for an
  empty result and `null` for a `cert` ID that does not exist.
- `--csv` always writes the file, including a header-only file for an empty
  result, so a scheduled job never silently re-reads a previous run's report.
  Multi-valued columns (matched identities, SANs) are written one row per value
  rather than joined into a field, so the output is round-trippable.
- `days_left` is floored: negative once a certificate has expired, and `0` only
  within the last 24 hours before expiry.
- Piping into `head`, or quitting `less` early, ends output cleanly instead of
  reporting a broken pipe.
- Exit codes: `0` completed (even with no results), `1` failed, `2` no
  certificate with that crt.sh ID.

### Behaviour worth knowing

- Precertificate/leaf pairs are collapsed on `(issuer CA, serial)` — RFC 6962
  requires both to carry the same serial — keeping the lowest crt.sh ID. A pair
  is only collapsed when its validity windows also agree, so a serial collision
  between genuinely different certificates keeps both. `--no-dedupe` shows raw
  rows.
- `expiring` decides window membership and the `EXPIRED`/`days_left` labels with
  a single server-side clock, so `--skip-expired` cannot surface an `EXPIRED`
  row.
- A password embedded in `--db-url` is never echoed into error output.
- `--limit`, `--within`, `--valid-since` and `--since-expired` are range-checked
  client-side, before a connection is spent on the shared guest database.
- Connections are retried (queries are not); failures report their underlying
  cause rather than a bare "error connecting to server".

### Supply chain

- GitHub Actions pinned to full commit SHAs; dependencies locked by version and
  checksum. `cargo-audit`, `cargo-deny` and CodeQL run on every PR and weekly.
- Dual-licensed MIT OR Apache-2.0. Requires Rust 1.98+.

[Unreleased]: https://github.com/tiredithumans/crt-query/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/tiredithumans/crt-query/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tiredithumans/crt-query/releases/tag/v0.1.0

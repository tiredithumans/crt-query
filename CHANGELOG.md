# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release automation reads the `## [X.Y.Z] - YYYY-MM-DD` headers below verbatim:
the `guard` job fails without one matching the tag, and the release notes are
extracted from the matching section. No `v` prefix, ASCII hyphen.

## [Unreleased]

### Added

- `search --valid-since <days>` (default 365) and `--all-history`: a validity
  floor that aims the server-side `LIMIT` window at certificates that are still
  relevant. Without it the window landed on an arbitrary — in practice the
  oldest — slice, which was then sorted newest-first and looked authoritative.
- `expiring --since-expired <days>` (default 30): a lower bound on the expiry
  window. Previously `expiring` matched every certificate that had ever expired,
  so the window filled with rows from years ago before reaching anything close
  to renewal.
- `--width <cols>` to control table width, defaulting to the terminal width and
  falling back to 120 columns when stdout is a pipe.
- Exit code `2` for `cert <id>` when no such certificate exists, distinct from
  `1` for a failed run.
- Test suite (42 tests, all offline), `justfile` task runner, CI, CodeQL,
  `cargo-audit` and `cargo-deny` gates, and a release pipeline.

### Changed

- Timestamps are `DateTime<Utc>`: JSON now carries an explicit `Z` and table
  columns are labelled `(UTC)`. Previously naive timestamps were emitted with no
  zone marker and were read as local time.
- `--csv` writes one row per matched identity and per SAN instead of joining
  them into one field, so the output is round-trippable.
- `--skip-expired` is now exactly `--since-expired 0`, and one parameterized
  query serves both modes instead of two near-identical SQL constants.
- `days_left` is floored, so it is negative once a certificate has expired and
  `0` only within the last 24 hours before expiry.
- Rust 1.98 (MSRV and `rust-toolchain.toml` pin); comfy-table 7 → 8.

### Fixed

- Piping output into `head` (or quitting `less` early) no longer panics with
  exit 101; a closed reader now ends output cleanly.
- An empty result set still emits `[]` for `--json` and still writes the `--csv`
  file. Previously it wrote neither, so a scheduled job silently re-read the
  previous run's report.
- Deduplication only collapses a `(issuer CA, serial)` pair whose validity
  windows agree, so a serial collision between different certificates no longer
  drops one and grafts its identities onto the survivor.
- `expiring` decides window membership and the `EXPIRED`/`days_left` labels with
  a single server clock, so `--skip-expired` cannot surface an `EXPIRED` row.
- A password embedded in `--db-url` is no longer echoed into error output.
- `--limit`, `--within`, `--valid-since` and `--since-expired` are range-checked
  before a connection is spent on the shared guest database.
- The `--csv` destination is checked for writability before the query runs.
- Connection failures report their underlying cause instead of a bare
  "error connecting to server".

[Unreleased]: https://github.com/tiredithumans/crt-query/compare/HEAD...HEAD

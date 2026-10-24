# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release automation reads the `## [X.Y.Z] - YYYY-MM-DD` headers below verbatim:
the `guard` job fails without one matching the tag, and the release notes are
extracted from the matching section. No `v` prefix, ASCII hyphen.

## [Unreleased]

### Changed

- `search` and `expiring` print the `--limit` note whenever a name fills its
  row window, including under `--no-dedupe`, so a truncated result no longer
  passes for a complete one.
- A terminal now shows `connecting to crt.sh:5432…` while the tool connects,
  instead of nothing until the connection succeeds or fails.
- `--help` for each subcommand lists the global flags under `Connection`,
  `Output` and `Cache` headings, apart from the subcommand's own.

### Fixed

- A negative `--within`, `--since-expired`, `--valid-since`, `--limit` or
  `--width` is reported as out of range instead of as an unknown flag.
- An empty `expiring` result no longer ends "(in the first --limit rows)":
  an empty result is complete.

### Security

- Every invisible Unicode format character in certificate text, such as the
  soft hyphen in `exa\u{00ad}mple.com`, is shown as a visible escape in the
  table and CSV, not just the bidi and zero-width ones.
- Cached results are written readable by their owner only, even in a cache
  directory that was created with looser permissions.

## [0.6.1] - 2026-10-05

### Fixed

- The README and `search --help` now say what `%` and `_` really match: a term
  made only of wildcards finds nothing, unlike on the crt.sh website. (#54)

### Security

- Zero-width characters in certificate text are shown as visible escapes in
  the table and CSV, so `gi\u{200b}thub.com` no longer passes for
  `github.com`. (#54)

## [0.6.0] - 2026-09-27

### Added

- `cert` accepts a SHA-256 fingerprint as well as a crt.sh ID, bare or
  colon-separated as `openssl x509 -fingerprint -sha256` prints it. (#50)
- `expiring --fail-on-expiring` exits `4` when the report lists anything. (#50)
- `--quiet` (`-q`) drops informational messages from stderr and keeps errors,
  so cron stops mailing successful runs. (#50)
- Connection settings can come from `CRT_QUERY_HOST`, `CRT_QUERY_PORT`,
  `CRT_QUERY_DBNAME`, `CRT_QUERY_USER` and `CRT_QUERY_DB_URL`. (#50)
- Static musl builds for `x86_64` and `aarch64` Linux, for Alpine and for glibc
  older than 2.34; `install.sh` picks them automatically. (#50)
- A native Windows ARM64 build, which `install.ps1` now installs in place of the
  emulated x86-64 one. (#50)

### Changed

- **Breaking:** a config-file `db_url` combined with `--host`, `--port` or a
  `CRT_QUERY_*` setting is now an error instead of silently overriding them.
  Keep only one of the two. (#50)
- **Breaking:** `cache path` exits `1` when no absolute cache directory can be
  found, instead of printing nothing and exiting `0`. (#50)

### Fixed

- A `cert` lookup that finds nothing is cached for an hour, not 30 days, so a
  certificate that reaches crt.sh later is found. (#50)
- Two databases on one server no longer share cache entries. Existing entries
  miss once after upgrading. (#50)
- Two runs writing the same cache entry at once can no longer corrupt it. (#50)
- `check-update` no longer fails on GitHub's API rate limit. (#50)

### Security

- On Windows, `check-update` runs `System32\curl.exe` by full path, so a
  `curl.exe` placed next to `crt-query.exe` is no longer run instead. (#50)

## [0.5.2] - 2026-09-25

### Changed

- Updated dependencies; no behaviour change. (#45, #48)
- The Homebrew tap now updates as each release is published; it had been stuck
  on v0.4.0. (#43)

## [0.5.1] - 2026-09-04

### Fixed

- `search` and `expiring` print a note on stderr when `--limit` filled up and
  deduplication then shortened the list, so a cut-off answer no longer looks
  complete. Raise `--limit` or use `--no-dedupe`. (#40)

## [0.5.0] - 2026-09-04

### Added

- Results are cached locally: `search` and `expiring` for an hour, `cert` for
  30 days. A fully cached run never connects to crt.sh. (#37)
- `--no-cache` skips the cache, and `--refresh` re-queries and updates it. (#37)
- `cache path` and `cache clear`, plus `cache` and `cache_ttl_secs` config
  keys. (#37)

### Changed

- A connection retry that then succeeds is no longer reported. If every attempt
  fails, the error still lists each cause. (#36)
- Retries are quicker: five attempts backing off from 250 ms to 2 s, and a
  server that accepts the connection but never answers is given up on
  sooner. (#36)

## [0.4.1] - 2026-09-04

### Changed

- Every documented `cargo install --git` command now passes `--locked`, so a
  source install builds the dependency versions that were tested. (#25)

### Fixed

- The prebuilt Linux binaries start again on Debian 12, RHEL 9, Ubuntu 22.04
  and Amazon Linux 2023, and `brew install` on Linux works; they need glibc
  2.34 or newer. (#21)
- An empty or whitespace-only search term is a usage error (exit `2`) instead
  of an empty result. (#31)
- A `--csv` write that fails part-way leaves the previous report intact. (#30)
- A `--csv` destination that is a symlink is no longer deleted by the
  pre-query check. (#24)
- A certificate with unparseable validity dates sorts last in `search` and
  `expiring`, not first. (#32)
- `cert --json` for a missing ID keeps exit code `3` when the reader closes the
  pipe early. (#24)
- `install.sh` and `install.ps1` check that the new binary runs before it
  replaces the installed one. (#22)
- `install.sh` works with BusyBox `sha256sum`, and on musl Linux says there is
  no prebuilt build instead of installing one that cannot run. (#22)
- `install.ps1` no longer closes the PowerShell window when it fails, and
  resolves a relative `-Dir` before adding it to `PATH`. (#22)
- The macOS install docs no longer tell you to run an `xattr -d` that
  fails. (#22)
- `check-update`'s upgrade hint lists only routes that work on your platform,
  including `install.ps1` on Windows and `brew upgrade` for Homebrew
  installs. (#27, #29)
- Connection errors no longer blame an overloaded crt.sh when the failure was
  elsewhere, such as a wrong password for another host. (#23)
- Connection settings that cannot work, such as a `db_url` with no host, fail
  at once instead of being retried. (#23)
- An invalid `db_url` in the config file is reported with the file's path, not
  as `invalid --db-url`. (#23)

### Security

- A CSV field starting with `-` is quoted unless it is a finite number, so it
  cannot reach a spreadsheet as a formula. (#26, #30)
- Bidirectional control characters in certificate text are escaped in the
  table and in CSV, so a row cannot display a hostname it does not
  contain. (#24, #26)

## [0.4.0] - 2026-09-03

### Added

- Prebuilt binaries for ARM64 Linux (`aarch64-unknown-linux-gnu`). (#18)
- On Windows ARM, `install.ps1` installs the x86-64 build, which runs under
  emulation, instead of failing. (#18)

### Changed

- **Breaking:** `--csv` is now a machine format: NULL is an empty field instead
  of `-`, and timestamps are full RFC 3339. Scripts that read `-` or
  minute-precision times need updating. (#17)
- **Breaking:** `cert` exits `3` instead of `2` when no certificate has that ID,
  so `2` only ever means a usage error. Scripts that check for `2` need
  updating. (#14)
- **Breaking:** `check-update --csv` uses display headers (`Current`, `Latest`,
  `Update Available`, `Release URL`); the `--json` keys are unchanged. (#14)

### Fixed

- A search term containing a backslash no longer finds nothing for
  certificates that exist. (#15)
- A stalled crt.sh no longer hangs the run: a connection attempt gives up after
  15 s and a query after 180 s. (#15)
- A rejected password or missing database fails at once instead of being
  retried. (#15)
- `install.ps1` no longer fails at the last step of a successful install. (#13)
- `install.ps1` no longer expands `%VAR%` entries in your user `PATH`, such as
  rustup's, when it adds itself. (#13)

### Security

- CSV fields a spreadsheet would run as a formula (starting `=`, `+`, `@`, tab
  or carriage return) are prefixed with `'`. (#17)
- Control characters in certificate fields are escaped in table output, so they
  cannot corrupt the terminal. (#17)
- Release archives carry build-provenance attestations; check one with
  `gh attestation verify <archive> --repo tiredithumans/crt-query`. (#18)

## [0.3.0] - 2026-09-03

### Added

- `search` accepts several names, and `--limit` applies to each:
  `crt-query search example.com example.org`. (#11)
- `search --skip-expired` shows only certificates that have not expired. (#11)

### Fixed

- `--width` is now honoured exactly, narrower or wider. (#11)

## [0.2.0] - 2026-09-03

### Added

- One-line installers, `install.sh` for Linux and macOS and `install.ps1` for
  Windows, that verify checksums; re-run one to upgrade. (#7)
- A Homebrew formula. (#7)
- `check-update` reports whether a newer release exists. Nothing checks in the
  background. (#7)
- `completions <shell>` for bash, zsh, fish, PowerShell and elvish. (#7)
- A config file for connection settings, at
  `$XDG_CONFIG_HOME/crt-query/config.toml` or
  `%APPDATA%\crt-query\config.toml` on Windows. (#7)
- `expiring` accepts several domains. (#7)
- A progress line on stderr while a query runs, shown only in a terminal. (#7)

### Fixed

- Table columns such as IDs, serials and timestamps no longer wrap
  mid-value. (#4)

## [0.1.0] - 2026-09-02

Initial release.

### Added

- `search` finds certificates by domain or identity, `cert` shows one
  certificate by crt.sh ID, and `expiring` lists certificates about to expire.
- Table output by default, `--json` to stdout, and `--csv <path>` to a file.
- Connection overrides: `--host`, `--port`, `--dbname`, `--user` and
  `--db-url`.

[Unreleased]: https://github.com/tiredithumans/crt-query/compare/v0.6.1...HEAD
[0.6.1]: https://github.com/tiredithumans/crt-query/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/tiredithumans/crt-query/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/tiredithumans/crt-query/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/tiredithumans/crt-query/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/tiredithumans/crt-query/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/tiredithumans/crt-query/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/tiredithumans/crt-query/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/tiredithumans/crt-query/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tiredithumans/crt-query/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tiredithumans/crt-query/releases/tag/v0.1.0

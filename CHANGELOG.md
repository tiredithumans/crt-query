# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release automation reads the `## [X.Y.Z] - YYYY-MM-DD` headers below verbatim:
the `guard` job fails without one matching the tag, and the release notes are
extracted from the matching section. No `v` prefix, ASCII hyphen.

## [Unreleased]

### Fixed

- **The prebuilt Linux binaries could not start on most current distributions.**
  Both Linux targets built on Ubuntu 24.04 runners, and a binary inherits the
  glibc floor of the machine that linked it: std's process-spawn path — reached
  by `check-update` through `Command::new("curl")` — references `pidfd_spawnp`
  and `pidfd_getpid`, which are versioned symbols as of glibc 2.39, so the
  linker stamped a non-weak `GLIBC_2.39` requirement into both v0.4.0 archives.
  `ld.so` treats a missing non-weak version as fatal, so the binary died before
  `main` on Debian 12, RHEL/Rocky 9, Ubuntu 22.04 and Amazon Linux 2023 — and on
  Homebrew for Linux the failure aborted `brew install` itself, which runs the
  binary to generate completions. The Linux rows now pin `ubuntu-22.04`
  runners, which drops the floor to glibc 2.34, and a `glibc floor` step fails
  the release if a future runner-image bump raises it again.
- **Both installers no longer destroy a working install to find out the new
  binary does not run.** They overwrote the installed binary and only then ran
  `--version` on it, so a replacement that cannot start on this system had
  already replaced a working one — with the scratch directory cleared by the
  exit trap and nothing left to restore. `install.sh` also sent that check's
  stderr to `/dev/null`, discarding the loader's own explanation. The download
  is now staged beside the destination, run, and moved into place only if it
  works; the failure message carries the loader's first line.
- `install.sh`'s checksum verification used GNU long options that BusyBox's
  `sha256sum` rejects, so on a BusyBox host a byte-perfect download exited
  non-zero and was reported as `checksum mismatch … the download does not match
  the release's SHA256SUMS`. It now uses `-c` with output redirected, the one
  form GNU coreutils, BusyBox and macOS `shasum` all accept.
- `install.sh` mapped every Linux to `unknown-linux-gnu`, so a musl host
  installed a glibc binary whose interpreter does not exist — `execve` returns
  ENOENT and the shell reports "not found" for a file that is plainly there. It
  now detects musl and reports that the release ships no such archive, which is
  what the existing no-build-for-this-target branch was already written to say.
- `install.ps1` ended its failure path with `exit 1`, which unwinds the *host*
  under both documented invocations (`irm … | iex` and the scriptblock form) —
  so the one outcome a user most needs to read, the checksum mismatch, closed
  the window it was printed to. It now raises a single-line terminating error,
  which still exits 1 under `pwsh -File`.
- `install.ps1` wrote a relative `-Dir` verbatim into the persistent user PATH,
  where every future process would resolve it against its own working
  directory. The path is now resolved before use.

### Changed

- The README now states the glibc 2.34 floor for the prebuilt Linux archives,
  in the install table and again under manual download.
- The macOS Gatekeeper documentation was wrong: `curl` does not set
  `com.apple.quarantine` — only downloaders that opt into
  `LSFileQuarantineEnabled` do — so nothing on the documented install paths ever
  arrived quarantined, and the manual-install instructions told people to run an
  `xattr -d` that fails with `No such xattr`. Signing status has no bearing on
  quarantine either. The claim is corrected in the README, `install.sh` and the
  Homebrew packaging notes; `install.sh` still clears the attribute, now before
  the run check rather than after, for an archive that arrived another way.

## [0.4.0] - 2026-09-03

### Added

- Prebuilt binaries for `aarch64-unknown-linux-gnu`. `install.sh` computed this
  target on every ARM64 Linux host — Graviton, Ampere, a Raspberry Pi — and no
  release shipped an archive for it, so the install failed with a list of what
  was available. The Homebrew formula gains a matching `on_arm` block inside
  `on_linux`.
- Windows on ARM installs the x86-64 build, which Windows runs under emulation,
  rather than failing outright when no native `aarch64-pc-windows-msvc` archive
  exists. It says so when it does. The `x86` branch is gone: crt-query has never
  shipped a 32-bit build, so it could only ever resolve to an archive that does
  not exist.
- Every release archive now carries a build-provenance attestation signed by the
  workflow run that produced it. `SHA256SUMS` is served from the same release as
  the archives it covers, so it proves they agree with each other and nothing
  about where they came from — which matters because `check-update` points at
  the same channel. Verify with
  `gh attestation verify <archive> --repo tiredithumans/crt-query`.

### Changed

- **`--csv` is now a machine format rather than a copy of the table.** It shared
  the table's formatters, so a NULL was written as `-` and timestamps lost their
  seconds. The `Days Left` column therefore mixed `-30` with a literal `-`,
  which types the whole column as text in pandas and Excel — contradicting the
  code's own claim that `days_left` "is usable on its own in JSON and CSV" — and
  a certificate expiring at 23:59:59 was recorded as `23:59`. NULL is now an
  empty field and timestamps are full RFC 3339 instants. A script reading the
  `-` placeholder or parsing minute-precision timestamps needs updating; the
  table is unchanged.

- **`cert` now exits `3`, not `2`, when no certificate has that crt.sh ID.**
  clap exits `2` on a usage error, so `crt-query cert "$id"` with an empty or
  unset `$id` reported "no such certificate" for what was a typo — turning a
  shell slip into a false "the certificate is gone" alert, the exact confusion
  the distinct code exists to prevent. A script keying on `2` for not-found
  needs updating; `0` and `1` are unchanged.
- `check-update --csv` and its table now use display column headers
  (`Current`, `Latest`, `Update Available`, `Release URL`) like every other
  record type. The `--json` keys are unchanged.

### Fixed

- CSV fields that a spreadsheet would evaluate as a formula are now prefixed
  with `'`. Issuer, subject, common name and SAN are text lifted from a public
  certificate-transparency log — whoever got the certificate issued chose them —
  and Excel and LibreOffice execute a cell beginning `=`, `+`, `@`, tab or
  carriage return. The rule deliberately excludes `-`, so `days_left`'s
  negatives stay numeric.
- Control characters in a certificate field no longer corrupt the terminal.
  comfy-table counts ANSI escape bytes as printable width, so an escape inside a
  common name was split mid-sequence by wrapping: the reset landed on a
  different line, the row was drawn narrower than its own borders, and the
  colour leaked into the rest of the session. A carriage return returned the
  cursor to column 0 and overwrote the line just drawn. Both are now escaped to
  visible text in table output. JSON was never affected.

- A search term containing a backslash no longer reports "No certificates
  found" for certificates that exist. The identity filter's `ILIKE` had no
  `ESCAPE` clause, so Postgres' default backslash escape applied: every
  backslash was swallowed and the character after it taken literally, making
  `a\b` search for `ab` and a trailing backslash build a pattern that cannot
  match at all. `--help` documents `%` and `_` as the only wildcards, and
  `ESCAPE ''` now makes that true. Hostnames do not contain backslashes, so
  this shows up on identity terms — a DN fragment, an email SAN — where a
  confident, wrong "nothing exists" is the worst possible answer.
- A stalled crt.sh no longer hangs the run. `connect_timeout` bounds only the
  TCP connect, not the startup exchange and not the query, so a server that
  accepted the socket and then stopped answering left the process silent until
  the two-hour keepalive default noticed — wedging a scheduled `expiring --csv`
  slot instead of failing into the next run. A statement is now capped at 180s
  and a connection attempt at 15s. The statement cap sits deliberately above
  crt.sh's own ~120s timeout so the server's more specific "narrow your query"
  message stays the one you see.
- A rejected credential or a missing database is no longer retried three times.
  Those fail identically on every attempt, so retrying only buried the real
  error under two misleading "retrying..." lines. Load and transport failures
  still get the full retry budget.

- Windows installs no longer abort at the last step. `install.ps1` confirmed a
  good install by piping `crt-query --version` into `Select-Object -First 1`;
  the pipeline short-circuits on that one line, a short-circuited native command
  never sets `$LASTEXITCODE`, and reading an unset variable under
  `Set-StrictMode -Version Latest` is a terminating error. The binary was
  already in place, so the failure came after a successful install: no PATH
  entry, an error on stderr, and — for the documented `irm | iex` route — a
  shell killed by the `exit 1`. It presented as intermittent because a session
  where any earlier native command had run had a stale `$LASTEXITCODE` to read.
  CI now runs that verification shape against a freshly built binary on
  `windows-latest`; the existing script gate only parses the file.
- `install.ps1` no longer flattens `%VAR%` entries in your user `PATH`.
  `[Environment]::GetEnvironmentVariable` expands them and
  `SetEnvironmentVariable` writes the expanded text back as `REG_SZ`, so adding
  crt-query to `PATH` permanently resolved every other installer's unexpanded
  entry — rustup's `%USERPROFILE%\.cargo\bin` among them. The value is now read
  and written through the registry with its kind preserved.

## [0.3.0] - 2026-09-03

### Added

- `search` accepts several names, like `expiring` already did:
  `crt-query search example.com example.org`. One statement per name — the
  statement timeout rules out folding them into a single query — so `--limit`
  applies per name, results merge into one list, and a certificate matching two
  of the names appears once carrying both matched identities.
- `search --skip-expired` — only certificates that have not expired. It floors
  the window at the server's own clock rather than a client-side timestamp, the
  same rule that stops `expiring --skip-expired` surfacing an `EXPIRED` row. A
  separate predicate rather than a zero-day `--valid-since`, because zero is
  already `--all-history`; the two compose, and `--all-history --skip-expired`
  means "everything crt.sh holds that is live today".

### Fixed

- `--width` is now honoured exactly, in both directions. It could previously
  neither narrow nor widen the wide result tables: comfy-table applies a column
  constraint over the table width, so the no-wrap and minimum-width heuristics
  added in 0.2.0 held every request open at roughly 190 columns, and
  `ContentArrangement::Dynamic` stops at the natural content width rather than
  spending surplus space. An explicit `--width` now switches to
  `DynamicFullWidth` and drops those constraints — they exist to keep the
  *automatic* layout readable, and should not overrule a width someone asked
  for. Choosing the width automatically is unchanged.

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

[Unreleased]: https://github.com/tiredithumans/crt-query/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/tiredithumans/crt-query/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/tiredithumans/crt-query/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/tiredithumans/crt-query/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/tiredithumans/crt-query/releases/tag/v0.1.0

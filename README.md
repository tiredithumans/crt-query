# crt-query

> Query [crt.sh](https://crt.sh) certificate-transparency data straight from its
> public PostgreSQL database — no HTTP API, no scraping, no API key.

[![CI](https://github.com/tiredithumans/crt-query/actions/workflows/ci.yml/badge.svg)](https://github.com/tiredithumans/crt-query/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.98-orange.svg)](./rust-toolchain.toml)

Three questions from the command line: what certificates exist for a name, what
is inside one certificate, and what is about to expire. Output is a table, JSON,
or CSV.

```console
$ crt-query cert 22625564176
┌─────────────────────┬────────────────────────────────────────────────────────────────┐
│ crt.sh ID           ┆ 22625564176                                                    │
│ Issuer CA ID        ┆ 204411                                                         │
│ Issuer              ┆ C=GB, O=Sectigo Limited, CN=Sectigo Public Server              │
│                     ┆ Authentication CA OV R36                                       │
│ Subject             ┆ C=US, ST=California, O=Internet Corporation For Assigned Names │
│                     ┆ and Numbers, CN=example.com                                    │
│ Common Name         ┆ example.com                                                    │
│ Serial              ┆ 009de10580fa26441939f38af4afb1cb40                             │
│ Not Before (UTC)    ┆ 2025-11-20 00:00                                               │
│ Not After (UTC)     ┆ 2026-11-20 23:59                                               │
│ SHA-256 Fingerprint ┆ 5c83f01af4edf38533f0da804bb740960120e9da1129216281a8542aea374b │
│                     ┆ dd                                                             │
│ SANs                ┆ example.com; example.edu; example.net; example.org             │
└─────────────────────┴────────────────────────────────────────────────────────────────┘
```

## Install

| Platform | Command |
| --- | --- |
| macOS · Linux (x86-64 · ARM64) | `brew install tiredithumans/tap/crt-query` |
| macOS · Linux | `curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh \| sh` |
| Windows (x86-64) | `irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1 \| iex` |
| From source | `cargo install --locked --git https://github.com/tiredithumans/crt-query` |

Every prebuilt route resolves the newest release, verifies the archive against
that release's `SHA256SUMS`, and stages the new binary beside the installed one
so it only replaces it once it has been shown to run. **Re-run the same command
to upgrade.**

The prebuilt Linux binaries are glibc builds and need **glibc 2.34 or newer** —
RHEL/Rocky 9, Ubuntu 22.04, Debian 12, Amazon Linux 2023 and anything later. On
an older distribution, or on a musl system such as Alpine, build from source.

<details>
<summary>Script options, and installing by hand</summary>

Piping a script into a shell is a decision, not a default. To read it first:

```sh
curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh -o install.sh
less install.sh
sh install.sh
```

`install.sh` takes `--dir <path>` and `--version vX.Y.Z`. When piping, options
go after `sh -s --`:

```sh
curl -fsSL .../install.sh | sh -s -- --dir "$HOME/.local/bin"
```

`install.sh` also reads `CRT_QUERY_DIR` and `CRT_QUERY_VERSION`, which is the
ergonomic way to configure the piped form — no `sh -s --` needed.

`install.ps1` takes `-Dir <path>`, `-Version vX.Y.Z` and `-NoPathUpdate`. It has
no environment-variable equivalent. It
installs to `%LOCALAPPDATA%\Programs\crt-query` and adds that to your user
`PATH` — no admin rights, but open a new terminal for it to take effect. To pass
options, the script has to become a scriptblock first:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1))) -Dir C:\tools
```

**Manual download.** Releases ship archives for `x86_64-unknown-linux-gnu`,
`aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin` and
`x86_64-pc-windows-msvc`, plus one `SHA256SUMS` covering all of them. Both Linux
archives are glibc builds requiring **glibc 2.34 or newer**; there is no musl
archive, so Alpine and other musl systems build from source:

```sh
TARGET=x86_64-unknown-linux-gnu        # Apple Silicon: aarch64-apple-darwin
# `latest` redirects to the newest release, so there is no version to keep
# up to date here. The tag is in the archive name once it lands.
BASE=https://github.com/tiredithumans/crt-query/releases/latest/download
curl -LO "$BASE/SHA256SUMS"
ARCHIVE=$(awk -v t="$TARGET" '$2 ~ t {sub(/^\*?\.\//, "", $2); print $2}' SHA256SUMS)
curl -LO "$BASE/$ARCHIVE"

sha256sum --ignore-missing -c SHA256SUMS   # macOS: shasum -a 256 --ignore-missing -c

tar -xzf "$ARCHIVE"
sudo install -m 0755 "${ARCHIVE%.tar.gz}/crt-query" /usr/local/bin/
```

Every archive also carries a build-provenance attestation signed by the release
workflow. `SHA256SUMS` proves the archives agree with each other; the
attestation is what ties them to the run that built them:

```sh
gh attestation verify "$ARCHIVE" --repo tiredithumans/crt-query
```

Two things that catch people out doing this by hand:

- Gatekeeper quarantine is set by the downloader, not by macOS in general: only
  applications that opt into `LSFileQuarantineEnabled` — browsers, Mail,
  AirDrop — mark what they fetch. `curl` does not, so an archive downloaded the
  way described above carries no `com.apple.quarantine` and needs no `xattr`
  step. If you fetched the archive with a browser instead, clear it with
  `xattr -d com.apple.quarantine /usr/local/bin/crt-query`. Signing status has
  no bearing on this either way.
- `SHA256SUMS` covers one release. Verifying a new archive against the previous
  release's file fails, and should — delete the stale one rather than adding
  flags until it stops complaining.

</details>

## Usage

```sh
# Certificates for a domain or identity, crt.sh style
crt-query search example.com --limit 100

# Several names at once, and only certificates that are still valid
crt-query search example.com example.org --skip-expired

# Everything in one certificate, by crt.sh ID
crt-query cert 22625564176

# What is expiring, or recently expired
crt-query expiring example.com example.org --within 30
```

`--json` writes JSON to stdout instead of a table, and `--csv <path>`
additionally writes a CSV file. Both are global:

```sh
crt-query search example.com --json | jq '.[].id'
crt-query expiring example.com example.org --within 90 --csv report.csv
```

`search` and `expiring` both take any number of names. Each one is queried
separately — the guest database's statement timeout rules out folding them into
a single query — so `--limit` applies per name. The results merge into one
report, and a certificate matching two of the names appears once, carrying both
matched identities.

Connection overrides: `--host`, `--port`, `--dbname`, `--user`, or a full
`--db-url postgresql://…`. Set them once in a [config file](#configuration)
rather than on every run.

## The search window

The guest database enforces a statement timeout, so every query must stay on the
full-text index and let `LIMIT` terminate early — which rules out a server-side
`ORDER BY`. `LIMIT` therefore takes an *arbitrary* slice of the matching rows, in
practice the oldest. Both search commands bound that slice so it lands on
certificates you care about:

| Flag | Default | Meaning |
| --- | --- | --- |
| `search --valid-since <days>` | 365 | Only certificates still valid within this many days of now |
| `search --all-history` | off | No validity floor; search everything crt.sh holds |
| `search --skip-expired` | off | Only certificates that have not expired — stricter than `--valid-since`, and combines with `--all-history` to mean "everything crt.sh holds that is live today" |
| `expiring --within <days>` | 30 | Look-ahead: certificates expiring within this window |
| `expiring --since-expired <days>` | 30 | Look-back: how far back to include already-expired certificates |
| `expiring --skip-expired` | off | Exclude expired certificates entirely (same as `--since-expired 0`) |

Without the look-back bound, `expiring` matches every certificate that ever
expired, and the `LIMIT` window fills with rows from years ago before reaching
anything close to renewal.

## Output

| | |
| --- | --- |
| Timestamps | UTC everywhere. JSON and CSV carry an explicit offset; table columns are labelled `(UTC)` and truncate to the minute |
| `--json` | Always valid JSON, including `[]` for an empty result and `null` for a missing `cert` ID |
| `--csv` | `search`, `cert` and `expiring` always write the file, header-only when empty, so a scheduled job never re-reads a stale report. The file is written beside the destination and renamed into place once complete, so a run that fails at any point — the write itself included — leaves the previous report untouched, and a reader never sees a partial one. A symlinked destination keeps its link. `check-update` writes a one-row file; `completions` writes none |
| CT-log text | Certificate fields are chosen by whoever got the certificate logged, so both the table and the CSV neutralise them: a cell a spreadsheet would evaluate as a formula is prefixed with `'`, and bidirectional overrides are written as `\u{202e}` rather than silently reordering the row. A leading `-` is quoted only when the field does not parse as a finite number, so a negative `days_left` is never touched while `-inf` is |
| `completions` | Emits a shell script rather than a record, so it ignores both `--json` and `--csv` and never creates the CSV destination |
| `days_left` | Floored: negative once expired, `0` only within the last 24 hours before expiry |
| Table width | `--width <cols>` is met exactly, narrowing *or* widening. Without it: the terminal width, or 120 columns when stdout is a pipe |
| Exit codes | `0` completed, even with no results · `1` failed · `3` no certificate with that crt.sh ID. `2` is clap's usage error, so a malformed command never looks like a missing certificate |

Left to size itself, the table keeps atomic columns — an ID, a hex serial, a
timestamp — off the wrapping list, and holds a floor under the free-text ones so
they cannot be squeezed to a character per line. An explicit `--width` is an
instruction rather than a hint, so it overrides both and is honoured exactly.

Multi-valued columns (matched identities, SANs) reach CSV as one row per value
rather than joined into a field, so the file stays round-trippable. CSV is a
machine format rather than a picture of the table: an empty field means NULL
(the table's `-` is display only), timestamps are full RFC 3339 instants with
seconds, and `days_left` is therefore a genuine integer column. A field that
would begin `=`, `+`, `@`, tab or carriage return is prefixed with `'` so a
spreadsheet treats it as text — certificate subjects come from a public log and
are chosen by whoever got the certificate issued. Negative numbers are never
prefixed. While a query is in flight, `querying crt.sh:5432 for "example.com"…` goes to stderr —
suppressed when stderr is not a terminal, so scheduled runs keep clean logs.
Piping into `head`, or quitting `less` early, ends output cleanly.

## Configuration

Connection settings can live in a config file instead of being repeated on every
run: `$XDG_CONFIG_HOME/crt-query/config.toml` (falling back to `~/.config/…`),
or `%APPDATA%\crt-query\config.toml` on Windows.

```toml
# Every key is optional; anything omitted keeps its built-in default.
host = "crt.sh"
port = 5432
dbname = "certwatch"
user = "guest"

# Or set the connection in one go. A db_url overrides the four keys above,
# exactly as --db-url overrides the individual flags.
# db_url = "postgresql://guest@crt.sh:5432/certwatch"
```

Precedence, highest first: command-line flag, config file, built-in default. A
missing file is fine. A file that exists but does not parse is an error — an
unknown or misspelled key fails the run rather than being ignored, so a typo
cannot quietly leave you querying somewhere you did not intend.

Only the connection is configurable. Output format, limits and windows stay on
the command line, where the run that produced a report also records how.

## Staying current

`crt-query check-update` reports whether a newer release exists, and exits `0`
either way — being out of date is a report, not a failure. It is the only
subcommand that contacts anything other than crt.sh, and only when you ask;
nothing checks in the background. It shells out to the system `curl`, so that
has to be on `PATH` — the only path in this tool that runs an external program.
On Windows the search is Rust's, not `PATH` alone: it looks in the directory
holding `crt-query.exe` before `System32` and before `PATH`. `--json` gives `current`, `latest`,
`update_available` and `release_url` for a scheduled check.

To upgrade, re-run whatever you installed with: `brew upgrade crt-query`,
`install.sh`, `install.ps1`, or `cargo install --locked --git … --force`.

There is deliberately no `crt-query self-update`. A tool that downloads and
executes replacement code hands a compromised release channel a free upgrade to
code execution on every machine that runs it; re-running an installer that
verifies what it fetches costs one line and does not.

## Shell completions

```sh
crt-query completions zsh  > ~/.zfunc/_crt-query
crt-query completions bash > /usr/local/etc/bash_completion.d/crt-query
crt-query completions fish > ~/.config/fish/completions/crt-query.fish
```

`powershell` and `elvish` work too. Homebrew installs the bash, zsh and fish
scripts for you.

## Notes on the data

**Deduplication.** crt.sh stores the precertificate and the final leaf
certificate as separate rows, and identity searches return one row per matched
identity. By default the tool groups rows by certificate and collapses
precert/leaf pairs sharing `(issuer CA, serial)` — RFC 6962 requires both to
carry the same serial — keeping the lowest crt.sh ID. A pair is only collapsed
when its validity windows agree too, so a serial collision between genuinely
different certificates keeps both — unless the colliding certificates happen to
share a validity window as well, which the tool cannot tell apart from a real
precert/leaf pair. Like the dedup itself, this is best-effort. `--no-dedupe`
shows the raw rows.

**It is a shared public service.** crt.sh enforces statement timeouts and
connection limits, and is intermittently slow or unreachable. The tool retries
connections (not queries), keeps every query on the full-text index, and never
sorts server-side; sorting and dedup happen client-side over at most `--limit`
rows. Dedup is best-effort within that window — a precert/leaf pair straddling
the limit boundary may both appear.

**Wildcards.** `%` and `_` in a search term act as SQL wildcards in the identity
match, mirroring the crt.sh website's behaviour.

## Build

Requires Rust 1.98+ (pinned via `rust-toolchain.toml`) and
[`just`](https://just.systems).

```sh
just build-release   # binary lands in target/release/crt-query
just verify          # fmt-check · lint · test · msrv · lint-scripts · build — offline
just verify-full     # adds cargo-audit + cargo-deny (needs network)
```

`just --list` shows every recipe. `lint-scripts` covers `install.sh` and
`install.ps1`; it needs `shellcheck` and `pwsh` on PATH.

Every test is offline and never contacts crt.sh — it is a shared public service
on donated infrastructure, and a test suite pointed at it would be both flaky
and rude.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). It covers the setup, the gates, and the
constraints behind the SQL worth reading before changing a query. Participation
is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Report vulnerabilities privately — see [SECURITY.md](.github/SECURITY.md). Do
not open a public issue.

GitHub Actions are pinned to full commit SHAs, dependencies are locked by version
and checksum, and `cargo-audit` plus `cargo-deny` run on every PR and weekly.
Dependabot keeps the pins moving.

## License

[Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.

# crt-query

> Query [crt.sh](https://crt.sh) certificate-transparency data straight from its
> public PostgreSQL database — no HTTP API, no scraping, no API key.

[![CI](https://github.com/tiredithumans/crt-query/actions/workflows/ci.yml/badge.svg)](https://github.com/tiredithumans/crt-query/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.98-orange.svg)](./rust-toolchain.toml)
[![Platform](https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey.svg)](#install)

`crt-query` connects to `crt.sh:5432` (database `certwatch`, read-only `guest`
access) and answers three questions from the command line: what certificates
exist for a name, what is in one certificate, and what is about to expire.
Output is a table, JSON, or CSV.

## Table of contents

- [Install](#install)
- [Upgrade](#upgrade)
- [Usage](#usage)
- [Configuration](#configuration)
- [Shell completions](#shell-completions)
- [The search window](#the-search-window)
- [Output](#output)
- [Exit codes](#exit-codes)
- [Deduplication](#deduplication)
- [Caveats](#caveats)
- [Build](#build)
- [Contributing](#contributing)
- [Security](#security)
- [License](#license)

## Install

Every release ships prebuilt archives for `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`, `x86_64-apple-darwin` and `x86_64-pc-windows-msvc`,
alongside a `SHA256SUMS` file covering all of them. Every route below that
installs a prebuilt binary verifies that checksum first.

### Linux and macOS

```sh
curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh | sh
```

The script detects your target triple, resolves the newest release — there is
no version to substitute here or keep current — verifies the archive against
that release's `SHA256SUMS`, installs to `/usr/local/bin`, and on macOS clears
the Gatekeeper quarantine attribute that an unsigned binary arrives with.
Re-run it to [upgrade](#upgrade).

Piping a script into a shell is a decision, not a default. To read it first:

```sh
curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh -o install.sh
less install.sh
sh install.sh
```

Options go after `sh -s --` when piping — a directory that needs no `sudo`, or
a pinned release:

```sh
curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh \
  | sh -s -- --dir "$HOME/.local/bin"

curl -fsSL https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.sh \
  | sh -s -- --version v0.1.0
```

### Homebrew (macOS and Linux)

```sh
brew install tiredithumans/tap/crt-query
```

`brew upgrade` then finds new versions on its own, and Homebrew strips the
macOS quarantine attribute from what it downloads, so there is no `xattr` step
on this path.

### Windows

In PowerShell:

```powershell
irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1 | iex
```

Installs to `%LOCALAPPDATA%\Programs\crt-query` and adds it to your user
`PATH` — no admin rights needed. Open a new terminal for the updated `PATH` to
take effect.

To pass options, the script has to become a scriptblock first:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/tiredithumans/crt-query/main/install.ps1))) -Dir C:\tools
```

`-Dir <path>` chooses the install directory, `-Version v0.1.0` pins a release,
and `-NoPathUpdate` leaves your `PATH` alone.

### Manual download

Pick a release from the [releases page](https://github.com/tiredithumans/crt-query/releases),
then — substituting the version and the target triple for your machine
(`aarch64-apple-darwin` for Apple Silicon, `x86_64-apple-darwin` for Intel):

```sh
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-gnu
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/crt-query-$VERSION-$TARGET.tar.gz"
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/SHA256SUMS"

sha256sum --ignore-missing -c SHA256SUMS   # macOS: shasum -a 256 --ignore-missing -c SHA256SUMS

tar -xzf "crt-query-$VERSION-$TARGET.tar.gz"
sudo install -m 0755 "crt-query-$VERSION-$TARGET/crt-query" /usr/local/bin/
```

On macOS the binaries are unsigned, so Gatekeeper quarantines them. Clear the
attribute after installing — and again after every upgrade, because a fresh
download arrives quarantined too:

```sh
xattr -d com.apple.quarantine /usr/local/bin/crt-query
```

### From source

Any OS with Rust 1.98+ (see `rust-toolchain.toml`):

```sh
cargo install --git https://github.com/tiredithumans/crt-query
```

## Upgrade

`crt-query check-update` reports whether a newer release exists:

```console
$ crt-query check-update
crt-query 0.2.0 is available (running 0.1.0): https://github.com/tiredithumans/crt-query/releases/tag/v0.2.0
```

It is the only subcommand that contacts anything other than crt.sh, and only
when you ask: nothing checks for updates in the background, on a timer, or as a
side effect of a query. `--json` gives
`{"current", "latest", "update_available", "release_url"}` for a scheduled
check. The exit code is `0` either way — being out of date is a report, not a
failure.

How to actually upgrade depends on how you installed:

| Installed with | Upgrade with |
| --- | --- |
| `install.sh` | Re-run it. It resolves the newest release, verifies `SHA256SUMS`, replaces the binary, and re-clears the macOS quarantine attribute. |
| `install.ps1` | Re-run it. Same, and it clears the new download's mark-of-the-web. |
| Homebrew | `brew update && brew upgrade crt-query` |
| Manual download | Repeat the download steps for the new version, **including the checksum check**. |
| From source | `cargo install --git https://github.com/tiredithumans/crt-query --force` |

Two things that catch people out when upgrading by hand on macOS:

- The quarantine attribute comes back with **every** download, not just the
  first, so `xattr -d com.apple.quarantine /usr/local/bin/crt-query` is part of
  each manual upgrade. `install.sh` and Homebrew do it for you.
- `SHA256SUMS` covers one release. Verifying a new archive against the previous
  release's file fails, and should — delete the stale one rather than adding
  flags until it stops complaining.

There is deliberately no `crt-query self-update`. A tool that downloads and
executes replacement code hands a compromised release channel a free upgrade to
code execution on every machine that runs it; re-running an installer that
verifies what it fetches costs one line and does not.

## Usage

```sh
# Search certificates by domain or identity (crt.sh-style)
crt-query search example.com --limit 100

# Full details for one certificate by crt.sh ID
crt-query cert 984858191

# Certificates expired or expiring within N days, sorted by expiry
crt-query expiring example.com --within 30 --skip-expired

# Several domains at once: one report, sorted by expiry across all of them
crt-query expiring example.com example.org example.net --within 30

# Is there a newer release? (the only subcommand that talks to GitHub)
crt-query check-update
```

`expiring` takes any number of domains. They are queried one at a time — the
guest database's statement timeout is what rules out folding them into a single
query — so `--limit` applies per domain, and a certificate covering two of them
appears once, carrying both matched identities.

Output is a table by default. `--json` emits JSON to stdout instead;
`--csv <path>` additionally writes a CSV file. Both are global flags and work
with every subcommand:

```sh
crt-query search example.com --json | jq '.[].id'
crt-query expiring example.com example.org --within 90 --csv report.csv
```

Connection overrides: `--host`, `--port`, `--dbname`, `--user`, or a full
`--db-url postgresql://...`. Repeating them on every run is what the
[config file](#configuration) is for.

## Configuration

Connection settings can live in a config file instead:

- Linux and macOS: `$XDG_CONFIG_HOME/crt-query/config.toml`, falling back to
  `~/.config/crt-query/config.toml`
- Windows: `%APPDATA%\crt-query\config.toml`

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

Precedence, highest first: command-line flag, config file, built-in default.

A missing file is not an error — it just means every setting comes from flags
and defaults. A file that exists but does not parse *is* an error: an unknown
or misspelled key fails the run instead of being ignored, so a typo cannot
quietly leave you querying somewhere you did not intend.

Nothing else is configurable there. Output format, limits and windows stay on
the command line, where the run that produced a report also records how.

## Shell completions

`crt-query completions <shell>` writes a completion script to stdout for
`bash`, `zsh`, `fish`, `powershell` or `elvish`. Where it belongs depends on
your setup; the usual answers:

```sh
# zsh — any directory on your $fpath
crt-query completions zsh > ~/.zfunc/_crt-query

# bash
crt-query completions bash > /usr/local/etc/bash_completion.d/crt-query

# fish
crt-query completions fish > ~/.config/fish/completions/crt-query.fish
```

```powershell
# PowerShell — add to your profile
crt-query completions powershell | Out-String | Invoke-Expression
```

Homebrew installs the bash, zsh and fish scripts for you.

## The search window

The guest database enforces a statement timeout, so every query must stay on
the full-text index and let `LIMIT` terminate early — which rules out a
server-side `ORDER BY`. `LIMIT` therefore takes an *arbitrary* slice of the
matching rows, in practice the oldest ones. Both search commands bound that
slice with a validity predicate so it lands on certificates you care about:

| Flag | Default | Meaning |
| --- | --- | --- |
| `search --valid-since <days>` | 365 | Only certificates still valid within this many days of now |
| `search --all-history` | off | No validity floor; search everything crt.sh holds |
| `expiring --within <days>` | 30 | Look-ahead: certificates expiring within this window |
| `expiring --since-expired <days>` | 30 | Look-back: how far back to include already-expired certificates |
| `expiring --skip-expired` | off | Exclude expired certificates entirely (same as `--since-expired 0`) |

Without the look-back bound, `expiring` matches every certificate that ever
expired, and the `LIMIT` window fills with rows from years ago before reaching
anything close to renewal. Widen `--since-expired` to look further back.

## Output

- Timestamps are UTC everywhere. JSON carries an explicit `Z`; table columns
  are labelled `(UTC)`.
- `--json` always emits valid JSON, including `[]` for an empty result and
  `null` for a `cert` ID that does not exist.
- `--csv` always writes the file, including a header-only file for an empty
  result — so a scheduled job never silently re-reads the previous run's
  report. Multi-valued columns (matched identities, SANs) are written as one
  row per value rather than joined into a single field, so the file is
  round-trippable.
- `days_left` is floored: negative once a certificate has expired, and `0` only
  for one expiring within the next 24 hours.
- Table width follows the terminal, or falls back to 120 columns when stdout is
  a pipe. Override with `--width <cols>`.
- While a query is in flight, `querying crt.sh:5432 for "example.com"…` goes to
  stderr, so a slow answer from the shared database is distinguishable from a
  hang — and `expiring` over several domains shows how far along it is. It is
  suppressed when stderr is not a terminal, so scheduled runs keep clean logs,
  and it is on stderr so it never lands in a piped table or JSON document.
- Piping into `head` or quitting `less` early ends output cleanly rather than
  reporting a broken pipe.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Completed; results emitted, even if there were none |
| 1 | The run failed |
| 2 | `cert` — no certificate with that crt.sh ID |

`check-update` exits `0` whether or not you are behind; read `update_available`
from its `--json` output to branch on it.

## Deduplication

crt.sh stores the precertificate and the final leaf certificate as separate
rows, and identity searches return one row per matched identity. By default
the tool groups rows by certificate and collapses precert/leaf pairs sharing
`(issuer CA, serial)` — RFC 6962 requires both to carry the same serial —
keeping the lowest crt.sh ID. `--no-dedupe` shows the raw rows.

A pair is only collapsed when its validity windows also agree. RFC 6962 gives
a precertificate and its leaf the same `notBefore`/`notAfter`, so a pair that
disagrees is a serial collision between genuinely different certificates and
both are kept.

## Caveats

- The guest database is a shared public service: it enforces statement
  timeouts and connection limits, and is intermittently slow or unreachable.
  The tool retries connections (not queries), keeps every query on the
  full-text index, and never sorts server-side; sorting and dedup happen
  client-side over at most `--limit` rows.
- `--limit` bounds raw identity rows server-side, so the deduplicated
  certificate count shown may be lower. Dedup is best-effort within that
  window: a precert/leaf pair straddling the limit boundary may both appear.
- `%` and `_` in a search term act as SQL wildcards in the identity match,
  mirroring the crt.sh website's behavior.
- crt.sh sits behind a transaction-pooling pgbouncer, so the tool uses
  unnamed prepared statements (`query_typed`); named prepared statements
  fail there.

## Build

Requires Rust 1.98+, pinned via `rust-toolchain.toml`, and
[`just`](https://just.systems) as the task runner.

```sh
just build-release
target/release/crt-query --help
```

`just --list` shows every recipe. The gates CI runs:

```sh
just verify        # fmt-check · lint · test · msrv · lint-scripts — all offline
just verify-full   # adds cargo-audit + cargo-deny (needs network)
```

`lint-scripts` covers `install.sh` and `install.ps1`; it needs `shellcheck` and
`pwsh` on PATH.

Every test is offline and never contacts crt.sh — it is a shared public service
running on donated infrastructure, and a test suite pointed at it would be both
flaky and rude.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). It covers the setup, the gates, and the
constraints behind the SQL that are worth reading before changing a query.

Participation is governed by the [Code of Conduct](CODE_OF_CONDUCT.md).

## Security

Report vulnerabilities privately — see [SECURITY.md](.github/SECURITY.md).
Do not open a public issue.

Supply-chain posture: GitHub Actions are pinned to full commit SHAs,
dependencies are locked by version and checksum, and `cargo-audit` (RustSec
advisories) plus `cargo-deny` (licence policy, crate sources, banned crates) run
on every PR and weekly. Dependabot keeps the pins moving.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

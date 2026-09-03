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
- [Usage](#usage)
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

Prebuilt archives are on the [latest release](https://github.com/tiredithumans/crt-query/releases/latest)
for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`
and `x86_64-pc-windows-msvc`. Each comes with a `SHA256SUMS` file covering
every archive in the release — always verify before installing.

### Linux

```sh
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-gnu
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/crt-query-$VERSION-$TARGET.tar.gz"
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf "crt-query-$VERSION-$TARGET.tar.gz"
sudo install -m 0755 "crt-query-$VERSION-$TARGET/crt-query" /usr/local/bin/
```

### macOS

Pick the target for your chip: `aarch64-apple-darwin` for Apple Silicon (M-series),
`x86_64-apple-darwin` for Intel.

```sh
VERSION=v0.1.0
TARGET=aarch64-apple-darwin   # Intel: x86_64-apple-darwin
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/crt-query-$VERSION-$TARGET.tar.gz"
curl -LO "https://github.com/tiredithumans/crt-query/releases/download/$VERSION/SHA256SUMS"
shasum -a 256 --ignore-missing -c SHA256SUMS
tar -xzf "crt-query-$VERSION-$TARGET.tar.gz"
sudo install -m 0755 "crt-query-$VERSION-$TARGET/crt-query" /usr/local/bin/
```

The binaries are unsigned, so Gatekeeper quarantines them on first run. Clear
the quarantine attribute once, after installing:

```sh
xattr -d com.apple.quarantine /usr/local/bin/crt-query
```

### Windows

Run in PowerShell:

```powershell
$Version = "v0.1.0"
$Target  = "x86_64-pc-windows-msvc"
Invoke-WebRequest -Uri "https://github.com/tiredithumans/crt-query/releases/download/$Version/crt-query-$Version-$Target.zip" -OutFile "crt-query.zip"
Invoke-WebRequest -Uri "https://github.com/tiredithumans/crt-query/releases/download/$Version/SHA256SUMS" -OutFile "SHA256SUMS"

$expected = ((Select-String -Path SHA256SUMS -Pattern ([regex]::Escape("crt-query-$Version-$Target.zip"))).Line -split '\s+')[0]
$actual   = (Get-FileHash crt-query.zip -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) { throw "checksum mismatch: expected $expected, got $actual" }

Expand-Archive crt-query.zip -DestinationPath . -Force
$InstallDir = "$env:LOCALAPPDATA\Programs\crt-query"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
Move-Item "crt-query-$Version-$Target\crt-query.exe" "$InstallDir\crt-query.exe" -Force
Remove-Item "crt-query-$Version-$Target" -Recurse

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($userPath -notlike "*$InstallDir*") {
    $newPath = if ($userPath) { "$userPath;$InstallDir" } else { $InstallDir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
}
```

Installs to `%LOCALAPPDATA%\Programs\crt-query` and adds it to your user
`PATH` — no admin rights needed. Open a new terminal window for the updated
`PATH` to take effect.

### From source

Any OS with Rust 1.98+ (see `rust-toolchain.toml`):

```sh
cargo install --git https://github.com/tiredithumans/crt-query
```

## Usage

```sh
# Search certificates by domain or identity (crt.sh-style)
crt-query search example.com --limit 100

# Full details for one certificate by crt.sh ID
crt-query cert 984858191

# Certificates expired or expiring within N days, sorted by expiry
crt-query expiring example.com --within 30 --skip-expired
```

Output is a table by default. `--json` emits JSON to stdout instead;
`--csv <path>` additionally writes a CSV file. Both are global flags and work
with every subcommand:

```sh
crt-query search example.com --json | jq '.[].id'
crt-query expiring example.com --within 90 --csv report.csv
```

Connection overrides: `--host`, `--port`, `--dbname`, `--user`, or a full
`--db-url postgresql://...`.

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
- Piping into `head` or quitting `less` early ends output cleanly rather than
  reporting a broken pipe.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Completed; results emitted, even if there were none |
| 1 | The run failed |
| 2 | `cert` — no certificate with that crt.sh ID |

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
just verify        # fmt-check · lint · test · msrv — all offline
just verify-full   # adds cargo-audit + cargo-deny (needs network)
```

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

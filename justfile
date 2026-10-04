# crt-query task runner. `just` with no arguments lists every recipe.
#
# CI and a contributor's laptop run the SAME commands: every recipe CI runs,
# it runs verbatim. The reverse does not hold — `actionlint` and CodeQL's
# Analyze have no local counterpart — so `just verify-full` is strong evidence
# rather than a guarantee (current list in the note above `verify-full`).

default:
    @just --list

# --- Build & run -----------------------------------------------------------

# Debug build.
build:
    cargo build --locked

# Optimised build; the binary lands in target/release/crt-query.
build-release:
    cargo build --locked --release

# The release ships static musl archives; without this the tag push would be
# their first compile — the same trap `build-release` exists to close for the
# release profile. Builds for this machine's own CPU: the host's cc drives the
# link and cannot link for another architecture. No musl-tools needed — rustc
# carries musl's CRT objects and libc.a, and nothing in the tree compiles C on
# Linux. Linux-only, so absent elsewhere.
# Static musl release build; the binary lands in target/<cpu>-unknown-linux-musl/release/crt-query.
[linux]
build-musl:
    rustup target add {{ arch() }}-unknown-linux-musl
    cargo build --locked --release --target {{ arch() }}-unknown-linux-musl

# Run the CLI, e.g. `just run search example.com --limit 20`.
run *ARGS:
    cargo run --locked -- {{ARGS}}

# --- CI gates --------------------------------------------------------------
# Each recipe below is one job step in ci.yml. Keep them in lockstep.

# Rewrite formatting in place (not a gate; `fmt-check` is).
fmt:
    cargo fmt --all

# Formatting gate.
fmt-check:
    cargo fmt --all -- --check

# Lint gate. --all-targets covers tests and benches, not just the binary.
lint:
    cargo clippy --locked --all-targets -- -D warnings

# Test gate (offline: crt.sh is shared, so no test ever contacts it).
test:
    cargo test --locked

# Replaced a `build` gate that only repeated `test` (whose integration tests
# build the binary anyway — see the Docs step in ci.yml). A broken intra-doc
# link is a reference nobody can follow, and nothing checked for one until
# this. --document-private-items because this is a binary crate: almost nothing
# in it is public, so without the flag rustdoc skips nearly every comment.
# Docs gate: rustdoc with every warning, broken intra-doc links included, an error.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --document-private-items

# Fast inner-loop type check; fails in seconds where `verify` takes minutes.
check:
    cargo check --locked --all-targets

# They are piped into a shell by people who have not read them, so they get a
# gate like everything else.
# Lint gate for install.sh, install.ps1 and packaging/homebrew/generate.sh.
lint-scripts:
    #!/usr/bin/env bash
    set -euo pipefail
    command -v shellcheck >/dev/null || {
        echo "shellcheck not found (brew install shellcheck / apt install shellcheck)" >&2
        exit 1
    }
    # install.sh must stay POSIX: it runs under whatever /bin/sh a machine has.
    shellcheck --shell=sh install.sh
    shellcheck packaging/homebrew/generate.sh
    command -v pwsh >/dev/null || {
        echo "pwsh not found (brew install powershell / see aka.ms/powershell)" >&2
        exit 1
    }
    # A parse check, not a full analysis: install.ps1 cannot be exercised on a
    # non-Windows machine, so the thing worth catching here is a syntax error
    # that would only surface on someone's first install.
    pwsh -NoProfile -Command '
        $errs = $null
        [void][System.Management.Automation.Language.Parser]::ParseFile(
            (Resolve-Path install.ps1), [ref]$null, [ref]$errs)
        if ($errs) { $errs | ForEach-Object { $_.ToString() }; exit 1 }
        Write-Host "install.ps1 parses"
    '

# MSRV gate: the version declared in Cargo.toml must actually build.
msrv:
    #!/usr/bin/env bash
    set -euo pipefail
    MSRV=$(sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)
    echo "declared MSRV: $MSRV"
    rustup toolchain install "$MSRV" --profile minimal >/dev/null 2>&1 || true
    RUSTUP_TOOLCHAIN="$MSRV" cargo check --locked --all-targets

# --- Dependency policy -----------------------------------------------------
# Both need network access, so `verify` leaves them out and `verify-full`
# includes them.

# RustSec advisory scan (config: .cargo/audit.toml).
audit:
    cargo audit

# License policy, crate-source and ban gating (config: deny.toml).
deny:
    cargo deny check

# --- Aggregates ------------------------------------------------------------

# Every offline CI gate, in CI order. Run this before opening a PR.
verify: fmt-check lint test msrv lint-scripts doc
    @echo ""
    @echo "verify OK — NOT run (needs network): audit, deny."
    @echo "  just verify-full adds the dependency gates"

# Not covered here: `actionlint` (CI installs it from a pinned tarball, not a
# recipe) and CodeQL's Analyze (GitHub only) — both required checks with no
# local counterpart. `build-release` and `build-musl` are CI steps too, left
# out for costing minutes on a profile nothing else here exercises; run them
# by hand when touching the release profile or the release targets.
#
# (`just --list` shows only the comment line directly above a recipe, which is
# why the summary sits last rather than first.)
# Full CI parity for what can run locally: every offline gate plus both dependency-policy scans.
verify-full: fmt-check lint test msrv lint-scripts doc audit deny
    @echo ""
    @echo "verify-full OK — the gates that can run locally all pass."
    @echo "  CI additionally runs: actionlint, CodeQL Analyze, build-release, build-musl."

# --- Release helpers -------------------------------------------------------

# Print the version declared in Cargo.toml.
version:
    @sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1

# The output is gitignored: release.yml's `tap` job generates the formula in CI
# and pushes it to the tap, never back here, so a committed copy only goes stale.
# Generate the Homebrew formula from a release (default: latest), for inspection. See packaging/homebrew/README.md.
homebrew-formula VERSION="":
    ./packaging/homebrew/generate.sh {{VERSION}}

# Everything the release skill checks before cutting a tag.
release-check: verify-full
    @echo ""
    @echo "version: $(just version)"
    @echo "changelog [Unreleased] section:"
    @sed -n '/## \[Unreleased\]/,/^## \[/p' CHANGELOG.md | head -20

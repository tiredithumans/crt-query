# crt-query task runner. `just` with no arguments lists every recipe.
#
# The point of this file is that CI and a contributor's laptop run the SAME
# commands: every gate below is invoked verbatim by .github/workflows/ci.yml,
# so `just verify` passing locally means the PR's required checks will pass.

default:
    @just --list

# --- Build & run -----------------------------------------------------------

# Debug build.
build:
    cargo build --locked

# Optimised build; the binary lands in target/release/crt-query.
build-release:
    cargo build --locked --release

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

# Fast inner-loop type check; fails in seconds where `verify` takes minutes.
check:
    cargo check --locked --all-targets

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
verify: fmt-check lint test msrv
    @echo ""
    @echo "verify OK — NOT run (needs network): audit, deny."
    @echo "  just verify-full = full CI parity (adds the dependency gates)"

# Full CI parity: the offline gates plus both dependency-policy scans.
verify-full: fmt-check lint test msrv audit deny
    @echo ""
    @echo "verify-full OK — every required check should pass on the PR."

# --- Release helpers -------------------------------------------------------

# Print the version declared in Cargo.toml.
version:
    @sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1

# Everything the release skill checks before cutting a tag.
release-check: verify-full
    @echo ""
    @echo "version: $(just version)"
    @echo "changelog [Unreleased] section:"
    @sed -n '/## \[Unreleased\]/,/^## \[/p' CHANGELOG.md | head -20

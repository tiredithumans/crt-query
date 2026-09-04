# Homebrew tap

`crt-query.rb` is a binary formula: it installs the archives a GitHub release
publishes, verified against the same `SHA256SUMS` a manual install would check.
It is generated — `just homebrew-formula` rewrites it from a release, so the
checksums cannot drift from the archives they point at.

Homebrew is worth carrying because it gives the macOS/Linux cohort an upgrade
path the release archives do not: `brew upgrade` finds new versions on its own,
where a downloaded binary has to be replaced by hand.

## Publishing a new version

`brew install tiredithumans/tap/crt-query` resolves to a **public** GitHub
repository named `tiredithumans/homebrew-tap`, with formulae under `Formula/`.
Homebrew cannot install from a private tap without per-user credentials, so the
tap repo has to be public before that command works for anyone else.

This is automated. Publishing a release fires `release: published`, which runs
the `tap` job in `.github/workflows/release.yml`: it regenerates the formula
from the published `SHA256SUMS`, checks it, and pushes it to the tap. Nothing to
copy by hand.

It needs a `TAP_TOKEN` secret on the crt-query repo — a fine-grained PAT with
Contents: read and write on `tiredithumans/homebrew-tap`. Without it the job
fails on its first step and the tap is left untouched.

Pre-releases are deliberately skipped. `release: published` fires for them, but
one `Formula/crt-query.rb` is what every `brew upgrade` follows, and every other
install path already ignores pre-releases by resolving `/releases/latest`.

Before it pushes, the job checks that every checksum in the formula matches the
published `SHA256SUMS`, that the formula is valid Ruby, that the archive's build
provenance verifies, and — the one that actually breaks installs — that the
released binary answers every call the formula makes, since Homebrew runs the
binary it installs both in `generate_completions_from_executable` and in
`test do`.

To re-sync without cutting a release:

```sh
gh workflow run release.yml -f tap_only=true    # syncs from the latest release
```

### By hand, if the automation is broken

```sh
just homebrew-formula              # or: just homebrew-formula v0.2.0
```

Then, in a checkout of the tap:

```sh
cp .../crt-query/packaging/homebrew/crt-query.rb Formula/crt-query.rb
brew audit --strict --online tiredithumans/tap/crt-query   # needs the tap tapped
git commit -am "crt-query X.Y.Z" && git push
```

`brew audit` only runs against a formula inside a tap, so it is neither one of
this repo's gates nor part of the `tap` job — which syntax-checks the formula
with `ruby -c` instead. Running the audit by hand is still worth it after a
change to `generate.sh`.

## Checking it before publishing

```sh
brew tap tiredithumans/tap
brew install --formula tiredithumans/tap/crt-query
brew test crt-query
crt-query --version
```

The formula's own `test do` block is deliberately offline: crt.sh is a shared
public service on donated infrastructure, and a formula test must not depend on
it being up.

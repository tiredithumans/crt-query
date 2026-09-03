# Homebrew tap

`crt-query.rb` is a binary formula: it installs the archives a GitHub release
publishes, verified against the same `SHA256SUMS` a manual install would check.
It is generated — `just homebrew-formula` rewrites it from a release, so the
checksums cannot drift from the archives they point at.

Homebrew is worth carrying because it gives the macOS/Linux cohort an upgrade
path the release archives do not: `brew upgrade` finds new versions on its own,
where a downloaded binary has to be replaced by hand. Homebrew also strips the
quarantine attribute from what it downloads, so this install path skips the
`xattr` step the README describes for a manual install.

## Publishing a new version

`brew install tiredithumans/tap/crt-query` resolves to a **public** GitHub
repository named `tiredithumans/homebrew-tap`, with formulae under `Formula/`.
Homebrew cannot install from a private tap without per-user credentials, so the
tap repo has to be public before that command works for anyone else.

After a release is published:

```sh
just homebrew-formula              # or: just homebrew-formula v0.2.0
```

Then, in a checkout of the tap:

```sh
cp .../crt-query/packaging/homebrew/crt-query.rb Formula/crt-query.rb
brew audit --strict --online tiredithumans/tap/crt-query   # needs the tap tapped
git commit -am "crt-query X.Y.Z" && git push
```

`brew audit` only runs against a formula inside a tap, which is why it is not
one of this repo's gates.

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

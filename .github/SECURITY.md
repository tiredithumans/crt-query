# Security Policy

## Supported versions

This project is pre-1.0. Only the latest release receives fixes.

## Reporting a vulnerability

Please report security issues **privately**, not as a public GitHub issue.

Use [GitHub's private vulnerability reporting](https://github.com/tiredithumans/crt-query/security/advisories/new)
on this repository. You should get an acknowledgement within a few days.

Please include what you need to reproduce it: the command line, the observed
behaviour, and the version (`crt-query --version`).

## Scope

crt-query is a read-only client for a public database. It sends no credentials
by default, writes no state beyond files you ask for with `--csv`, and the
`guest` account it uses is public and read-only.

Things that are in scope:

- Anything that could execute code or write outside a path the user specified.
- Credential disclosure through `--db-url` when pointing the tool at a private
  database — for example a password reaching stderr, a log, or an error message.
- Injection into the SQL sent to the server. Every user value is bound as a
  typed parameter; a way to break out of that is a real finding.

Things that are **not** vulnerabilities:

- The default `NoTls` connection. crt.sh's guest account is passwordless and
  serves public certificate-transparency data, so there is nothing to protect in
  transit. This is documented in `src/db.rs`. Pointing `--db-url` at a private
  database over an untrusted network is a different situation — see above.
- `%` and `_` behaving as SQL `LIKE` wildcards in a search term. That mirrors
  the crt.sh website and is documented behaviour, not injection.

## Dependencies

`cargo-audit` (RustSec advisories) and `cargo-deny` (licenses, crate sources,
banned crates) run on every PR and on a weekly schedule. Dependabot proposes
updates for both Cargo crates and the SHA-pinned GitHub Actions.

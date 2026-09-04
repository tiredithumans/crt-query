---
name: release
description: Cut and publish a release — bump the version, sync the lockfile, finalize CHANGELOG.md [Unreleased] → [X.Y.Z], release PR, tag the merge commit, verify the draft's binaries and checksums, publish on human sign-off. Use when the user says "release", "cut a release", "bump version", or asks to publish a new version.
argument-hint: "[bump type: patch, minor, major]"
---

# Release — bump → lockfile → changelog → PR → tag → draft → publish

The pipeline: release PR onto `main` → annotated `vX.Y.Z` tag on the **merge commit** →
`.github/workflows/release.yml` builds five target binaries and assembles a **draft** release with
`SHA256SUMS` → a human publishes it. Publishing is the only step that reaches users.

`main` is protected — never commit to it directly; everything lands via the release PR.

## 0. Determine the bump type

Use the argument if given. Otherwise read the commits since the last tag
(`git log $(git describe --tags --abbrev=0)..HEAD --oneline`):

- breaking change (`!` type, or a `Changed`/`Removed` changelog note that breaks a contract) → major
- any `feat:` → minor
- only `fix:` / `chore:` / `docs:` / `deps:` / `ci:` → patch

Pre-1.0 note: while the version is `0.x`, a breaking change bumps the **minor**.

## 1. Bump the version

One manifest, one place: `version` under `[package]` in `Cargo.toml`. The `guard` job in
`release.yml` fails the whole build if the tag and this value disagree.

## 2. Sync the lockfile

Every gate runs `--locked`, so a stale lockfile fails CI right after a version bump:

```sh
cargo update --workspace   # touches only the crt-query version line, no dep churn
```

Do not run a full `cargo build` just to refresh the lock.

## 3. Finalize the changelog

- Roll `## [Unreleased]` into `## [X.Y.Z] - YYYY-MM-DD` — **no `v` prefix, ASCII hyphen**
  (e.g. `## [0.2.0] - 2026-09-02`). This exact shape is load-bearing twice over: the `guard` job
  greps for it and fails without it, and the `draft` job extracts the release notes by matching it.
- Add a fresh empty `## [Unreleased]` stub above the new section.
- Update the link definitions at the bottom of the file.

## 4. Verify, commit, PR

```sh
just verify-full                       # full CI parity, incl. audit + deny
git checkout -b release/vX.Y.Z
git commit -m "chore: release vX.Y.Z"
git push -u origin release/vX.Y.Z
gh pr create --base main --title "chore: release vX.Y.Z" --body …
```

Run `verify-full`, not `verify`: the release re-runs the RustSec scan against this exact lockfile,
so catching a fresh advisory locally saves a whole cut cycle.

## 5. Wait for the required checks, merge

Watch with `gh pr checks <num> --watch`, or poll `gh pr view <num> --json statusCheckRollup` in a
background loop (~45s apart). If the branch goes behind, `gh pr update-branch <num>` and re-wait.

`gh pr merge <num> --squash --delete-branch`. Never `--admin`.

## 6. Tag the merge commit

```sh
git checkout main && git pull origin main
git tag -a vX.Y.Z -m "vX.Y.Z" <merge-sha>   # the MERGE COMMIT on main, not the branch head
git push origin vX.Y.Z
```

The tag push triggers `release.yml`: `guard` (tag ↔ manifest ↔ changelog) → a 5-target build matrix
→ a **draft** release.

## 7. Verify the draft

```sh
gh release view vX.Y.Z --json isDraft,assets
```

Expect six assets — the count follows the build matrix in `release.yml`, so update this list when
that changes:

- `crt-query-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz`
- `crt-query-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz`
- `crt-query-vX.Y.Z-aarch64-apple-darwin.tar.gz`
- `crt-query-vX.Y.Z-x86_64-apple-darwin.tar.gz`
- `crt-query-vX.Y.Z-x86_64-pc-windows-msvc.zip`
- `SHA256SUMS`

Sanity-check the notes rendered on the draft against `CHANGELOG.md`, and confirm `SHA256SUMS` lists
every archive. Download one archive and run the binary's `--version` if anything looks off.

Since v0.4.0 every archive also carries a build-provenance attestation. Check one:

```sh
gh attestation verify <archive> --repo tiredithumans/crt-query
```

It prints nothing when stdout is not a terminal, so read the **exit code**, not the output. If you
want to be sure the check is live rather than vacuous, run it against a wrong `--repo` and confirm
that fails.

## 8. Publish — ONLY on explicit human instruction

```sh
gh release edit vX.Y.Z --draft=false --latest
```

Never `gh release create`: the workflow already made the draft, and a second release would bypass
the assembled assets and checksums.

## 9. Regenerate the Homebrew formula

`generate.sh` copies every checksum out of the release's own `SHA256SUMS`, fetched over the public
download URL — which serves nothing for a draft. So this runs **after** publishing, never before:

```sh
just homebrew-formula        # defaults to the latest release
```

Then copy `packaging/homebrew/crt-query.rb` into the tap repository as `Formula/crt-query.rb`. Until
that lands, `brew install` still serves the previous version.

Homebrew runs the installed binary at install time (`generate_completions_from_executable`) and again
in `test do`, so the formula must only call subcommands that exist in the release it points at —
never one that only exists on `main`.

## Re-cut pattern (broken draft, not yet published)

1. Fix on a **new** branch; PR + merge as usual (steps 4–5).
2. Delete the stale draft and tag:
   `gh release delete vX.Y.Z --yes && git push origin :refs/tags/vX.Y.Z && git tag -d vX.Y.Z`
3. Re-tag the new merge commit (step 6). Reusing the version number is fine — the first draft never
   published.

Never re-point a pushed tag with a force-push.

## Output format

```
release: v0.2.0 (minor)

✅ version bumped (Cargo.toml) · lockfile synced
✅ CHANGELOG [Unreleased] → [0.2.0] - 2026-09-02
✅ verify-full green · PR #NN merged · tagged v0.2.0 on <merge-sha>
✅ draft verified: 4 archives + SHA256SUMS, notes match the changelog

⏸ awaiting human sign-off, then:
   gh release edit v0.2.0 --draft=false --latest
```

## Failure handling

- `just verify-full` fails → stop and report the failing gate.
- `guard` fails (tag/manifest/changelog drift) → fix on a new branch, then use the re-cut pattern.
- A build leg fails → the draft is incomplete; re-cut rather than publishing a partial asset set.
- Tag already exists → re-cut pattern; never overwrite silently.

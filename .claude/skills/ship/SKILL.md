---
name: ship
description: Land the current working-tree changes on main — branch, conventional commits, push, PR, wait on CI, merge, branch cleanup. Use when the user says "ship", "ship this/it", "land this", or asks for the full commit → PR → merge flow.
argument-hint: "[optional scope hint or PR title]"
---

# Ship — commit → push → PR → merge → cleanup

Land the current changes on `main` using this repo's flow, end to end, without stopping to ask
between steps. Treat any arguments as guidance (what to include, a scope hint, or the PR title).
Stop and ask only if the working tree mixes clearly unrelated work and the split is ambiguous, or
if a gate fails.

## 0. Preflight

- `git status --short` and `git log origin/main..HEAD --oneline`. Nothing to commit **and** nothing
  unpushed → report "nothing to ship" and stop.
- **Gate check:** if anything under `src/`, `Cargo.toml` or `Cargo.lock` changed, run `just verify`
  (fmt-check · lint · test · msrv — every offline CI gate, in CI order) and stop on failure. If
  dependencies changed, run `just verify-full` instead: it adds `audit` and `deny`, the two gates
  that need network and that a dependency bump is most likely to break.
  Changes limited to docs, `.claude/` or `.github/` can skip verify — say so in the PR test plan.
  Remote CI still runs the required checks either way.
- **Changelog check:** if the change is user-facing (a feature, a fix, or a behavior change), add an
  entry under `## [Unreleased]` in `CHANGELOG.md` before committing. Internal refactors, docs and CI
  changes do not need one.

## 1. Branch

`main` is protected — never commit to it directly. On `main`, create
`git checkout -b <type>/<short-slug>` where `<type>` is the dominant conventional-commit type
(`feat`/`fix`/`docs`/`chore`/`refactor`/`ci`/`deps`). Already on a topic branch → stay on it.

## 2. Commit

- Group into **logical** commits, one concern each, following Conventional Commits.
- Pass multi-line bodies with the `-m "$(cat <<'EOF' … EOF)"` heredoc form.
- Stage explicitly (`git add <paths>`), never a blind `git add -A` — leave unrelated files behind.

## 3. Push + PR

```sh
git push -u origin <branch>
gh pr create --base main --title "<conventional subject>" --body …
```

Body follows `.github/PULL_REQUEST_TEMPLATE.md`: `## Summary` (what + why, bulleted) and
`## Test plan` (checkboxes for what was **actually** run — an unchecked box beats a false one).

## 4. Wait on CI, then merge + cleanup

- Required checks: Test (ubuntu/macos/windows), MSRV, actionlint, cargo-audit, cargo-deny, CodeQL.
- Watch with `gh pr checks <num> --watch`. For a long run, poll
  `gh pr view <num> --json statusCheckRollup` in a background loop (~45s between polls) so a
  multi-minute CI run does not burn a tool timeout.
- If branch protection marks the branch behind: `gh pr update-branch <num>`, then re-wait — that
  starts a fresh CI cycle.
- Before merging, check `gh pr list --base <branch>`: deleting a branch another open PR is based on
  **closes** that PR. If anything is stacked on it, merge without `--delete-branch`.
- `gh pr merge <num> --squash --delete-branch`
- `git fetch --prune`, then confirm `git status` is clean on the updated `main`.
- Report the PR URL and the merge commit.

## Failure handling

- `just verify` fails → stop, report the failing gate's output, do not push.
- A required remote check goes red → fix on the branch and push; CI re-runs. Never merge over
  failing checks and never use `--admin`.
- `cargo-deny` fails on a **license** → the new dependency introduced a license not in `deny.toml`'s
  allow list. That is a deliberate decision: surface it, do not just append to the list.
- `cargo-audit`/`cargo-deny` fails on an **advisory** → prefer upgrading. An ignore entry needs the
  advisory id, why it is unreachable here, and the condition for dropping it again, added to BOTH
  `.cargo/audit.toml` and `deny.toml`.
- PR not mergeable (conflict, protection) → stop and report; never force-push to a shared branch.

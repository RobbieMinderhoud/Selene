---
name: release
description: Cut a Selene release — fold the CHANGELOG + version bump into the feature PR, run checks, then (after explicit user confirmation) merge the PR and tag. Use when the user wants to release/ship a version, cut a patch/minor/major, or "release it".
---

# Release Selene

Runbook for cutting a Selene release. Changes are already developed on a feature
branch with an open PR (see the git workflow in CLAUDE.md), so a release **folds
the CHANGELOG + version bump into that PR, merges it to `main`, then tags** — you
do **not** commit release changes straight to `main`. The pushed tag drives the
GitHub Actions bundle build.

**One hard stop: get explicit user confirmation before merging the PR or pushing
the tag** (both are outward-facing and hard to undo). Invoking this skill and
confirming the target PR + version counts as that approval.

## 0. Determine the target PR + version bump

- **Which PR** is being released? Usually the one just discussed/approved; if
  several are open, confirm which. Note its number `N` and branch.
- **Version** — SemVer; the `[workspace.package]` version in the root `Cargo.toml`
  is the single source of truth. Infer from the changes (bugfix → patch, new
  feature → minor, breaking → major) and state your choice; ask only if genuinely
  ambiguous, or honour a level/version the user gave. Compute from the current one.

## 1. Pre-flight

```
git checkout <pr-branch>          # the release rides the feature PR's branch
git pull --ff-only                # branch up to date with its remote
git status --short                # clean working tree (only release work to come)
gh pr view N --json state,baseRefName,headRefName   # PR OPEN, base main
just version-check                # versions currently in sync
```

## 2. Update CHANGELOG.md (on the branch)

`CHANGELOG.md` follows [Keep a Changelog]. **Functional (user-facing) changes
only** — omit internal refactors, tooling, tests, and chores.

- Add a new section above the latest one:
  `## [X.Y.Z] - YYYY-MM-DD` (today's date).
- Group entries under the standard headings, in this order, including only those
  that apply: `Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`.
- Match the existing voice: a **bold lead sentence** stating the user-visible
  effect, then detail. Describe behavior, not code.
- There are no `[x.y.z]: <url>` link-reference definitions at the bottom — don't
  add any.

## 3. Bump + sync the version

```
just version X.Y.Z        # writes Cargo.toml + propagates to tauri.conf.json + package.json
just version-check        # must report "version in sync: X.Y.Z"
```

## 4. Run checks (scope to the change)

- Frontend-only change (fast path):
  `pnpm exec tsc --noEmit` · `pnpm exec eslint src --max-warnings 0` ·
  `pnpm exec vitest run` · `pnpm exec prettier --write <changed files>`
- Rust or mixed change: `just lint` and `just test` (note `just test` runs the
  full cargo workspace + frontend; heavier).
- Everything must be green before committing. Report failures; don't push past them.

## 5. Commit + push the branch

```
git add -A
git commit -F - <<'EOF'
<subject>; release vX.Y.Z

<body: what changed and why>
EOF
git push origin <pr-branch>
```

Commit subject: concise, describing the change, ending with `; release vX.Y.Z`.

**Hook fact (`core.hooksPath` = `~/.githooks`):** a `commit-msg` hook strips
`Co-Authored-By: …anthropic.com` trailers. Per the user's global rule, **never
add any `Co-Authored-By` trailer** regardless. (The old `prepare-commit-msg` hook
that appended ` - #<issue-number>` has been removed — subjects are no longer
suffixed, so don't add or expect a `- #`.)

## 6. STOP — confirm with the user before merging + tagging

Show the user:
- the release commit (`git log -1 --format='%h %s'` + `git show --stat HEAD`),
- the new version, and
- that the next steps will **merge PR #N into `main`** and **create + push the
  annotated tag `vX.Y.Z`** (which triggers the bundle build).

Wait for explicit approval (already satisfied if they confirmed the PR + version
when invoking the skill). Do not merge or tag until they confirm.

## 7. Merge the PR

```
gh pr merge N --squash --delete-branch    # squash matches the repo's merge history
```

The squashed commit on `main` carries the feature **plus** the version bump and
CHANGELOG entry.

## 8. Tag

Annotated tag, named `vX.Y.Z`, message `Selene vX.Y.Z`, on the new `main` HEAD
(the squash-merge commit):

```
git checkout main && git pull --ff-only
git tag -a vX.Y.Z -m "Selene vX.Y.Z"
git push origin vX.Y.Z
```

After pushing, optionally check `git tag --list --sort=-v:refname` for gaps — if a
prior release was never tagged, offer to back-tag it on its release commit (don't
do it unprompted).

## Notes

- Do not create tags outside this confirmed flow (global rule: no tags without
  explicit approval — invoking this skill + the step-6 confirmation is the approval).
- A pushed `v*` tag triggers the GitHub Actions build (`.github/workflows/build.yml`)
  of the Windows + macOS bundles.
- Not yet set up: Conventional Commits, git-cliff changelog automation,
  test/lint-on-PR CI, signed/notarized releases. If those land, revisit this runbook.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/

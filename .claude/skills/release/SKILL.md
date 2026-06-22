---
name: release
description: Cut a Selene release — update CHANGELOG, bump+sync version, run checks, commit, then (after explicit user confirmation) push and tag. Use when the user wants to release/ship a version, cut a patch/minor/major, or "release it".
---

# Release Selene

Runbook for cutting a Selene release. Follow the steps in order. **There is one
hard stop: get explicit user confirmation before pushing or tagging** (push and
tag are outward-facing and hard to undo).

## 0. Determine the version bump

SemVer. The `[workspace.package]` version in the root `Cargo.toml` is the single
source of truth.

- If the user gave a level (patch/minor/major) or an explicit version, use it.
- Otherwise infer from the changes (bugfix → patch, new feature → minor,
  breaking → major) and state your choice. Ask only if genuinely ambiguous.
- Compute the new version from the current one (e.g. 1.2.1 + patch → 1.2.2).

## 1. Pre-flight

```
git status --short        # know what's uncommitted; expect only the release work
git rev-parse --abbrev-ref HEAD   # confirm on main (the project releases on main)
just version-check        # versions currently in sync
```

## 2. Update CHANGELOG.md

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

## 5. Commit

```
git add -A
```

Commit message: a concise subject describing the change, ending with
`; release vX.Y.Z`, then a body explaining the *why*/mechanism.

**Two hook facts (`core.hooksPath` = `~/.githooks`):**
- `prepare-commit-msg` **auto-appends ` - #<issue-number>`** (the number comes
  from the branch name; empty on `main`, so you get a bare ` - #`). **Do NOT type
  `- #` yourself** or it doubles to `- # - #`.
- `commit-msg` strips `Co-Authored-By: …anthropic.com` trailers. Per the user's
  global rule, **never add any `Co-Authored-By` trailer** regardless.

Use a heredoc so the body is preserved:

```
git commit -F - <<'EOF'
<subject>; release vX.Y.Z

<body: what changed and why>
EOF
```

Then verify the subject came out with a single ` - #`:
`git log -1 --format='%s'`. If it doubled, `git commit --amend` with the suffix
removed (the hook re-adds it).

## 6. STOP — confirm with the user before pushing

Show the user:
- the release commit (`git log -1 --format='%h %s'` + `git show --stat HEAD`),
- the new version, and
- that the next steps will **push to `main`** and **create+push the annotated
  tag `vX.Y.Z`**.

Wait for explicit approval. Do not push or tag until they confirm.

## 7. Push

```
git push origin main
```

## 8. Tag

Annotated tag, named `vX.Y.Z`, message `Selene vX.Y.Z`, on the release commit:

```
git tag -a vX.Y.Z -m "Selene vX.Y.Z" <release-commit>
git push origin vX.Y.Z
```

After pushing, optionally check `git tag --list --sort=-v:refname` for gaps — if a
prior release was never tagged, offer to back-tag it on its release commit (don't
do it unprompted).

## Notes

- Do not create tags outside this confirmed flow (global rule: no tags without
  explicit approval — invoking this skill + the step-6 confirmation is the approval).
- Not yet set up: Conventional Commits, git-cliff changelog automation, CI,
  signed/notarized releases. If those land, revisit this runbook.

[Keep a Changelog]: https://keepachangelog.com/en/1.1.0/

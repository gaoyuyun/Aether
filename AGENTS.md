# Fork Development Guide

This repository is a single-maintainer customization fork of
`https://github.com/fawney19/Aether`.

## Branch roles

- `upstream/main` is the canonical upstream history.
- `main` is a clean mirror of `upstream/main`; never add custom commits to it.
- `fork/dev` is the long-lived integration branch for all custom development.
- Keep custom commits small, focused, and as isolated from upstream internals as
  practical. Optional `feature/*` branches should start from and return to
  `fork/dev`.

## Sync workflow

Use rebase because this fork has one maintainer:

```bash
git fetch upstream
git switch main
git merge --ff-only upstream/main
git push origin main
git switch fork/dev
git rebase upstream/main
git push --force-with-lease origin fork/dev
```

- Rebase `fork/dev`; do not merge `upstream/main` into it.
- Never force-push `main`, and never use plain `--force`.
- If `main` cannot fast-forward or the worktree has unrelated changes, stop and
  diagnose instead of resetting or discarding work.
- When resolving conflicts, preserve upstream behavior unless a custom feature
  intentionally overrides it, then run relevant tests.
- Do not commit or push unless the user has requested it.

---
type: Recipe
title: Housekeep branches, worktrees, and a dirty main
description: Reclaim managed worktrees through the CLI verbs, delete only merged hand-made
  branches, and commit or stash uncommitted main before the next dispatch.
sources:
- cli-help/manager/dispatch-status.txt
- cli-help/manager/worktree-prune.txt
- cli-help/manager/dispatch-close.txt
- shipped/skills/orgasmic/references/recall-resume.md
- shipped/skills/orgasmic/references/dispatch.md
---

# Housekeep branches, worktrees, and a dirty main

## Goal

Leave the project with no leaked managed worktrees, no dead task branches, and a
main whose working tree is committed or stashed — without destroying a worker's
uncommitted output or ending a dispatch by accident.

Two kinds of leftovers exist and get different treatment:

- **Managed** worktrees and branches under `~/.orgasmic/worktrees/<project>/`
  belong to dispatches. Only `dispatch-close` and `worktree-prune` may remove
  them; both salvage a dirty tree to `refs/orgasmic/salvage/<sha>` first.
- **Hand-made** worktrees and branches (`sprint/*`, `wip/*`, feature branches)
  are yours. Plain git handles them, but only once they are merged.

## Steps

1. Read [manager and dispatch operations](/operations/dispatch.md). Run from the PRIMARY project root, not a dispatch worktree (its frozen
   `.orgasmic/` snapshot reports empty state). Take the read-only inventory the
   [recall bootstrap](/references/recall-resume.md) uses:
   `git status --short`, `git worktree list`, `git branch --no-merged main`,
   and `orgasmic manager dispatch-status`.
2. Read the tail of `dispatch-status`: `RECLAIMABLE_WORKTREE` lines (path, bytes,
   why) plus `RECLAIMABLE_TOTAL`, `HELD_WORKTREE` (will not be touched),
   `AWAITING_MERGE` (a reported dispatch waiting on you — it prints the `done`
   close to run after the merge), and `PARKED` (a task `in_review`/`in_progress`
   with no open dispatch).
3. Finish open dispatches first. A reported one: merge, then
   `orgasmic manager dispatch-close --task TASK-XXXXX --started-tx tx-... --status done ...`.
   A dead one: `orgasmic manager dispatch-status --cleanup-failed` (a WRITE)
   records the orphan close and clears the lease. Close removes the worktree and
   deletes a successful branch by default; an aborted close needs
   `--branch-delete` explicitly.
4. Measure, then reclaim managed leftovers:
   `orgasmic manager worktree-prune --dry-run`, then
   `orgasmic manager worktree-prune` (`--task TASK-XXXXX` to narrow). It skips
   any worktree an open dispatch names and refuses locked, submodule-bearing, or
   still-dirty-after-salvage trees; a refusal names what to clear by hand. There
   is no `--force`.
5. Hand-made worktrees: `git -C <path> status --short`. Clean and merged →
   `git worktree remove <path>`. Dirty → commit or stash inside it first, or
   leave it and record it under handoff `** In flight`.
6. Hand-made branches: `git branch --merged main` lists safe ones;
   `git branch -d <name>` (lower-case `-d` refuses unmerged). Never `-D` a
   branch that `git branch --no-merged main` still lists without reading its
   log first. Finish with `git worktree prune` so stale `.git/worktrees`
   metadata does not accumulate.
7. Dirty main: workers see committed refs only, so every uncommitted edit is
   invisible to the next dispatch and a review dispatched against it returns a
   confident verdict on code it never read. Commit it, or `git stash push -u -m
   "<why>"` and note the stash in the handoff.
8. Exit checklist: `git status --short` clean, `git worktree list` shows only
   trees you can name, `dispatch-status` prints no `RECLAIMABLE_WORKTREE`, and
   handoff `:LIVENESS:` matches HEAD.

## Complete example

```bash
orgasmic manager dispatch-status
orgasmic manager worktree-prune --dry-run
orgasmic manager worktree-prune
git worktree list
git branch --merged main
git branch -d sprint/old-task
git worktree prune
git status --short
```

## Pitfalls

`git worktree remove --force && git branch -D` on a managed tree destroys worker
output that never reached the merged branch; that is the data-loss path the
salvage ref exists to close. `worktree-prune` never ends a dispatch — a
`HELD_WORKTREE` is released only by `dispatch-close` or `--cleanup-failed`.
Nothing prunes on a timer or at daemon boot; this recipe is the operator-run
sweep. A worktree containing an unreadable descendant is skipped whole. The
manager-dispatch convention (repo: `shipped/prompt-studio/conventions/manager-dispatch.org`)
adds one item not covered here: `tmux ls` and kill stale `orgasmic-*-run-*`
sessions, which are not reaped automatically.

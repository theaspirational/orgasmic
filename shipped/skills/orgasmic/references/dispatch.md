---
type: Topic
title: Dispatch mechanics and lifecycle
description: Worker visibility, retained worktrees, dispatch lifecycle, and finalization
  ownership.
sources:
- shipped/skills/orgasmic/references/dispatch.md
---

# Dispatch — mechanics, visibility, lifecycle

## Workers only — no raw provider CLIs

Project work is implemented and reviewed through orgasmic workers — never by
shelling out to a raw provider CLI (`codex`, `claude`, `cursor-agent`). Raw
CLIs bypass daemon write authority, tx recording, worktree isolation, dispatch
leases, and recovery.

```
orgasmic manager dispatch --kind <kind> --mode <mode> --harness <harness> \
  [--model <model>] [--effort <effort>] --brief <path> [--from <ref>] [--branch <name>]
```

Discover installed `mode`+`harness` pairs: `orgasmic manager drivers`.
Choosing the values: [`agent-selection.md`](agent-selection.md).

`orgasmic manager drivers --health` shows the shared dispatch auth preflight
and any remembered provider quota lockout. Dispatch refuses an active lockout
as `provider_quota: <provider> locked until <time>`; `--force-preflight`
overrides only that remembered quota refusal and records the override on the
dispatch tx.

## Visibility — workers see committed refs only

- The worktree is built from `--from` (default: current branch HEAD). Every
  uncommitted edit is invisible to the worker — your source edits AND the live
  daemon's uncommitted `.orgasmic/` writes. Commit both to a branch and pass
  it as `--from`.
- A review dispatched against an uncommitted diff does not fail — it returns a
  confident verdict on code it never read. Before a review dispatch, confirm
  the diff is reachable from `--from`.
- Inside a worktree, `.orgasmic/` is a frozen snapshot. Verify graph state via
  the daemon, naming the project: `orgasmic task get --project <name> <ID>`.
- An aborted implementer close cleans up by default. To continue the same task
  chain, close with `--no-worktree-remove`; that explicit flag keeps and locks
  the checkout between rounds. The next implementer dispatch for the same task
  set reuses it, but still needs a new `--branch` because the derived name
  already exists. Task order does not matter.
- To bypass a retained chain checkout, pair `--fresh-worktree` with
  `--worktree <new-path>`. Prune skips a live between-round hold, but releases
  and reclaims it once its tasks no longer permit another implementer round.
- There is a small interrupt window after reuse unlocks the checkout and before
  the daemon registers the next round. If Ctrl-C lands there, re-run the
  dispatch before running `worktree-prune`; the checkout is temporarily
  unclaimed and therefore reclaimable.

## Lifecycle

- The brief routes the worker: role (implementer/planner/reviewer/griller),
  task id, read/write scope, acceptance and evidence expectations.
- Worker startup: task heading → `project.org` + `gotchas.org` → only the
  referenced conventions and source files.
- Worker finishes with `orgasmic dispatch finalize`; the manager closes by
  started_tx (`dispatch-close`).
- After every dispatch, echo kind, mode, harness, model, effort into the
  launch message, task evidence, and handoff.

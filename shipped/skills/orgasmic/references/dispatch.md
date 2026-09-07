---
type: Topic
title: Dispatch mechanics and lifecycle
description: Worker visibility, retained worktrees, dispatch lifecycle, and finalization
  ownership.
sources:
- shipped/skills/orgasmic/references/dispatch.md
- crates/orgasmic-daemon/src/run_catalog.rs
- crates/orgasmic-daemon/src/api.rs
- crates/orgasmic-daemon/src/supervisor.rs
- crates/orgasmic-drivers/src/trait.rs
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
  uncommitted source edit is invisible to the worker. Commit source changes
  and pass the ref as `--from`; the live external ledger is accessed through
  the daemon, not copied into the source worktree.
- A review dispatched against an uncommitted diff does not fail — it returns a
  confident verdict on code it never read. Before a review dispatch, confirm
  the diff is reachable from `--from`.
- Source worktrees may have no local `.orgasmic/project.org` after external-ledger
  migration. Shared Git repository identity recognizes legitimate checkout roots;
  foreign repositories, nested directories, conflicting markers, and tombstoned
  worktrees remain refused. Do not fabricate a marker to bypass recovery checks.
  Verify live graph state through the daemon, naming the project:
  `orgasmic task get --project <name> <ID>`.
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

## Partial reports and explicit completion

Local stdio, subprocess-stream-json, and tmux workers receive `ORGASMIC_REPORT_PATH`
for their existing per-run report artifact. An assigned destination overrides
conflicting configuration; an unassigned launch clears an inherited destination.
A nonempty worker-authored report takes precedence over transport summaries.

When a Hermes run ends with `protocol_end_without_finalize` and is released as an
orphan, available stored assistant text is preserved in `last.txt` with a **PARTIAL**
header, including when the CLI reserved an empty report. This does not apply to
cancellation or overwrite worker-finalized success. Earlier journal truncation can
limit what is recoverable; worker-written reports avoid that transcript limit.
A partial report is evidence to inspect, not a success or finalization declaration.

`orgasmic manager retro` is the separate read-only diagnostic workflow described in
[manual retrospectives](/recipes/manual-retrospective.md); its submit action does
not close these source dispatches.

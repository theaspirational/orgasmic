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
- A second round cannot reuse the derived branch name (`task-xxxxx-<kind>`
  already exists) — pass an explicit `--branch`.

## Lifecycle

- The brief routes the worker: role (implementer/planner/reviewer/griller),
  task id, read/write scope, acceptance and evidence expectations.
- Worker startup: task heading → `project.org` + `gotchas.org` → only the
  referenced conventions and source files.
- Worker finishes with `orgasmic dispatch finalize`; the manager closes by
  started_tx (`dispatch-close`).
- After every dispatch, echo kind, mode, harness, model, effort into the
  launch message, task evidence, and handoff.

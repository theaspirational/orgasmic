# verify/TASK-KPMFK — a live stage loses its completion watcher across a daemon restart

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-KPMFK`.

## The defect

`post_stage` held the stage, its target and completion ownership only in the
in-process `StageCompletion` task, which dies with the daemon process. Boot
reattach rehydrated the run and respawned only `DispatchCompletion`, and only
when BOTH `last_path` and `stdout_path` had been persisted. A stage run carries
`last_path: Some` and `stdout_path: None`, so it fell into the partial-artifact
warning branch and no stage watcher was recreated.

Consequence: a `grill`, `plan` or `architect` live across a daemon restart had
its worker finalize persisted and then emitted **no** `<stage>.completed` or
`<stage>.failed` tx, ever. `stage_outcome_from_session` — the whole subject of
TASK-C0XMR and TASK-QSSQH — was never called at all. Found by the TASK-C0XMR
reviewer (reviewer-codex-rmux, run-20260728T103249) as finding 2.

## Why the injection is in two places

The fix is two halves and each half is independently load-bearing, so the
injection disables both:

1. `Supervisor::append_stage_meta` returns before writing — nothing on disk says
   which stage a run is. Kills the production-path test.
2. `boot_reattach_candidate` drops the stage identity before building the
   candidate — boot recovery cannot see it even when it is there. Kills the
   three restart tests.

Injecting only (1) would leave the restart tests green, because they write the
durable marker themselves; injecting only (2) would leave the production test
green. Either alone would understate the defect.

## What the command proves

Three restart tests, one per shared stage path, each driving the real
`reattach_live_runs_on_boot` against a genuinely live rmux session with a
second, independent `ApiState`/`Supervisor` standing in for the post-restart
daemon — then finalizing the run the way its worker would and requiring the
terminal tx. Plus one production-path test requiring that a real
`POST /api/grill` actually persists the stage identity those three depend on.

The three tmux twins are the same assertions on the other transport and stay in
the suite. They are not in `cmd` because an orgasmic worker has `tmux` symlinked
to `rmux`, so they skip there — and a pinned pass/fail count that changes with
the machine pins nothing.

No compressed window is involved: the stage watcher polls at one second, so
restart-then-complete is already seconds, not minutes.

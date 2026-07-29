# verify/TASK-HAREX — a release that cannot finish leaves a run live forever

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-HAREX`.

## What the injection reintroduces

One line, in `begin_explicit_release` (supervisor.rs): the notification that
tells a run's event drain a release has been requested. Without it the drain's
`while let Some(evt) = events.recv().await` is unbounded, which is the state
every release path was in before this task.

That is only a defect in company with a driver that keeps a sender clone alive
outside the task the supervisor holds as `producer` — `StraySenderDriver` in the
tests. `stop_and_join_driver_producer` releases the control and joins-or-aborts
`producer`; a stray clone survives all of it, `recv()` never returns `None`, and:

- `release_one` never reaches `runs.remove(run_id)`, so the record stays live in
  `GET /runs` while `POST /runs/:id/release` answers 404 — its
  `explicit_release_in_progress` is already set;
- `timed_out_run` returns `None` for the record because a `terminal_outcome` is
  present, so stall, max-duration and idle all skip it;
- no `Lifecycle::Release` is ever appended, so `record_dispatch_orphaned` has
  nothing to fire on and no `manager.dispatch_orphaned` tx is written.

The only exits left are `manager dispatch-close` or restarting the daemon, which
is what run-20260726T080801 needed.

## Why the red is a wait expiring, not a wrong value

Pre-fix there is no tombstone to be wrong about. `a_dead_worker_...` fails as
`did not release within 10s` and `a_worker_finalize_...` fails as the release
call itself never returning. Both are the incident's shape: a run that is live
with nothing behind it.

## The window, and why the replay does not sit through it

Production spends `RELEASE_DRAIN_BUDGET` = `RELEASE_FINALIZATION_DRAIN_TIMEOUT`
= 20s. The replay compresses it to 300ms via `Supervisor::set_release_drain_budget`,
the same seam the daemon uses to install `ShutdownBudgets::release_drain`, so
the compressed run drives the real code path rather than a parallel one.

The production number is asserted separately and against the real constant, on
a paused clock, by `the_drain_gate_bounds_only_after_a_release_is_requested`:
an unarmed gate outwaits an hour of driver silence, an armed one ends within
one budget. `cargo run -p orgasmic-daemon --example release_drain_budget_ms`
prints the same number from compiled truth.

## What it must not catch

`a_quiet_healthy_worker_is_never_ended_by_the_release_drain_budget` is the
negative control, and it passes with or without the injection. The bound arms
on a *requested release*, never on silence, so a dispatch worker that says
nothing for twenty minutes while cargo builds is not touched by any of this.

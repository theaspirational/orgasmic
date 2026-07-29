# verify/TASK-VSWZT — a live run no rescue action can reach, and a refusal that names no verb

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-VSWZT`.

## What the injection reintroduces

Two hunks, both the pre-fix state:

1. **`post_run_recover` (api.rs)** loses its abandon branch. Every recovery
   action is then resolved out of the crash-recovery inventory's
   `interrupted / reattached / failed_recoverable / ambiguous` buckets — which
   by construction never contain a **live** run. `orgasmic update`'s fence
   reads `GET /runs/live`, the supervisor snapshot verbatim. The two surfaces
   are disjoint sets, so a run the fence refuses on has no recovery action able
   to reach it, and `--action abandon` gets `404 recoverable run <id>` like any
   other unknown id.
2. **`live_run_refusal` (update.rs)** goes back to
   `"... Close them or use the existing force path deliberately."` — it names
   the stuck runs and then names nothing that ends one.

Together they are the 2026-07-26 dead end around
`run-20260726T080801-79056fdb262348afb3e0eb6633d3cab2`: 80 minutes of a run
that every surface reported and none could clear, and a refusal that sent the
operator looking for a verb that did not exist. The only route that worked was
restarting the daemon.

## What this artifact does NOT claim to reproduce

The *first* symptom on the task — `POST /runs/:id/release` answering 404 while
the same run sits in `.live` — is TASK-HAREX's, and it is fixed. Its producer
is a single guard in `Supervisor::release_one`: a record that is present in
`runs` but carries `explicit_release_in_progress` answers `RunNotFound`. HAREX
bounded how long that flag can stay set (`DrainGate`, `RELEASE_DRAIN_BUDGET`,
20s), so the window is now at most one budget and self-clearing. This artifact
pins what HAREX did not touch: a manager's ability to *end* a run at all.

## Why the abandoned run in the test is the wedged kind

`acquire_a_run_nothing_will_ever_end` uses `ApiHoldingDriver`, whose spawned
task parks on a `Notify` while holding a sender clone — the stray-sender shape
HAREX's `DrainGate` exists for. So the green run proves the composition the
brief asked for: abandon rides the *bounded* release rather than bypassing it.
The budget is compressed to 300ms through `Supervisor::set_release_drain_budget`,
the same seam the daemon uses to install `ShutdownBudgets::release_drain`, so
the replay drives the real path instead of a parallel one; the production number
is asserted separately, against the real constant, by HAREX's
`the_drain_gate_bounds_only_after_a_release_is_requested`.

Every timeout on that run is disabled (`stall`, `max_run_duration`, `idle` all
`Some(0)`). That is deliberate and it is the incident: the stranded run was
invisible to all three sweeps, which is why a *manager* verb had to exist.

## The negative half

`abandon_on_a_run_that_is_not_live_says_so_instead_of_not_found` is the other
edge. Post-fix, abandon on a run the supervisor is not holding does not answer
`active run <id>` or `recoverable run <id>` — the two strings that made the
incident unreadable — but says the run is not live and that a run which has
already ended carries its own release tombstone. The answer distinguishes "this
is over" from "never heard of it", which pre-fix nothing did.

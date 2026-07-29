# verify/TASK-JK66P-vzmze — the wedge that would not die

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-JK66P --artifact verify/TASK-JK66P-vzmze`. Its mirror
image is `verify/TASK-JK66P`, the healthy worker that must not be killed. One
fix, two directions, one artifact each — because a change that only satisfies
one of them is not a fix, it is the other bug.

## What the injection reintroduces

One line, in `apply_driver_event_to_record` (supervisor.rs): the stall clock
advances for every drained event regardless of variant. That was the pre-fix
state, and the crate's own test named it —
`heartbeat_is_non_terminal_so_drain_never_releases_on_it`, "the event drain
resets `last_driver_event_at` for every drained event (variant-agnostic)".

Measured 2026-07-26 on
`run-20260726T144430-aa47b867840f42f282b30d3469949731` (acp-stdio / codex):
118 heartbeats at exactly 30s apart, 0 tool calls, 0 tool results, 0 bytes
written into the worktree, 6.77s of CPU in 60 minutes. Against a 600s stall
timeout that run is unreleasable until the 4-hour
`DEFAULT_MAX_RUN_DURATION`. The manager's shell watcher caught it in an hour
because it was told to; the daemon would have taken four.

## Why the red is what it is

The first assertion is the incident: the run is still live after its stall
window, because something that is not work kept resetting the clock. The second
is the mechanism in one line — a heartbeat must refresh liveness (a harness that
*stops* beating is a different failure, needing a different response) and must
not refresh work.

## Why the fix is not "stop counting heartbeats"

Because that alone kills every rmux dispatch. `pane_activity` is the only stall
input a pane transport has (TASK-RWCRN, and the variant's own doc comment in
`orgasmic-core::session` names this change as the one that must not drop it), so
the clock splits by *evidence*, not by variant class: pane bytes count, harness
stderr and heartbeats do not, and when the evidence channel is silent the daemon
looks at what is running under the run before it shoots. The other direction of
that same design is `verify/TASK-JK66P`.

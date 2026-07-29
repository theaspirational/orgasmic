# verify/TASK-RB1ZN — one error for two opposite states

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-RB1ZN`.

## What the injection reintroduces

One hunk, in `Supervisor::release_one`: the in-progress guard answers
`SupervisorError::RunNotFound` again, as it did before this task.

That is the whole defect. The record IS in `runs` — it is present, it is
reported by `GET /runs/live`, and the update fence refuses on it — and it is
being released by somebody else, which is the opposite of absent. Answering the
same error for both states erases the difference before any surface can see it,
so every surface downstream renders 404 for a run it can watch being alive.

The injection deliberately leaves the fix's other half in place: the
`release_in_progress_conflict` renderer and the `ReleaseInProgress` arms on both
release surfaces stay exactly where they are, and become unreachable. A caller
cannot re-derive what the supervisor collapsed, which is precisely why the split
had to happen at the supervisor and not at each surface.

## What the red proves

`a_run_wedged_mid_release_says_so_instead_of_run_not_found` (supervisor.rs) —
the state itself, built rather than simulated. `StraySenderDriver` (TASK-HAREX)
leaves one sender clone parked in a task the supervisor does not own, so
`events.recv()` never yields `None` and a release sits in the bounded drain for
its whole budget. While it sits there, a second release gets `RunNotFound` for a
run the same test then asserts is live in the snapshot.

`a_run_wedged_mid_release_is_a_conflict_at_both_release_surfaces` (api.rs) —
the same wedge on the wire, at both surfaces an operator can reach:
`POST /runs/:id/release` and `POST /runs/:id/recover {"action":"abandon"}`. The
pinned red is the 2026-07-26 incident's first symptom verbatim: `404 active run
<id>` for a run reported by `/runs/live`.

## Why the wedge is deterministic and not a sleep

Both tests wait for the finalize-admission marker (`worker_finalize_admitted`,
TASK-QSSQH), which `release_one` appends under the same lock guard that sets
`explicit_release_in_progress` and *before* the teardown that follows. Its
arrival is proof that the record is present AND already being released — the
exact state the split is about.

Polling with a second release call instead would be a race, not a wait: before
the first release is admitted, a second one does not get refused, it gets
admitted itself. That is why the marker, not a retry loop, is the trigger.

The drain budget is compressed to 1.5s through `set_release_drain_budget`, the
same seam `ShutdownBudgets::release_drain` uses, so the replay drives the real
bounded path. The assertions cross that window in microseconds; the tests cost
one wedge each (~1.6s, ~1.9s measured). The production number (20s,
`RELEASE_FINALIZATION_DRAIN_TIMEOUT`) is what the 409's text names, and is
asserted against the real constant by the api test.

## The green half, which the red does not cover

Both tests end on the OTHER side of the split: once the drain's budget expires
and the record is gone, the same calls go back to `RunNotFound` / 404. That is
the contract the CLI's already-released rescue branch
(`is_release_run_not_found_error`, TASK-DWJVH item B) keys on, and it must not
have moved. Those assertions pass under the injection too — they are there to
pin what the fix must NOT change, not to reproduce the defect.

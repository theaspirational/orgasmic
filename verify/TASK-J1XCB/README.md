# TASK-J1XCB — what this proof replays, and what it cannot

Two daemon tests failed under load, passed alone, and had no owning task, so
`scripts/run-tests.sh` classified them REAL and every loaded full-suite run on
this machine exited nonzero:

- `supervisor::tests::dead_pid_aborts_joins_hung_producer_then_receiver_releases`
- `tests::replacement_start_waits_out_a_shutting_down_predecessor`

Both were fixed rather than registered. Neither defect was in the daemon: both
tests measured a **fixed amount of wall clock** against work whose real cost is
a `tokio::time` timer, and a timer on a loaded host fires late.

## What each one was really asserting

`dead_pid_…` hands the driver stop a hung `DriverControl::release` *and* a hung
producer, which is the only way to reach the abort path it is named after.
`stop_and_join_driver_producer` therefore spends `DRIVER_RELEASE_TIMEOUT` twice
before the abort closes the channel. Measured alone on an idle machine on
2026-07-29: **10.08s**, against a 12s assertion. A 1.19x margin is not a hang
detector, it is a bet on the machine.

`replacement_start_…` (TASK-ATAXN's regression) parks a predecessor in a 1200ms
connection drain and starts a replacement 250ms later. The replacement's ceiling
is the predecessor's published budget, `DRAIN + 2 * TAIL`, and `TAIL` was 100ms
— so the entire margin was the ~375ms head start, against a drain enforced by a
timer.

## The fixes

`dead_pid_…` injects a 150ms driver-release budget through a new test-only cell
on `Supervisor`, mirroring TASK-HAREX's `set_release_drain_budget`, and its
driver now emits the racing terminal event from `release()` — where both tests'
names already say it happens — instead of from a 6s producer sleep that landed
inside the join window only because that window happened to be `[5s, 10s]`.
Test cost: 10.08s -> 0.44s.

`replacement_start_…` raises `TAIL` to 2s. That is not a tolerance bump on the
term under test: `graceful_shutdown` passes `release_drain` to `wait_idle` and
`writer_shutdown` to `shutdown_within`, both of which return the moment their
subject is idle, and this predecessor has no release in flight and a quiet
writer. The predecessor's real cost is unchanged, the replacement's ceiling
grows by 3.8s, and the test still runs in 1.53s.

No production behaviour changed. The `Supervisor` cell defaults to
`DRIVER_RELEASE_TIMEOUT` and its setter is `#[cfg(test)]`.

`replacement_start_…` had a *second* load-dependent failure, which surfaced only
once the first was fixed. It is pinned separately in
`verify/TASK-J1XCB-marker/`.

## The honest limit of a deterministic artifact

The failures are load-probabilistic. Measured here 2026-07-29 on the pre-fix
binary, full `orgasmic-daemon --lib` at `--test-threads=64`: cpu hogs did not
reproduce them at all (0 of 6 runs under 10 `yes` children at load average 164),
and **suite concurrency** did — four copies of the binary at once put
`dead_pid_…` red in **10 of 12** runs. Post-fix, same harness: **0 of 12**. Full
counts and conditions are in the task's implementer report. A `cmd` that reproduced
*that* would go red only some of the time, and an artifact which only sometimes
goes red is worse than none: the replay's green would mean nothing.

So the injection does not reproduce the load. It removes both fixes and then
makes each loss deterministic by adding the lateness the load supplied:

| injected | why it is faithful |
| --- | --- |
| `stop_and_join_driver_producer` sleeps 3s before it starts | the teardown then costs 13s against the unchanged 12s bound; under load the same two timers simply ran past it |
| `graceful_shutdown` sleeps 1.5s before its first phase | the predecessor overruns its published budget, which is what a loaded host does to a `timeout`-bounded drain and the teardown after it |

What the replay proves: with the work running late, the wall-clock observation
fails and the load-independent one does not exist to be asked — because the red
phase is the pre-fix code.

What it does not prove: that the fixed tests survive the *same* injected
lateness, since `orgasmic verify` reverts the whole patch before the green run.
That counterfactual was measured by hand instead — the two trigger sleeps
applied WITHOUT the two test-side reverts, i.e. the fixed tests against a
3s-late driver stop and a 1.5s-late shutdown: both green. See the task's
implementer report for that run.

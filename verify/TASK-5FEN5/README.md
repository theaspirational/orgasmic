# TASK-5FEN5 — what this proof replays, and what it cannot

Two daemon tests failed under induced load, passed alone, and had no owning
task, so `scripts/run-tests.sh` classified them REAL and every loaded
full-suite run exited nonzero:

- `supervisor::tests::live_babysitter_summary_flushes_on_event_threshold`
- `index::tests::live_rebuild_preserves_repo_url_and_schedules_post_bind_refresh`

Both were fixed rather than registered. Neither defect was in the daemon: both
tests observed background work by handing it a **fixed budget** instead of
waiting on the signal that the work was done.

## The honest limit of a deterministic artifact

The failures are load-probabilistic. Measured here on 2026-07-29, pre-fix, full
`orgasmic-daemon --lib` under the load recipe (12 `yes` hogs,
`--test-threads=64`), 11 runs:

| test | pre-fix red | post-fix red | alone |
| --- | --- | --- | --- |
| `live_babysitter_summary_flushes_on_event_threshold` | 3 / 11 | 0 / 10 | green |
| `live_rebuild_preserves_repo_url_and_schedules_post_bind_refresh` | 1 / 11 | 0 / 10 | green |

A `cmd` that reproduced *that* would go red about a quarter of the time, and an
artifact which only sometimes goes red is worse than none: the replay's green
would mean nothing. So the injection does not reproduce the load. It restores
the two budgets **and makes each one lose deterministically**, by doing to the
background work what the load did to it:

| injected | why it is faithful |
| --- | --- |
| the babysitter summary append sleeps 3s (> the restored 2s budget) | under load the flush arrived after the budget; here it always does |
| the first live Git probe returns without writing a URL | that is exactly what `refresh_repo_url` does when its 3s bound is spent — measured at 2.24s on a *passing* loaded run |

What the replay proves: with the work arriving late, the budget-based
observation fails and the signal-based observation does not exist to be
asked — because the red phase is the pre-fix code.

What it does not prove: that the fixed tests survive the *same* injected
lateness, since `orgasmic verify` reverts the whole patch before the green run.
That counterfactual was measured by hand instead — the trigger halves of the
injection applied WITHOUT the test-side revert, i.e. the fixed tests against a
3s-late flush and a first Git probe that returns nothing: both green. See the
task's implementer report for that run.

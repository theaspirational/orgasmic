# verify/TASK-JK66P — a healthy worker killed for being quiet

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-JK66P`. Its mirror image is `verify/TASK-JK66P-vzmze`,
the wedge that must still die; the two are one fix and each artifact pins one
direction of it.

## What the injection reintroduces

One early return, in `Supervisor::observe_work_evidence` (supervisor.rs): the
daemon never looks at what is running under a run. That is precisely the pre-fix
world — before this task the stall detector had one input, driver events, and a
pane transport's only driver event is `pane_activity`.

Measured 2026-07-29 on `dispatch-TASK-MRJRK-implementer-20260729T000911`, and
again the same night on TASK-4YC8E: `pane_activity` every ~30s until 00:41:22,
then the worker ran `scripts/run-tests.sh` — which redirects cargo to files, so
the pane writes nothing — and at 00:51:22, ten minutes of silence to the second,
`run_complete stall_timeout_exceeded`, `finalized_by_worker: false`. The worker
was healthy: report.md complete but for the gate it was killed producing, branch
committed, verify artifact self-tested PASS. The manager salvaged both by hand.

## Why the red is what it is

Pre-fix there is nothing to be subtly wrong about: the run is simply gone. The
first assertion fails on the first expired budget (`quiet window 0`), which is
the kill. The second fails on the tombstone, which is the other half of the
incident — the operator got `stall_timeout_exceeded` and nothing else, the same
string a genuinely wedged harness produces, so wedge and healthy-worker were
indistinguishable at the only place anyone looks.

## What the fix reads instead

CPU burned by the process subtree under the run — the pane's root process for
tmux (resolved through `tmux display-message '#{pane_pid}'`), the wrapper pid
for subprocess transports. Liveness alone is deliberately not the test:
TASK-VZMZE's wedged harness was alive for the entire hour it did nothing at
0.19% of a core, while a cargo build's subtree reads in the hundreds of percent.
The threshold sits between those two measurements at 5%.

## The window, and why the replay does not sit through it

Production spends `DEFAULT_STALL_TIMEOUT` = 600s. The replay compresses it to
1s through `stall_timeout_secs` — the same per-run field production resolves the
default into — so every line of the detection path runs, on the real clock, at
1/600th of the wall time. The real 600s constant is asserted separately by
`stall_detector_releases_after_no_driver_events` and by the two TASK-RWCRN pane
tests, which use `DEFAULT_STALL_TIMEOUT` itself.

## What the probe cannot be proven by, here

No unit test can put a real cargo build under a real tmux pane, so these two
drive the decision with a probe double and the production probe is proven
separately against a real process subtree:
`process_subtree_cpu_probe_sees_a_real_cpu_burning_child` (a `/bin/sh` busy loop
reads as work) and
`process_subtree_cpu_probe_does_not_mistake_a_live_idle_child_for_work` (a live
`sleep` does not). Those two are the production `ps` parse, subtree walk and
threshold on the real OS.

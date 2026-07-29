# TASK-SVKPN — a fast-exiting harness must not lose its transcript

## What the probe proves, and where

The defect is in `run_subprocess_stream_json`
(`crates/orgasmic-drivers/src/modes/subprocess_stream_json.rs`): its select loop
covers stdout, stderr and a command channel, and the command branch `break`s on
release — or on the channel closing — without draining stdout.
`tokio::select!` picks at random among ready branches, so the break can land
with the harness's entire output still unread in the pipe.
`finalize_subprocess_exit` then distills an empty summary, synthesizes no
`RunComplete`, and the daemon orphans the run as
`protocol_end_without_finalize` with an empty transcript.

The probe runs at the driver layer, on purpose. The daemon-level symptom is
real and was measured (TASK-Z7VQK), but it is *load-dependent*: it needs the
early-exit watcher to observe the pid gone while lines are still pending, and
under a quiet machine the loop usually wins the race. The 5FEN5 precedent says
a load-triggered symptom is not patch-expressible and asks for pinned A/B
counts instead; those are in this task's report. What is patch-expressible is
the ordering itself, and that is what this artifact pins.

## Why the red is deterministic and not a coin flip

The construction removes the load dependency rather than papering over it:

1. `acquire` spawns the child and *queues* the producer task. There is no
   `.await` after that `tokio::spawn`, so on the `current_thread` flavour the
   test pins, the producer has not been polled when `acquire` returns.
2. The test then blocks the runtime thread — `std::thread::sleep`, deliberately
   not `tokio::time::sleep` — until the harness pid is a zombie. The harness has
   printed all 32 lines and exited; every one of them is readable in the pipe;
   nothing in its process group can be signalled into a different exit status by
   the release's group reap.
3. Only then is the release sent. The producer's *first* poll sees 32 lines
   readable and a release command ready at the same instant.

That is the production ordering exactly — the daemon's early-exit watcher
releasing a harness that has already finished — with the timing pinned instead
of sampled. Measured 8/8 identical on the injected tree: **0 of 32 lines**.

There is no sleep in the harness. TASK-Z7VQK's report is explicit that a
fixture-side delay is what hid this defect (every fresh fixture used to pay a
~160ms first-exec evaluation, and a build with a 200ms pre-body sleep dropped
the family's failures from 4 to 1 in the same load window). Adding one back
here would have made the probe green against a broken driver.

## The bound

The fix drains stdout and stderr after the break, bounded by
`RELEASE_DRAIN_BUDGET`, and the bound is derived rather than chosen:

    PRODUCER_JOIN_BUDGET_MS (5000)   supervisor.rs DRIVER_RELEASE_TIMEOUT — the
                                     join it gives this task after the release ack
  - GROUP_REAP_GRACE_MS     (2000)   TERM → grace → KILL, inside the same join
  - FINALIZE_SLACK_MS       (1000)   finalize_subprocess_exit's channel sends
  = RELEASE_DRAIN_BUDGET    (2000)

Overrunning the drain costs only the events still unread; overrunning the join
would cost the whole synthesis, because the supervisor then aborts this task
mid-finalize. This is the driver-side sibling of TASK-HAREX's `DrainGate`
(which bounds the *supervisor's* wait on the other end of the same channel);
they compose and are not the same gate.

## The injection

`injection.patch` deletes the post-release drain and its call site, restoring
the pre-fix loop exactly — `break`, reap, finalize. It adds `dead_code` to the
drain helper's existing `allow` so the red run is a test failure and not a
warning storm. It changes nothing else: not the fixture, not the test, not the
budget constants, not the acp-stdio sibling.

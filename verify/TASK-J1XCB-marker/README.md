# TASK-J1XCB (marker) — the second failure mode of one test

`tests::replacement_start_waits_out_a_shutting_down_predecessor` had two
load-dependent failures, not one. `verify/TASK-J1XCB/` pins the first (the
replacement is refused while its predecessor is leaving normally — TASK-ATAXN's
outage). This artifact pins the second, which surfaced only after that one was
fixed, in this task's own post-fix reproduction runs:

    thread 'tests::replacement_start_waits_out_a_shutting_down_predecessor'
    panicked at crates/orgasmic-daemon/src/lib.rs:1808:50:
    the predecessor published its departure

Measured 2026-07-29, 1 of 12 runs, four copies of the daemon `--lib` binary
running concurrently at `--test-threads=64`.

## The defect

The test slept 250ms after `shutdown.send(())` and then read the shutdown marker
once, with `.expect()`. But `publish_shutdown_marker` runs on the daemon's
shutdown task, after `drain_started_rx` resolves — so that line asserted that
the host had scheduled a task inside 250ms. On an oversubscribed machine it had
not, and the test failed before reaching anything it is about.

## The fix

Wait for the marker instead of assuming the sleep produced it: poll until it
appears, with the original "the predecessor finished shutting down before the
replacement even started" liveness assertion as the loop's failure exit. A
predecessor that really finished early still fails the test, with the message
that says so; one that is merely slow to publish is waited for. The 250ms sleep
stays — it is what makes the replacement arrive after the transient lock budget,
which is a separate thing the test needs.

Test-side only. No production behaviour changed.

## What the replay proves, and does not

Same limit as the sibling artifact: `orgasmic verify` reverts the whole patch
before the green run, so the green phase is the fixed test without the injected
600ms publication delay. The counterfactual — the fixed test *with* that delay —
was measured by hand; see the task's implementer report.

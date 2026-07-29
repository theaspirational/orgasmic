# TASK-TZKAC — what this proof replays, and what it cannot

`acquire_daemon_lock` retried the daemon instance lock for
`DAEMON_LOCK_RETRY_BUDGET` — 125ms, sized by TASK-870YX for "a CLI probe that
holds the lock for microseconds" — and then, when the home held no shutdown
marker, **stopped trying the lock entirely**. Everything after that point was
classification of a holder it would never race again, and the first thing the
incumbent probe could not establish became a permanent refusal:
`LockHolder::NotDeparting`, "Refusing to start a competing daemon".

TASK-J1XCB measured that losing to a transient holder twice, on a host doing
nothing more exotic than running four copies of one test binary:

- `api::tests::deleted_or_corrupt_claim_reconstructs_exact_status_and_post`,
  1 of 36 loaded runs, on the test's *restart* of the daemon — "the configured
  port is 0, so the incumbent address cannot be probed".
- `tests::daemon_lock_retries_a_transient_probe_hold` — same holder class, same
  budget — "the incumbent has not created an auth token yet (it may still be
  booting)".

This is production behaviour on the ATAXN/870YX lock protocol. A real operator
restart can lose the same race, and the outcome is a machine with no daemon.

## What was actually wrong

Two things, and the failure needs both.

**The retry stopped permanently.** A holder that released at 126ms was refused
exactly as hard as one that never released.

**"I could not observe" was treated as "there is a competing daemon".** Look at
what the two measured refusals say. *The incumbent has not created an auth token
yet — it may still be booting.* *The configured port is 0, so the address cannot
be probed.* Neither is a statement about the holder; both are statements about
the observer. Every outcome the probe can produce short of "a daemon answered,
and it is for this home" is equally the signature of a daemon still booting and
of one that is wedged. Refusing on those is refusing on the absence of evidence.

## The fix, and where its number comes from

Only `HolderVerdict::Healthy` concludes anything now. Everything else is
`Inconclusive`, remembered as evidence rather than acted on, while the loop
keeps trying the lock, keeps re-reading the shutdown marker — a predecessor
holds the lock from the start of its drain but publishes its marker only once
the drain has begun, so the marker can appear mid-wait — and keeps probing.

The ceiling is `undeclared_holder_budget`: **this daemon's own
`ShutdownBudgets::total`**, the interval this very process would hold the lock,
listener closed and answering nothing, if it were the one departing. It is
derived, not chosen. A holder that has published no marker is not a different
kind of holder from one that has; it is the same holder before it has said
anything. Giving both the same ceiling makes one statement true of the whole
protocol — *no holder, declared or not, can cost a replacement more wall clock
than a real shutdown can* — which is the rule `DaemonShutdownMarker::budget`
already applied to a marker that overstates its own budget. One number, one
derivation, and it moves when `ShutdownBudgets` moves.

What did **not** change: a healthy incumbent still answers the first probe and
is still reported immediately, and a predecessor that published a marker and
then outlived its own budget is still conclusive and still refuses at once.
Those are positive evidence. `NotDeparting` was the only verdict reached by
running out of patience, and it is the only one that now costs the ceiling.

## The injection

The load is not reproduced, and deliberately so. The original failures are
load-probabilistic — 1 in 36 — and an artifact that only sometimes goes red is
worse than none, because then its green means nothing.

Instead the injection **removes the fix by stating the pre-fix rule exactly**:
`undeclared_holder_budget` returns `Duration::ZERO`, so once the 125ms transient
budget expires with no marker on disk, the lock is never attempted again and the
first inconclusive probe result becomes a permanent refusal. That is the old
control flow, written as one constant.

The two tests it catches supply the determinism the load used to:

| test | what it does to the code what the load did |
| --- | --- |
| `daemon_lock_outwaits_a_holder_that_outlives_the_transient_budget` | holds the lock from an alive in-process thread for **1.2s** and then lets go — the same holder as `daemon_lock_retries_a_transient_probe_hold`'s 25ms one, with the lateness written in instead of hoped for |
| `daemon_lock_continuously_held_fails_closed` | injects a 300ms `ShutdownBudgets` and asserts the wait reaches it — so a ceiling that is not derived from those budgets fails |

The injected red reproduces J1XCB's second measured refusal **verbatim**: *the
incumbent has not created an auth token yet (it may still be booting)*. That
string is pinned in `expect-red`, which is what keeps a future green from being
bought by some unrelated instance-lock failure.

## The honest limits

**The transient holder in the api.rs restart was never identified.** J1XCB ruled
out the obvious candidates by reading — the predecessor's lock is dropped before
its join returns, the home is a fresh tempdir per iteration, nothing outside
`acquire_daemon_lock` opens `daemon.lock` — and proposed logging the pid the
lock file records at the refusal site. That probe was built and shipped
(`describe_lock_holder`, on every refusal from this path), and run under the
reproduction harness. See the task's implementer report for the counts. The fix
does not depend on the answer: it works for *any* holder that lets go, which is
why it was worth shipping before the identification landed. If the sighting
recurs, the refusal now names the pid, whether it is alive, the boot record and
the marker — so the next observation answers the question this one could not.

**The replay does not prove the fixed tests survive the injected condition**,
since `orgasmic verify` reverts the whole patch before the green run. Here that
gap is narrow: the injection is one constant in production code and touches no
test, so the green run exercises the same two tests against the real ceiling.

**A wedged holder now costs a start 40s instead of 125ms** before it refuses.
That is the deliberate trade, and it is stated rather than hidden: the 125ms
answer was fast and sometimes wrong in the direction that leaves a machine with
no daemon. The wait is announced in the log while it happens, and the refusal at
the end of it names everything the home knows about the holder.

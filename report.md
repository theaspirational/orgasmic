# TASK-TZKAC — implementer report

Branch `task-tzkac-impl`.

## Premise corrections

**The brief's premise held.** `acquire_daemon_lock` did exactly what the task
body says: retry inside `DAEMON_LOCK_RETRY_BUDGET`, then stop retrying the lock
entirely and classify. Read at `crates/orgasmic-daemon/src/lib.rs:516-638` on
the merged HEAD (4bb4da0). No correction needed.

Two refinements to the diagnosis, both of which changed the fix:

1. **The give-up is only half the defect.** The other half is that the
   *classification* treats inconclusive observations as conclusive. Both
   measured refusals are statements about the observer, not the holder: "the
   incumbent has not created an auth token yet (it may still be booting)" and
   "the configured port is 0, so the incumbent address cannot be probed". A
   longer retry alone would not have been enough — the loop has to stop
   refusing on the absence of evidence.

2. **`api::tests::deleted_or_corrupt_claim_…` can only ever be fixed by
   acquiring the lock, never by classifying.** It runs `test_options()` with
   `port_override: Some(0)`, so the incumbent probe is *permanently* impossible
   for that home. That rules out "make the probe smarter" as a fix and forces
   the fix into the retry, which is where it belongs.

## 1. The re-own commit — `bc7bb51`

`verify/flake-registry.toml`'s one TASK-J1XCB row moved to `owner =
"TASK-TZKAC"`, with a rewritten header comment saying why the hand-off happened
(the mechanism is a production protocol rule, not J1XCB's fourth test fix) and
noting the `evidence` field is still J1XCB's unchanged measurement.

    $ bash scripts/run-tests.sh --check
    registry: OK — 7 entries in verify/flake-registry.toml, every owner open

Landed separately and first, as asked, so it is visible even if later work
stalls. TASK-J1XCB's graveyard check is unblocked.

## 2. The probe — built, shipped, run, did not fire

J1XCB's proposed probe was to log the pid recorded inside the lock file at the
refusal site. Built as `lock_file_recorded_pid` + `describe_lock_holder`
(`lib.rs`), and it **ships** rather than being a throwaway: every refusal from
this path now appends `[holder: …]` naming the pid the lock file records and
whether that pid is alive, the boot record (`daemon.boot`, pid/phase/seq), and
the shutdown marker. Sample, from the injected red:

    [holder: the lock file records no pid; no boot record; no shutdown marker]

**It did not reproduce pre-fix.** Harness, J1XCB's recipe: four concurrent
copies of the `orgasmic-daemon --lib` binary at `--test-threads=64`, pre-fix
binary carrying the probe. **7 rounds = 28 loaded copy-runs, 0 reproductions**
of either named test. Per-copy results were identical across all 28:
`563 passed; 2 failed`, both failures the same two non-target tests (below).
J1XCB's own rate was 1 in 36, so 0 in 28 is unsurprising rather than
contradictory. I stopped that harness at round 7 to spend the remaining wall
clock on the post-fix rounds.

**So the api.rs restart's transient holder is still unidentified**, stated
plainly as the brief allows. The probe is in place permanently, so the next
sighting answers it.

**But the post-fix harness caught the class in the act, once, on the real
path.** Round 3, copy 4 — a genuine `Daemon::run` (the log line immediately
after is `orgasmic daemon starting pre-bind boot work`, so this is a booting
daemon, not one of the `acquire_daemon_lock` unit tests) met an undeclared
holder, waited it out, and started:

    INFO orgasmic_daemon: daemon instance lock is held by something that has
    recorded no shutdown and does not answer a health probe; retrying the lock
    for as long as a shutdown may take before refusing to start
    budget_ms=40000 holder=the lock file records no pid; no boot record; no
    shutdown marker
    INFO orgasmic_daemon: the undeclared lock holder released; took the daemon
    instance lock waited_ms=488

488ms is **3.9x** `DAEMON_LOCK_RETRY_BUDGET`. Pre-fix that start refuses — it
is exactly the registered flake's shape. What the probe adds about the holder:
it recorded **no pid in the lock file**, no boot record, and no shutdown
marker. `open_and_try_lock_daemon` writes the acquirer's pid on every
successful acquisition, so a holder that had completed that path would have
named itself. That narrows the holder to something that had the `flock` but had
not yet finished writing its identity — which is precisely the "took the lock
and has not said anything yet" class the fix is built for, and it is why the
fix did not need the identification to be correct.

I am *not* claiming this is the same holder the api.rs restart met; one
captured instance in a 64-thread binary cannot be attributed to a specific
test. It is evidence that the class is real, occurs on the production path
under this load, and is now survived.

The two failures seen in all 28 copy-runs, neither a target test:

| test | classification |
| --- | --- |
| `supervisor::tests::poll_direct_child_pid_prefers_worker_server_over_generic_sibling` | registered flake, owner TASK-STWVB; panic `fake cursor-agent did not start children` matches the registered signature exactly |
| `supervisor::tests::required_test_tooling_is_present` | **environment-blocked.** `tmux` on this PATH resolves to the rmux shim (`/var/folders/…/rmux-shim-…/tmux`), which the tooling check rejects: `required test tooling is missing: tmux (gates 9 tests)`. A property of running the suite from inside an rmux pane, not of this change. Reported, not registered. |

## 3. The decision — what a holder that has declared nothing is owed

Committed as `5c97409`.

**Only "healthy incumbent" concludes anything.** `classify_lock_holder` now
returns `HolderVerdict::Healthy` or `HolderVerdict::Inconclusive(detail)`.
Every outcome short of "a daemon answered and it is for this home" — no auth
token, port 0, connection refused, non-success status, wrong home — is equally
the signature of a daemon still booting and of one that is wedged, so it is
remembered as evidence instead of acted on.

**Past the transient budget the loop keeps going.** It keeps trying the lock, it
re-reads the shutdown marker each round (a predecessor holds the lock from the
start of its drain but publishes its marker only once the drain has begun, so
the marker can appear mid-wait — the old code read it exactly once), and it
re-probes at most once per `INCUMBENT_PROBE_INTERVAL` so probes never queue.

**The ceiling: `undeclared_holder_budget(opts)` = this daemon's own
`ShutdownBudgets::total()`.** The derivation, which is the part the task asked
to be deliberate about:

> A holder that has published no marker is not a different kind of holder from
> one that has. It is the same holder before it has said anything — a daemon
> entering shutdown holds the lock from the moment its drain begins and
> publishes its marker only when the drain has begun; a booting daemon holds it
> from `acquire_daemon_lock` returning until its listener answers. Both windows
> are undeclared and both end. Giving the undeclared case the same ceiling as
> the declared one makes a single statement true of the whole protocol: **no
> holder, declared or not, can cost a replacement more wall clock than a real
> shutdown can** — which is exactly the rule `DaemonShutdownMarker::budget`
> already applies to a marker that overstates its own budget.

It is `opts.shutdown_budgets`, not the global default, so it needs no new config
surface, it shrinks in tests that already inject budgets, and it moves when
`ShutdownBudgets` moves. In production it is 40s.

**What did not change**, deliberately:

- A healthy incumbent still answers the first probe and is still reported as
  `DaemonAlreadyRunning` at the same latency. This is the common "lock is held"
  case and it did not get slower.
- A predecessor that published a marker and then outlived its own budget is
  *positive* evidence of being stuck, so it stays conclusive and refuses at
  once. `a_predecessor_that_never_finishes_shutting_down_fails_fast_and_names_itself`
  keeps its `elapsed < BUDGET + 5s` bound.
- `NotDeparting` was the only verdict ever reached by running out of patience,
  and it is the only one that now costs the ceiling.

**The name-the-evidence property survived and grew.**
`LockHolder::NotDeparting` now carries `waited`, so the refusal says how long it
looked, and `describe_lock_holder` appends what the home knows about the holder:

    daemon instance lock <tmp>/home/daemon.lock is held, but the incumbent is not
    healthy and no daemon has recorded a shutdown for this home, so the holder is
    not a departing predecessor: retried the lock for 125.982083ms and the
    incumbent has not created an auth token yet (it may still be booting)
    [holder: the lock file records no pid; no boot record; no shutdown marker].
    Refusing to start a competing daemon

**The stated trade:** a genuinely wedged holder now costs a start 40s instead of
125ms before it refuses. That is the deliberate cost of not refusing on the
absence of evidence, it is announced in the log while it happens (`retrying the
lock for as long as a shutdown may take before refusing to start`), and the
refusal at the end names everything observed. The 125ms answer was fast and
sometimes wrong in the direction that leaves a machine with no daemon.

## 4. Verify artifact — `ce4e8e7`, self-tested

`verify/TASK-TZKAC/` — `injection.patch`, `cmd`, `expect-red`, `README.md`.

The injection removes the fix by stating the pre-fix rule as one constant:
`undeclared_holder_budget` returns `Duration::ZERO`, so past the 125ms transient
budget the lock is never attempted again and the first inconclusive probe
result becomes a permanent refusal. Two tests catch it, and they supply the
determinism the original 1-in-36 load could not:

- `daemon_lock_outwaits_a_holder_that_outlives_the_transient_budget` (new) —
  an alive, in-process holder that lets go **1.2s** later. The same holder as
  `daemon_lock_retries_a_transient_probe_hold`'s 25ms one, with the lateness
  written into the code instead of hoped for (5FEN5 template).
- `daemon_lock_continuously_held_fails_closed` — injects a 300ms
  `ShutdownBudgets` and asserts the wait reaches it, so a ceiling that is not
  derived from those budgets fails.

The injected red reproduces J1XCB's second measured refusal **verbatim** ("the
incumbent has not created an auth token yet (it may still be booting)"), pinned
in `expect-red` so a future green cannot be bought by an unrelated instance-lock
failure.

Tested replay, on a clean tree, before reporting:

    $ ./target/debug/orgasmic verify TASK-TZKAC --artifact verify/TASK-TZKAC
    verify TASK-TZKAC
      artifact  verify/TASK-TZKAC
      command   env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME cargo test -p orgasmic-daemon --lib -- --exact tests::daemon_lock_outwaits_a_holder_that_outlives_the_transient_budget tests::daemon_lock_continuously_held_fails_closed --test-threads=1
      [tree]    clean
      [inject]  injection.patch applied
      [red]     as pinned — exit 101, signature matched
      [revert]  reverted; tree byte-identical
      [green]   passes without the injection — exit 0
    verify TASK-TZKAC: PASS — red-then-green replay reproduced

## 5. Harness rounds

Same recipe both times: four concurrent copies of the `orgasmic-daemon --lib`
binary at `--test-threads=64`, each copy a full 566-test suite.

| | rounds | copy-runs | `deleted_or_corrupt_claim_…` | `daemon_lock_retries_a_transient_probe_hold` |
| --- | --- | --- | --- | --- |
| pre-fix (probe build) | 7 | 28 | 0 red | 0 red |
| post-fix | 7 complete (+1 stalled) | 28 | **0 red** | **0 red** |

**Honest reading of these counts.** The post-fix zeroes are *consistent with*
the fix but are not by themselves proof of it, because the pre-fix rounds were
also zero — the defect's rate is ~1 in 36 and neither harness run was long
enough to be decisive. The load-bearing evidence is elsewhere and is
deterministic: the injection proof (`verify/TASK-TZKAC`, red-then-green
replayed), the new
`daemon_lock_outwaits_a_holder_that_outlives_the_transient_budget` which
reproduces the class on every run, and the captured 488ms production-path wait
above.

**Round 8 stalled and was killed.** After ~26 minutes with 552–560 of 566 tests
complete per copy and no further output, the machine held **31 leaked `/bin/bash`
children accumulating zero CPU** — driver-shim processes from tests that never
reaped them. Unrelated to lock acquisition: no refusal was logged in any
round-8 copy, and the only entries on the new wait path were the two from this
task's own tests. Classified environment-blocked, aggravated by ~50 minutes of
4x64-thread load plus a concurrent CLI suite.

## 6. Gates

Per-crate via `scripts/run-tests.sh`, every cargo invocation under
`env -u ORGASMIC_RUN_ID -u ORGASMIC_HOME`, output to files, never
`ORGASMIC_ALLOW_BILLED_TESTS`.

| crate | verdict | notes |
| --- | --- | --- |
| `orgasmic-core` | **GREEN** | 0 failures |
| `orgasmic-drivers` | **GREEN** | 0 failures |
| `orgasmic-daemon` | **RED — 2, both environment** | see below |
| `orgasmic-cli` | **GREEN** | 0 failures, clean machine |
| registry `--check` | **OK** | 6 entries, every owner open (7 before the deletion) |

**`orgasmic-daemon`** — the only two "REAL" failures are the same tooling gate
in two binaries: `required_test_tooling_is_present`,
`required test tooling is missing: tmux (gates 9 tests)` /
`(gates 1 test)`, at `crates/orgasmic-drivers/src/modes/rmux.rs:508`. On this
PATH `tmux` resolves to the rmux shim
(`/var/folders/…/rmux-shim-…/tmux`), which the check rejects. A property of
running the suite from inside an rmux pane, not of this change — and it failed
in isolation too, which is the signature of an environment gate rather than a
race. Reported, not registered. All four tests this task touches passed in that
same clean run:

    test api::tests::deleted_or_corrupt_claim_reconstructs_exact_status_and_post ... ok
    test tests::daemon_lock_continuously_held_fails_closed ... ok
    test tests::daemon_lock_retries_a_transient_probe_hold ... ok
    test tests::daemon_lock_outwaits_a_holder_that_outlives_the_transient_budget ... ok

`supervisor::tests::poll_direct_child_pid_prefers_worker_server_over_generic_sibling`
also fired and was correctly classified **FLAKE** against TASK-STWVB — its
registered signature matched.

**`orgasmic-cli` — the first run was RED and I discarded it as invalid.** I had
started it while the harness was running, and it failed 2 tests at
**1-minute load average 97–108**, self-inflicted. Both were load artifacts:

- `exact_close_after_recovery_releases_the_replacement_run` — the daemon's own
  error text says it: `daemon request timed out after 30s (1m load average
  102.34) — the daemon may be healthy but the system is under load`.
- `sigterm_exits_through_graceful_shutdown_rather_than_default_disposition` —
  `serve died on SIGTERM's default disposition`. The test's own TASK-Q07Y5
  comment documents exactly this window. **Ruled out as mine by evidence, not
  by argument:** the CLI suite log contains **zero** occurrences of the new wait
  path's log line and **zero** refusals, so `acquire_daemon_lock` never left the
  fast path anywhere in that suite.

Re-run on an unloaded machine: **GREEN, 0 failures.**

## 7. New-task candidates

1. **`index::tests::live_rebuild_preserves_repo_url_and_schedules_post_bind_refresh`
   is red again under heavier load than TASK-5FEN5 measured**, with a *new*
   panic — 5FEN5's fix (retry the probe) is exhausted rather than bypassed:
   `the live Git probe for project never resolved
   ssh://git@example.com/org/project.git: every attempt returned without
   writing a URL`, `index.rs:2154`. **4 of 4 copies in the single slowest
   post-fix round** (201s/copy vs 102s baseline), 0 of 24 in every other round.
   Cannot be registered — 5FEN5 is closed, and an excuse with no open owner is
   the permanent exemption this registry forbids. Needs a task.
2. **The daemon suite leaks `/bin/bash` driver-shim children under sustained
   load** and can wedge a full run near completion — 31 zero-CPU strays, round
   8 above. Distinct from the tmux gate; worth a task because it makes long
   loaded rounds unreliable as evidence.
3. **`required_test_tooling_is_present` fails for every suite run from inside an
   rmux pane**, because the shim `tmux` is rejected. Every dispatched worker on
   this machine hits it and must argue it away by hand. Worth either an
   acknowledged skip path or a shim the check accepts.

## Residual risk

- **The transient holder of the api.rs restart was never identified.** The fix
  is holder-agnostic by construction, so this does not block it, but if the
  refusal ever fires again in production the pid probe is what will name it.
- **A wedged holder now costs a start 40s instead of 125ms.** Deliberate,
  logged while it happens, and stated in the artifact README. If an operator
  workflow depends on a fast refusal, this is the change that would surprise
  them.
- **`INCUMBENT_PROBE_INTERVAL` adds up to ~40 health probes to a failing
  start** where there used to be one. Each is bounded by
  `INCUMBENT_PROBE_TIMEOUT` and only runs against a lock already held, but it
  is new outbound traffic on a failure path.
- **Post-fix harness rounds are corroboration, not proof** — see the honest
  reading in section 5.

## Branch and HEAD

`task-tzkac-impl`, HEAD `a94ddfa`. Four commits:

| sha | what |
| --- | --- |
| `bc7bb51` | registry row re-owned J1XCB -> TZKAC (the asked-for first act) |
| `5c97409` | the protocol fix |
| `ce4e8e7` | `verify/TASK-TZKAC` injection proof |
| `a94ddfa` | registry row deleted, defect fixed |

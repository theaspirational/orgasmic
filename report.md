# TASK-Z7VQK — one pre-warmed shared fake agent for `dispatch_endpoint.rs`

Branch `task-z7vqk-impl`. The implementation, the verify artifact and the two
invariant tests are commit **`1b87384`**; anything after it is this report.

**Read the premise corrections before the design.** The fix does what the task
asked and is verified — and it also proves the task's diagnosis is not what is
making the named family red. Acceptance criterion 2 ("the family runs green
suite-concurrently") is **UNMET**, for a reason that lives in
`orgasmic-drivers`, with the evidence below.

## Premise corrections first

**1. The Gatekeeper cost is real, and bigger than the task states.** Measured on
this machine 2026-07-29 while three other agents held the load average between
18 and 45 — first exec of a two-line script, ten samples:

```
fresh script  : 157 1760 21942 617 235 243 260 403 ms
hard link      : 5 466 74 63 50 13 10 47 ms      (to an already-evaluated inode)
byte copy      : 157 ms                          (new inode ⇒ pays in full)
```

Spawned the way the daemon spawns a harness (stdin piped, timed to first stdout
line), ten installs each: fresh per-test fixture **153–1666ms**, shared
pre-warmed fixture **7–17ms**. The cache key is the inode, not the path, exactly
as `.orgasmic/gotchas.org` records.

**2. The named family's redness is NOT (mostly) that cost, and removing the cost
makes the family WORSE.** This is the finding that matters and it is
reproducible. Evidence, in order:

- Instrumented the fixture to record its own lifetime. In a failing run the
  harness **ran to completion in 9.5 ms** and printed all 16 of its lines:

  ```
  1785291384.515414000 start 19568 /var/…/bin/cursor-agent
  1785291384.524909000 end   19568
  subprocess-stream-json exit synthesis decision binary="cursor-agent"
    exit_code=Some(0) distill_is_some=false assistant_len=0 system_chunks=0
  ```

  The driver recorded **zero** output. The transcript was produced and lost, so
  no `run_complete` was synthesized and the run was flagged orphan. That is the
  `dispatch_endpoint.rs:2632` failure, and it is not a timeout in any meaningful
  sense — the wait expires long after the harness is dead.

- The same signature reproduces on the **unmodified** file: solo runs of
  `dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail`, 5 each,
  baseline **1 red / 4 green**, fixed **2 red / 3 green**, identical
  `system_chunks=0`. Pre-existing.

- Mechanism, by inspection:
  `crates/orgasmic-drivers/src/modes/subprocess_stream_json.rs:418` selects over
  stdout, stderr and a command channel; the command branch `break`s the loop on
  release *or* on the channel closing (`None`), **without draining stdout**, and
  `tokio::select!` picks at random among ready branches. `finalize_subprocess_exit`
  then distills an empty summary. A harness that exits fast enough loses its
  tail.

- Confirmed causally: a build identical to the fix except for a `/bin/sleep 0.2`
  before the harness body — i.e. the start latency Gatekeeper used to supply —
  suppresses most of the failures. Same load window, side by side:
  **fixed 4 failures / delayed-start 1 failure**.

So the ~160ms first-exec queue this task set out to remove was *masking* a
product race. Removing it is still right; it is not, on its own, sufficient to
make the family green, and on this machine it makes it redder.

## The design

One executable serves the whole file.

- `FAKE_AGENT_DISPATCHER` — a single `#!/bin/sh` file, published once beside the
  test binary and **exec'd once at install time**, where no deadline is running
  (`--orgasmic-fixture-warm`, asserted, TASK-GEZHQ's precedent). The answer is
  written to a `.warmed` receipt so "was it exec'd" is a fact on disk.
- Every fixture — `cursor-agent`, `worker-server`, `codex` — is a **hard link**
  to that file. Same inode, so the evaluation is already paid. A copy would not
  do: measured, a byte-identical copy pays the full 157ms.
- **Per-test behaviour survives** as a `.behaviour` data file beside each link,
  which the dispatcher **sources**. Sourcing is not exec'ing, so a per-test
  script is never evaluated at all. `$0` is the link the caller exec'd (verified
  empirically, and it is how `src/test_fixtures.rs` already finds its data), so
  each link reads its own test's script and no other's. Call sites are
  unchanged: `install_fake_cursor_agent(tmp, script)` still takes the same
  script text, shebang and all — the shebang is just a comment when sourced.
- The **hermetic cc-compiled harness** gets the same treatment: compiled once,
  pre-warmed once, hard-linked per test. Its two knobs (session id, generic
  sibling fork) moved from compile-time interpolation to **environment**
  variables, deliberately not argv: `dispatch_response_pid_prefers_worker_server_child`
  asserts the generic sibling can never match `worker-server`, and a fixture
  argv carrying `--session-id worker-server-pid` would appear in every child's
  `ps` line and defeat it.
- The shared file is keyed **by its own contents**, not by the test binary's
  name. The `test_fixtures.rs` precedent assumes the binary's name carries a
  build hash; measured false here — `dispatch_endpoint-1edf7bef84349123`
  survived edits to this file, so a name-keyed fixture is a stale fixture the
  moment its text changes. (Caught in flight by the content assert, which fired
  on a stale file mid-task.)
- No per-test executable identity or count is asserted anywhere in the file, so
  sharing breaks nothing (checked: the only identity assertions are the two
  `ps`-line ones above, which read `worker-server`/`cursor-agent` substrings).
- `wait_for_session_ready`'s 30s bound is untouched, and its doc comment now
  says what it is: a hang guard, not a budget Gatekeeper is spending. TASK-HAREX's
  release/drain timing and TASK-5FEN5's bound change are both left alone.

## Verification

### Verify artifact — `verify/TASK-Z7VQK/`

Two tests, one per property, because the injection breaks both:

- `every_fake_harness_is_the_same_executable` — inode identity of
  `cursor-agent`, `worker-server` and `codex` against the shared file.
- `the_shared_fake_agent_is_exec_d_before_any_test_waits_on_it` — the warm
  receipt exists and holds what the fixture answered.

`injection.patch` restores the pre-fix world in the three ways that made it cost
something: `copy` instead of `hard_link`, no warm-up, and a one-time
**inode-keyed** sleep standing in for the evaluation (keyed by inode because
that is how the real one is keyed — a path-keyed stand-in would be red even with
the fix and would prove nothing).

Self-tested before reporting:

```
$ ./target/debug/orgasmic verify TASK-Z7VQK --artifact verify/TASK-Z7VQK
  [tree]    clean
  [inject]  injection.patch applied
  [red]     as pinned — exit 101, signature matched
  [revert]  reverted; tree byte-identical
  [green]   passes without the injection — exit 0
verify TASK-Z7VQK: PASS — red-then-green replay reproduced
```

The probe deliberately does not drive a daemon: the end-to-end symptom is not a
reliable oracle while the drain race above is unfixed. `README.md` in the
artifact says so and pins the load measurements, per the 5FEN5 precedent.

### A/B, suite-concurrent (the acceptance's recipe)

Both arms are whole-file runs of the same tree's two binaries. Sequential arms
turned out to measure the machine, not the change (the same arm took 15s and
209s in consecutive rounds), so the honest rounds run **both arms side by side
in one window**, under a looping `orgasmic-daemon --lib --test-threads=64` plus
the machine's ambient load from other agents' work (load average 45–58).

| round | baseline (fresh per test) | fixed (shared, pre-warmed) |
|-------|---------------------------|-----------------------------|
| A (concurrent, load ~45) | 0 failed / 34 passed, 34s | 3 failed / 33 passed, 229s |
| B (concurrent, load ~49) | 0 failed / 34 passed, 37s | 2 failed / 34 passed, 112s |
| C (concurrent, load ~17) | 1 failed / 33 passed, 96s | 2 failed / 34 passed, 86s |
| 1 (sequential) | 0 failed, 115s | 1 failed, 40s |
| 2 (sequential) | 0 failed, 14s | 2 failed, 207s |

Totals across the five rounds: baseline **1** family failure, fixed **10**. Note
round C, taken after the other agents' work drained off the machine: the gap
narrows to 1 vs 2 — the effect scales with load, as a race should.

Failures, all rounds, are the named family and nothing else:
`dispatch_clean_worktree_protocol_end_without_finalize_orphans`,
`dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail`,
`dispatch_system_only_session_without_finalize_orphans_not_scrapes`,
`dispatch_early_exit_auto_releases_stuck_lease`,
`dispatch_delayed_protocol_end_without_finalize_orphans`.

Control, same window: fixed **4 failures** vs fixed-plus-200ms-start-delay
**1 failure** — the start latency, not the fixture's identity, governs the rate.

**Read this table as the finding, not as a regression in the fixture work.** The
fixed arm is redder because it is faster, and the driver loses a fast harness's
output. The wall-clock inflation (229s vs 34s) is downstream of that: a failing
cursor test holds the workspace environment lock for its whole 30s bound, so the
other eight queue behind it.

### Gates — `scripts/run-tests.sh`, per crate

| crate | verdict | failures |
|-------|---------|----------|
| `orgasmic-core` | GREEN | 0 |
| `orgasmic-cli` | GREEN | 0 |
| `orgasmic-drivers` | RED | 2 — `real_claude_stream_json_bridge_reports_auth_error` (real-claude smoke, `Elapsed`), `installed_harnesses_answer_their_own_readiness_probe` (asserts the machine's installed `cursor-agent` answers). Both environment-dependent, both in a crate this change does not compile into. |
| `orgasmic-daemon` | RED | run 1 (load ~46): 4 — 2 × `required_test_tooling_is_present` (tmux absent), `dispatch_protocol_end_without_finalize_orphans_and_leaves_artifacts_empty`, `dispatch_system_only_session_without_finalize_orphans_not_scrapes`. run 2 (load ~15): 5 — 2 × `required_test_tooling_is_present`, `recovery_reattaches_rmux_session_when_handle_exists` (QCG6J twins, green in isolation), `dead_pid_aborts_joins_hung_producer_then_receiver_releases` (J1XCB's set, green in isolation), `dispatch_subprocess_exit_synthesizes_run_complete_from_system_tail`. |

Classification: `required_test_tooling_is_present` × 2 — **environment-blocked**
(`tmux` resolves only to a temporary rmux shim dir on this machine and was gone
by gate time); QCG6J and J1XCB entries — **known families, reported not
registered**, per the brief; the drivers pair — **pre-existing/environment**;
the `dispatch_*` entries — **the family this report is about**, red for the
driver-side reason documented above, not for a fixture reason.

## Recommendation

The change is complete, verified and self-contained. It is also, on this
machine, a net increase in gate noise until the drain race is fixed, because the
delay it removes was sedating a real product bug. Two ways to sequence it, both
cheap, and the choice is the manager's:

1. Land it now and dispatch the driver fix immediately after — the noise it adds
   is true red pointing at a real defect, with the evidence already gathered.
2. Hold this branch until the driver fix lands, then land both — the gate gets
   quieter in one step instead of louder then quieter.

What should *not* happen is masking it from the fixture side (a trailing sleep
before the fake harness exits would make all six tests green and hide a product
defect that costs a real run its transcript).

## Changed

- `crates/orgasmic-daemon/tests/dispatch_endpoint.rs` — the fixture rework
  above; six inline `codex` stub installs collapsed into `install_fake_codex`;
  two new invariant tests; one diagnostic improvement to
  `wait_for_session_run_complete_summary` (it now prints what the session
  actually held, which is what made the diagnosis above possible); the
  `wait_for_session_ready` doc comment updated.
- `verify/TASK-Z7VQK/{injection.patch,cmd,expect-red,README.md}` — new.

One deliberate non-change: `dispatch_cleanup_releases_worker_and_deletes_worktree_branch`
installs a `codex` stub it never puts on the dispatch's `PATH`, so it drives
whatever harness the machine has. Left as found and named in a comment — making
it hermetic changes what that test dispatches, which is not this task's to
decide.

## New-task candidates

1. **P1/P2 — a fast-exiting harness loses its transcript and its `run_complete`.**
   `run_subprocess_stream_json` breaks its select loop on release/channel-close
   without draining stdout, and `select!` is unbiased. Product-visible: the run
   is flagged `protocol_end_without_finalize` and orphaned with an empty
   transcript. Evidence above. Candidate shape: `biased;` with stdout first,
   plus a bounded drain of stdout after `released` before
   `finalize_subprocess_exit`. Owner note: adjacent to TASK-HAREX's
   release/drain work; TASK-J1XCB is live in `supervisor.rs`, not here.
2. **P3 — `dispatch_cleanup_releases_worker_and_deletes_worktree_branch` is not
   hermetic** (above).
3. **P3 — `src/test_fixtures.rs` shares the stale-fixture hazard**: its shared
   file is keyed by the test binary's name, and that name is reused across
   rebuilds. It has no content assert, so a stale fixture there is silent.

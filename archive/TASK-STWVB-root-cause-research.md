# TASK-STWVB: independent root-cause research

Investigated 2026-09-07 at `216b28fd`. The research phase did not change source or ledger state.

## Finding

The strongest supported diagnosis is **a test that couples its correctness verdict to external process progress**. The remaining registered failure is a shell-fixture readiness timeout, before the production PID-selection function is called. A host launch stall can therefore fail the test without exercising the behavior its name describes.

The narrower assertion that **Gatekeeper's serialized first-execution checks are the root cause of the remaining flake is not established**. Historical observations make operating-system execution delays plausible, but neither the task's prose nor a green isolation rerun identifies the subsystem responsible. Current measurements do not reproduce the natural failure or the claimed fixed first-exec penalty.

There is a separate confirmed limitation in the gate: the remaining waiver matches a generic readiness-timeout string and a successful rerun. It does not establish why readiness failed. That explains why the runner can issue a verdict while the task still lacks a demonstrated causal diagnosis.

## Trace of the actual failure

1. [`shared_test_executable`](../crates/orgasmic-daemon/src/test_fixtures.rs#L204) checks a content-hashed fixture and runs its `warm` mode. This is already implemented, not proposed work.
2. The [test](../crates/orgasmic-daemon/src/supervisor.rs#L12631) spawns that executable in an owned process group, with `cursor-worker-sibling` and a private temporary readiness path.
3. The [shell mode](../crates/orgasmic-daemon/src/test_fixtures.rs#L96) backgrounds two sleeps and runs `/usr/bin/touch` to create the marker. All three executable paths are absolute; a concurrently altered PATH is not needed by this mode.
4. After spawn returns, the test allows 30 seconds for the marker. Failure produces `fake cursor-agent did not start children` and now an owned-process snapshot. Stderr is discarded; the message alone does not distinguish blocked execution, early exit, or marker-command failure.
5. Only **after readiness** does it call [`resolve_dispatch_watch_pid`](../crates/orgasmic-daemon/src/supervisor.rs#L12662). The registered panic cannot be attributed to a wrong result from that call: it has not run yet.

The shared-file initialization and `spawn()` occur before the readiness timer. Later PID resolution also calls synchronous `ps`/`pgrep` subprocesses. Consequently, a total test duration over 30 seconds with a passing result does not locate a stall inside the readiness phase.

## Evidence checked independently

| Evidence | What it establishes | What it does not establish |
|---|---|---|
| Current exact test: one pass, 0.21 s; `fresh_first_execs=0` | Current path works in this bounded execution and uses the existing fixture | Absence of a suite-only flake |
| Three sequential new-file controls: first direct exec 2.81–3.29 ms, second exec 2.58–3.28 ms; hard link and explicit interpreter also about 3 ms | The claimed universal 150–180 ms fresh-file penalty is absent in this probe | That historical macOS policy delays never occurred, or that parallel execution is safe |
| September 2 original tool results: two **passing** tests took 41.54 s and 43.40 s; logs have the same completion second | Correlated long test durations | A reproduced readiness failure, an identified blocked syscall, or Gatekeeper attribution |
| Preserved September 7 injected failure: stopped `/bin/sh`, state `T`, CPU `0:00.00`, failure after 30.11 s | An intentionally stopped fixture produces the registered symptom | A naturally occurring Gatekeeper stall |
| Replay that captured failure through the current runner: with registry, exit 0 / GREEN modulo flake; with an empty scratch registry, exit 1 / REAL; isolation passes in both | The registry exemption changes the verdict without causal host evidence | A same-binary live production regression being hidden; this is historical-log replay against the restored current test binary |

Raw September 2 evidence and historical qualifications are linked in [the companion investigation](TASK-STWVB-historical-evidence.md). The September 6 raw measurement files were also read: their quiet/CPU-pressure passes do not recreate an execution-policy stall.

## Hypotheses and discriminators

1. **Host-level process execution delay.** Predicts delayed unrelated process controls and a blocked execution path overlapping the readiness failure. Historical observations support this family of explanations; a current incident with phase timing and a process stack is still missing.
2. **Fixture-specific failure.** Predicts an exited/stopped wrapper, failing marker command, or blocked fixture operation while unrelated launches remain fast. The stopped-wrapper control demonstrates that the panic signature is compatible with this alternative. It is not evidence that this caused the historical incident.
3. **Cross-test state interference.** Predicts reproducibility with a particular co-running test or mutated state. The private marker path, argv mode, absolute executable paths, and failure before PID selection narrow this explanation. No discriminating current reproduction establishes or categorically eliminates it.

`S` process state and CPU time rounded to `0:00.00` do not prove that a process executed zero instructions, nor identify its wait channel. Likewise, passing alone is compatible with both resource contention and an application race.

## Why the existing host classifier does not close the diagnosis

The [live sampler](../scripts/run-tests.sh#L530) uses `pgrep -x syspolicyd` plus cumulative `ps -o time=`. It returned a real cumulative value during this investigation. The registry's claim that the sampler is dead is obsolete.

The [host judgment](../scripts/run-tests.sh#L755) requires a run-wide syspolicyd CPU rate of at least 1.50 CPU seconds per wall second over at least 10 seconds. Load alone does not trigger it. That is a CPU-utilization proxy, not an observation of an individual child's launch latency: a localized wait can occur without crossing the whole-run rate threshold. Conversely, a high rate does not attribute a particular panic to the scanner.

The [registered branch](../scripts/run-tests.sh#L1377) only requires a matching signature and a successful isolation rerun before counting a flake. Host degradation affects the final run verdict, but host evidence is not required for that registered match; the replay returned GREEN with host state unknown. This is the existing policy, not evidence of a newly introduced classifier regression.

Apple's [trusted-execution investigation guidance](https://developer.apple.com/forums/thread/706379) uses system-log evidence to diagnose specific policy problems. Its [Gatekeeper overview](https://support.apple.com/en-ph/guide/security/sec5599b66df/web) describes execution checks; neither source establishes this repository's claimed universal per-inode timing or serialized queue. Those claims need local evidence.

## Verification and remaining proof

Commands executed:

```sh
scripts/run-tests.sh -p orgasmic-daemon --lib supervisor::tests::poll_direct_child_pid_prefers_worker_server_over_generic_sibling
python3 /Users/aspirational/.codex/artifacts/orgasmic-stwvb-research-20260907/exec-probe.py
scripts/run-tests.sh --classify /Users/aspirational/.codex/artifacts/orgasmic-stwvb-diagnostic-20260907/injected-readiness-failure.log
```

The final command was repeated with an empty scratch registry. Logs, the runnable sequential probe, and JSON results are in `/Users/aspirational/.codex/artifacts/orgasmic-stwvb-research-20260907/`. A narrow recent syspolicyd-log query for the probe's temporary path returned no rows; this is not proof that no policy evaluation occurred. An initial wrapper invocation with an unsupported forwarded `--exact` layout failed on argument parsing before running a test; it was corrected and is excluded from bug evidence.

No full suite, deliberate scan storm, security-setting changes, provider calls, or daemon restart was used during research.

The missing decisive observation is **one natural readiness failure with timings around warm/spawn/marker, wrapper exit or wait state, and contemporaneous host execution-policy evidence**. Until then, classify the root-cause finding as test/host timing coupling with the OS trigger unresolved; do not claim a Gatekeeper fix, a newly broken sampler, or a diagnosed PID-selection race.

## Implemented follow-up

After the architectural direction was approved, PID selection was moved onto a deterministic `ProcessSnapshot`. Production now collects one bounded, kill-on-drop `ps` snapshot per poll and reports observation failure explicitly. Selection behavior is covered in memory; one tooling-locked integration check covers the real process adapter.

The runner now classifies the `test setup incomplete:` marker before registry lookup, preserves the isolation result as supporting evidence, and exits 4 rather than green. The former readiness-timeout registry entry was removed after the classifier self-test and both replacement daemon checks passed.

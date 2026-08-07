# verify/TASK-STWVB.1.1.1.1.1 — a killed cargo must never read as GREEN

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB.1.1.1.1.1`.

## Claim

The B2 arm guarded on `FAIL_COUNT -eq 0`, and its own comment says why:
*"cargo exits 101 whenever any test fails."* That justifies exempting **101**.
It does not justify exempting **every** non-zero code — and `FAIL_COUNT` is a
leaky proxy for the fact the comment already states.

A cargo killed by the OS (jetsam under the memory pressure this chain
documents) or by an operator `^C` exits 137/130 with the suite truncated.
`CRASH_COUNT` is 0 by construction — cargo itself died, so there is no failing
target to diff and no cause line to read, and round 5's log-derived detector
cannot see it. The only witness is the exit code, and that is exactly the
witness the guard gagged: one registered flake lifting `FAIL_COUNT` off zero
made the run print

```
  failures : 1
  crashed  : none — every failing target reported a failure list
  cargo    : exited 137 — ANOMALOUS: neither 0 nor libtest 101, the suite did not finish
  host     : calm (threshold syspolicyd_rate>=1.50; load corroborating only)
FLAKE (1) — green in isolation, registered signature matched:
verdict: GREEN modulo 1 registered flake(s). No unexplained red.
```

at **exit 0**, over a log the same live run stamped `# orgasmic-suite-exit:
137`. `ci.yml:197` gates on the exit code alone, so exit 0 merges. Identical
exit code, identical arm, opposite verdict — decided entirely by whether a
registered flake happened to fire.

The fix replaces the proxy with the fact: `SUITE_EXIT` in `{0, 101}` is
ordinary and classifies exactly as before, `?` (pre-stamp `--classify`) is
skipped, and anything else gets its own RED arm beside the crash arm, firing
whatever else classified. The `FAIL_COUNT` guard is **not** dropped and 101
stays exempt, which is the whole of M-1.

## Injection

The new arm is put back behind `[ "$FAIL_COUNT" -eq 0 ]` — F-1's exact shape,
one condition. The `cargo : exited 137 — ANOMALOUS` summary line still prints
under the injection; only the arm and therefore the exit code change, which is
precisely the exposure, since CI reads the exit code and not the block.

The FIRST failing selftest assertion is
`live path: same flake stub at exit 137 -> RED exit 1, the exit is named`, at
`exit 0, wanted 1`. Three more fail after it: the explicit
`a killed cargo never reads as GREEN modulo flake` assertion, the `--classify`
companion, and the degraded-host ordering case (which drops to INCONCLUSIVE
exit 4). Every other case passes under the injection — including all crash,
host-word, window, `--classify` flake and B2 cargo-exit cases — so the defect
is specific to an anomalous exit that coincides with a classified failure,
which is why five rounds of demonstrations missed it.

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh`, whose stub cargo emits one
registered, isolation-green failure and exits **137**. The case runs the
**live** path — `PATH="$TMP/bin:$PATH"`, no `--classify` — which is the path
`ci.yml:197` runs. No cargo, no money, about a second.

The same run pins the opposite direction with the **same stub at exit 101**
(`live path: same flake stub at exit 101 -> GREEN modulo flake, exit 0`), so
nothing but the exit code differs between the green and the red, and the fix
cannot be mistaken for re-dropping the `FAIL_COUNT` guard.

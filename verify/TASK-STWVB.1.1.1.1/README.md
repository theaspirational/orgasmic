# verify/TASK-STWVB.1.1.1.1 — a crashed target must never read as GREEN

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB.1.1.1.1`.

## Claim

`FAIL_COUNT` counts *listed* per-test failures. A binary that aborts or is
signalled dies with libtest, so it emits no `---- name ----` block and no
`failures:` list and counts **zero**. Every earlier crash detector in this
script keyed off cargo's exit code behind `FAIL_COUNT -eq 0` — which any other
classified failure switches off. On a board with seven registered flakes that
is the ordinary case, so a run holding a crashed binary *and* a registered
flake printed:

```
  failures : 1
CRASHED — nothing; the crash was named nowhere
FLAKE (1) — green in isolation, registered signature matched:
verdict: GREEN modulo 1 registered flake(s). No unexplained red.      exit 0
```

over a log containing `(signal: 6, SIGABRT: process abort signal)` and
`error: 2 targets failed`. `ci.yml` gates on the exit code alone, so **exit 0
merges**.

The fix detects the crash from the LOG — the `error: test failed, to rerun
pass` target set minus the targets that produced a parsed failure block, plus
cargo's crash-specific `process didn't exit successfully: … (signal: N, …)`
cause — and gives it its own `CRASHED (n)` section and its own RED arm above
`REAL_COUNT`, `FAIL_COUNT > 0` and `HOST_DEGRADED`. Being log-derived is what
makes it survive `--classify` and pre-stamp logs.

## Injection

The crash arm is put back behind `[ "$FAIL_COUNT" -eq 0 ]` — R-1's exact
shape, one condition. The `CRASHED` section still prints under the injection;
only the arm and therefore the exit code change, which is precisely the
exposure, since CI reads the exit code and not the block.

The FIRST failing selftest assertion is
`live path: crashed target + registered flake -> RED exit 1, crash named`,
at `exit 0, wanted 1`. Its `--classify` companion and the explicit
`crash + flake never reads as GREEN modulo flake` assertion fail after it.
Every other case — including the crashed-binary-alone case, the B2 cargo-exit
case and all thirteen `--classify` flake cases — still passes under the
injection: the defect is specific to crash-plus-flake, which is why four
rounds of demonstrations missed it.

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh`, whose stub cargo emits the
two-target shape real cargo produced in the round-4 review (one listed,
registry-matching failure; one aborted target with a `Caused by:` /
`(signal: 6, SIGABRT…)` cause) and exits 101. The case runs the **live** path
— `PATH="$TMP/bin:$PATH"`, no `--classify` — which is the path `ci.yml:197`
runs. No cargo, no money, about a second.

The same run pins the opposite direction: `live path: registered flake on calm
host -> GREEN modulo flake, exit 0` (M-1) still passes, so the fix cannot be
mistaken for re-dropping the `FAIL_COUNT` guard.

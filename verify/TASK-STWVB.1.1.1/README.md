# verify/TASK-STWVB.1.1.1 — restore the flake verdict on the live path

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB.1.1.1`.

## Claim

Round 3 moved the crashed-binary arm above `HOST_DEGRADED` (correct) but
dropped the `FAIL_COUNT == 0` guard that made its message true. `cargo test`
exits 101 whenever any test fails, so on the live path a registered,
isolation-green flake printed:

```
FLAKE (1) — green in isolation, registered signature matched:
verdict: RED — cargo exited 101 with no per-test failure list.
```

while `--classify` on the same log returned `GREEN modulo 1 registered flake`.
Two modes, one log, opposite verdicts. Restoring the guard keeps B2's position
and makes the live path and `--classify` agree.

## Injection

The `FAIL_COUNT -eq 0` conjunct is removed from the crashed-binary arm. Under
that ladder a live-path registered flake returns exit 1 with the false
"no per-test failure list" sentence. The FIRST failing selftest assertion is
`live path: registered flake on calm host -> GREEN modulo flake, exit 0`.

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh` with a stub cargo that
exits 101 *with* a failure list. No cargo, no money — about a second. The
thirteen `--classify` flake cases cannot see this defect (`SUITE_EXIT="?"`
short-circuits the arm); that is why it shipped.

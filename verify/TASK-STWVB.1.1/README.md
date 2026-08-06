# verify/TASK-STWVB.1.1 — judge syspolicyd on a rate, not run duration

`injection.patch` / `cmd` / `expect-red` are the replayable artifact:
`orgasmic verify TASK-STWVB.1.1`.

## Claim

`SYSPOLICYD_CPU_DEGRADED` as an absolute bound on cumulative CPU seconds is a
bound on how long the run took. Ambient accrual alone reaches 100 s in ~22
minutes; an in-run rate of 0.35–0.96 s/s crosses it in about two minutes. The
default whole-workspace invocation of `scripts/run-tests.sh` therefore returned
exit 4 on a calm host. Judging on CPU seconds per wall second of the sampled
window makes the signal duration-independent.

## Injection

`host_is_degraded` is restored to compare the absolute `syspolicyd_cpu` field
against `100.0`, ignoring `wall_s`. Under that comparison, a calm-host sample
that represents long ambient accrual (`syspolicyd_cpu=105.0,wall_s=1400` →
rate ≈ 0.075 s/s) reports DEGRADED. The FIRST failing selftest assertion is
`long ambient syspolicyd accrual is calm as a rate (absolute would trip)`.

## Why this pins the production path

The command drives `scripts/run-tests-selftest.sh` with
`ORGASMIC_HOST_STATE_SAMPLE` on the live path (stubbed `cargo`). No cargo, no
money — about a second. Under the fixed rate gate the case is calm / exit 0;
under the injection it is DEGRADED / exit 4.

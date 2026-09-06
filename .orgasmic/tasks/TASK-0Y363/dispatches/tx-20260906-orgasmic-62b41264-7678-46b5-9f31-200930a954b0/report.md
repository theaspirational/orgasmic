# Changed
- Added a Unix-only daemon diagnostic in `crates/orgasmic-daemon/src/lib.rs`:
  `tests::fd_exhaustion_starves_fresh_health_but_not_warm_connections_until_release`.
- The diagnostic boots only an owned child daemon on loopback/ephemeral port, lowers only the child `RLIMIT_NOFILE` after boot, fills descriptors to `EMFILE`, and repeats pressure/release twice.
- No production fd reserve, accept-loop policy, supervisor policy, lock replacement policy, public endpoint, deployment, daemon restart, or operator-daemon action was added.
- Commit: `e464ff13 Add fd exhaustion health diagnostic`.

# Verification Gates
- `cargo fmt`
- `ORGASMIC_FD_EXHAUSTION_OBSERVATIONS=/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-baseline-discriminator-observations.txt CARGO_BUILD_JOBS=3 CARGO_TARGET_DIR="$PWD/target/task-0y363" scripts/run-tests.sh --work-dir /Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-workdir-20260906210751 -p orgasmic-daemon --lib fd_exhaustion_starves_fresh_health_but_not_warm_connections_until_release`
  - Result: GREEN, 1 passed, 0 failed.
  - Wrapper log: `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-baseline-discriminator-run-tests.log`
  - Suite log: `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-workdir-20260906210751/suite.log`
  - Observation transcript: `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-baseline-discriminator-observations.txt`
  - Diagnostic report: `/Users/aspirational/.codex/artifacts/orgasmic-recovery-20260906/p0-diagnostic-report.md`
- Final `git diff --check`: clean.

# Diagnostic Observations
- Baseline warm and fresh `/api/healthz` plus authenticated `/api/daemon/status` succeeded before exhaustion.
- Cycle 1: child filled 33 descriptors at soft limit 47; child PID remained live; daemon instance lock remained held; warm health/status succeeded; fresh health/status failed with `Resource temporarily unavailable (os error 35)`; fresh health/status recovered after releasing 33 descriptors and restoring the limit.
- Cycle 2 repeated the same pressure/release result.
- Child daemon shutdown was bounded and reaped with exit status 0.
- This establishes accept/transport starvation: a missed fresh probe under `EMFILE` is not proof the daemon is dead, because an already-accepted warm path still works and the live owner still holds the lock.

# Unmet Criteria
- P0 production recovery is intentionally not fixed in this dispatch per parent scope decision.
- No discriminating production regression that fails without a selected production fix is retained, because this dispatch now delivers the baseline discriminator and diagnostic report only.
- Follow-up policy decision remains: how to make fresh health/status reliable under continuing or repeated descriptor pressure without ever treating a missed probe as permission to replace a live lock owner.

# Residual Risk
- The diagnostic is Unix-only and measured on this macOS worker; no Linux/CI resource-limit variant was run.
- The test lowers the child daemon limit after boot and uses an empty temp home; it does not reproduce the original multi-day session-handle leak, which was already fixed before this task.
- No production behavior changed, so runtime P0 exposure remains until a follow-up mitigation is selected and reviewed.

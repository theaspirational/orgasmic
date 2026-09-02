## Changed
- Removed the two `crates/orgasmic-cli/tests/dispatch.rs` flake entries from `verify/flake-registry.toml` after a fresh 2026-09-02 measurement at HEAD `512fcb08d040ab1a11e2fb19fd778fd68b51a10e`.
- Kept a compact registry comment recording the retirement measurement. No product or test code changed.

## Verification Gates
- Built the exact integration-test binary with the required private target-dir flag:
  `cargo test --target-dir /tmp/orgasmic-task-x0pv1-target-512fcb08 -p orgasmic-cli --test dispatch --no-run`
- Measurement: started 20 `/usr/bin/yes` CPU-pressure workers, then ran 20 rounds with one concurrent exact invocation of each test from `/tmp/orgasmic-task-x0pv1-target-512fcb08/debug/deps/dispatch-bf56d22ee7bf6eb7`:
  - `--exact dispatch_close_records_cleanup_failure_and_status_filter_lists_it --nocapture`: 20/20 passed.
  - `--exact dispatch_timeout_requests_daemon_cleanup --nocapture`: 20/20 passed.
  - Host: 10 logical CPUs; load average rose from 10.45 to 78.17 during the rounds.
  - Durable evidence: `/tmp/TASK-X0PV1-measure-512fcb08/summary.log` and per-round logs in the same directory; owning PID and exact pressure/test PIDs are recorded there.
- `python3`/`tomllib` parsed the edited registry: one valid entry remains.
- `git diff --check`: passed.
- `bash scripts/run-tests.sh --check`: registry phase passed (`registry: OK — 1 entries`), but the command exited 2 because unrelated existing injection proofs are stale (`artifacts: 70/102 replayable`). The same stale-artifact rejection was present before this edit. Log: `/tmp/TASK-X0PV1-check-512fcb08.log`.

## Unmet Criteria
- The full `scripts/run-tests.sh --check` command is not green because 32 pre-existing `verify/TASK-*` injection proofs reject as stale. Repairing those proofs is outside this dispatch's two-entry scope.
- TASK-X0PV1 cannot close despite the handoff's claim: `verify/flake-registry.toml` still contains `supervisor::tests::poll_direct_child_pid_prefers_worker_server_over_generic_sibling`, owned by TASK-X0PV1 and deliberately re-evidenced earlier today. Closing the task would violate the owner-lifecycle guard.

## Residual Risk
- The two deleted modes were previously observed only under full-workspace parallelism, which this laptop must not run. This targeted 40-test loaded sample did not reproduce either; any recurrence is intentionally classified REAL.
- This worktree has no `.orgasmic/tasks`, so `--check` reported that owner lifecycle was not checked locally. `orgasmic task get --project orgasmic TASK-X0PV1` confirmed the remaining owner's task is `in_progress`.

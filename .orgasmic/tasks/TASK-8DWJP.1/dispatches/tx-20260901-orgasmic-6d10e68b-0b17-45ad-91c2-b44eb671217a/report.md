## Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:84-197,268-353` now checks Git's unmerged index after every `pull --rebase --autostash`, reads paths with `git diff --name-only --diff-filter=U -z`, distinguishes active rebases from retained-autostash conflicts, parks the retained stash commit before dropping it, and resets only after the parked ref exists.
- `crates/orgasmic-daemon/src/ledger_sync.rs:560-582` runs conflict parking through the writer barrier; `ledger.sync_conflict` remains appended after the barrier returns.
- `crates/orgasmic-daemon/src/ledger_sync.rs:922-1120` asserts the literal `.orgasmic/machines/<id>/tx/<month>.org` route, modify/delete PATHS, retained local autostash bytes, remote reset, a clean second tick, and no markers on the remote.
- `crates/orgasmic-daemon/src/writer.rs:365-372,856-879,2386-2389` adds the inline writer `Barrier` command and typed `WriterHandle::run_barrier`; `writer.rs:3515-3571` proves an append queued during the barrier lands after the reset.
- Commit: `6692c2e6b982f5b7d9277e4a813849a124c4bcfd` (`TASK-8DWJP.1: fix(ledger-sync): park all conflicts behind writer barrier`).

## Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`: `23 passed; 0 failed; 814 filtered out`; `/tmp/TASK-8DWJP.1-daemon-final-3.log` (PID record `/tmp/TASK-8DWJP.1-daemon-final-3.pid`).
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: `22 passed; 0 failed; 282 filtered out`; `/tmp/TASK-8DWJP.1-cli-final.log` (PID record `/tmp/TASK-8DWJP.1-cli-final.pid`).
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: finished successfully; `/tmp/TASK-8DWJP.1-clippy-final.log` (PID record `/tmp/TASK-8DWJP.1-clippy-final.pid`).
- PASS — `cargo fmt --all --check`: exit 0; `/tmp/TASK-8DWJP.1-fmt.log` (PID record `/tmp/TASK-8DWJP.1-fmt.pid`).
- Classification evidence — two earlier parallel daemon-gate attempts timed out only in the pre-existing 10-second `two_daemon_loops_converge_through_the_bare_remote` deadline (`/tmp/TASK-8DWJP.1-daemon-final.log`, `/tmp/TASK-8DWJP.1-daemon-final-2.log`); the exact test passed in 4.45s (`/tmp/TASK-8DWJP.1-daemon-flake-repro.log`) and the final full targeted gate passed, so classified load-sensitive rather than a regression.

## Unmet Criteria

- None.

## Residual Risk

- Optional LOW 5 conflict-ref count in `daemon status` was skipped to keep the fix scoped; conflict outcome/reason reporting is unchanged.
- The optional extra index-visibility assertion was not added; the test hard-codes and parses the required indexed machine tx route.

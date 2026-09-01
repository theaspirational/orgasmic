# Changed

- `crates/orgasmic-core/src/tx.rs:1231-1264` adds the requested duplicate legacy tx-id fold case: two machines start generations with `tx-20260901-orgasmic-0007`, followed by one `CLOSED_TX`.
- The test documents and pins the existing fold contract: `CLOSED_TX` selects the latest matching start, so that generation closes while the earlier duplicate remains visibly open rather than both being closed.
- Commit: `753136c05a32c31f2ca936b463d5c9766b9aef67` (`TASK-MSYN4.3.1: test(tx): pin duplicate legacy tx id fold`).
- Skipped the optional `TxIdPolicy::ProjectSequence` cosmetic rename to keep this test-only.

# Verification Gates

- PASS — `cargo test -p orgasmic-core --lib tx`: `26 passed; 0 failed`; `/tmp/TASK-MSYN4.3.1-core.log` (owner PID recorded in `/tmp/TASK-MSYN4.3.1-core.pid`).
- PASS — `cargo clippy -p orgasmic-core --all-targets -- -D warnings`: finished successfully; `/tmp/TASK-MSYN4.3.1-clippy.log` (owner PID recorded in `/tmp/TASK-MSYN4.3.1-clippy.pid`).
- PASS — `cargo fmt --all --check`: exit 0; `/tmp/TASK-MSYN4.3.1-fmt.log` (owner PID recorded in `/tmp/TASK-MSYN4.3.1-fmt.pid`).
- PASS — `git diff --check`; post-commit worktree is clean.

# Unmet Criteria

- None.

# Residual Risk

- Legacy duplicate numeric tx ids remain inherently ambiguous: the fold resolves a close to ledger-latest and leaves the earlier generation open; this test exposes that behavior but intentionally changes no production code.

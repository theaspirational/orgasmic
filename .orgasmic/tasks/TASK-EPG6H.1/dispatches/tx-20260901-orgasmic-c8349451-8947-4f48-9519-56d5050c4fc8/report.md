# Changed

- `crates/orgasmic-cli/src/manager.rs:9762-9826` retains each candidate close timestamp and checks only that task's node journal. A `task.state_transitioned` at or after the close now removes the torn-close candidate; same-second journal entries win as required.
- `crates/orgasmic-daemon/src/api.rs:18497-18559` applies the same per-task journal guard to `recorded_close_allows_repair`, reusing `crate::index::journal_tx_entry`.
- `crates/orgasmic-daemon/src/index.rs:3731` exposes `journal_tx_entry` within the crate instead of duplicating conversion.
- `crates/orgasmic-daemon/src/api.rs:18418-18436` applies the Done Evidence gate to legacy repair requests. The atomic dispatch-close endpoint remains separate; its existing one-transaction regression passes.
- Live-layout regressions are at `crates/orgasmic-cli/src/manager.rs:12332-12399`, `crates/orgasmic-daemon/src/api.rs:22458-22517`, and `crates/orgasmic-daemon/src/api.rs:32983-33113`.
- Commit: `1139bc0f22c712064c0b2ada6c05044ebffdebfb` (`TASK-EPG6H.1: fix(dispatch): honor journal and evidence repair guards`).

# Verification Gates

- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- torn_close`: `1 passed; 0 failed`; `/tmp/TASK-EPG6H.1-cli-torn-close-final.log`.
  - The brief's literal `--lib` command is not runnable because `orgasmic-cli` has no library target (`error: no library targets found`); evidence: `/tmp/TASK-EPG6H.1-cli-torn-close.log`. The equivalent binary unit-test target above passed.
- PASS — `cargo test -p orgasmic-daemon --lib -- repair`: `5 passed; 0 failed`; `/tmp/TASK-EPG6H.1-daemon-repair-final.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- evidence`: `6 passed; 0 failed`; `/tmp/TASK-EPG6H.1-daemon-evidence-final.log`.
- PASS — `cargo test -p orgasmic-daemon --lib -- ap971_dispatch_close_is_one_tx_append_plus_one_node_rewrite`: `1 passed; 0 failed`; `/tmp/TASK-EPG6H.1-daemon-atomic-close.log`.
- PASS — daemon tooling sentinel: `1 passed; 0 failed`; `/tmp/TASK-EPG6H.1-daemon-tooling-sentinel.log`.
- PASS — `cargo clippy -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`: `Finished dev profile`; `/tmp/TASK-EPG6H.1-clippy.log`.
- PASS — `cargo fmt --all --check`: `exit=0`; `/tmp/TASK-EPG6H.1-fmt-check.log`.
- PASS — `git diff --check`; worktree clean after commit.

# Unmet Criteria

None.

# Residual Risk

Only the scoped tests and requested lint/format gates ran. The full workspace suite was intentionally not run per the dispatch constraints.

# Changed

- `crates/orgasmic-daemon/src/api.rs:8772,24129-24336` removes the dead dispatch-type routing short-circuit, shares the exhaustive behavior pin table with the shipped-schema test, and derives the documented dispatch lifecycle set by subtracting the named non-dispatch tx types and `*.deleted` routes.
- `crates/orgasmic-cli/src/project_migrate.rs:45-53,61-89` deletes the unreachable `anomalies` counter and output line; the existing failing-file `bail!` remains authoritative.
- `crates/orgasmic-daemon/src/writer.rs:857-898,1425-1434` makes projection-failure ownership optional, leaves ownerless journal failures queue-only, removes the foreign-owner warning, and tests that a plain comment journal write creates no phantom owner.
- `crates/orgasmic-cli/src/project_migrate.rs:397-514,741-756` tracks branch/worktree/source-removal cutover progress and attaches exact per-run undo commands. The test forces failure after orphan-branch creation and proves only the branch-delete recovery step is printed.
- `crates/orgasmic-daemon/src/api.rs:32911-32994` sends request A through `refresh_after_tx`, pins its committed 503 and tx id, proves request B succeeds, and covers the ownerless comment path.
- Commit: `6d826636` (`TASK-SRBGS.1.1: fix(follow-ups): make routing and recovery failures honest`).

# Verification Gates

- PASS — daemon focused filters (`shipped_tx_types`: 1 passed; `apply_failure`: 2 passed; `ledger_route`: 1 passed; `comment`: 19 passed): `/tmp/TASK-SRBGS.1.1-daemon-gate.log`.
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- project_migrate`: `test result: ok. 3 passed; 0 failed`; `/tmp/TASK-SRBGS.1.1-cli-gate.log`.
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: `Finished dev profile`; `/tmp/TASK-SRBGS.1.1-clippy.log`.
- PASS — `cargo fmt --all --check`: exit 0; `/tmp/TASK-SRBGS.1.1-fmt.log`.
- PASS — daemon tooling sentinel: `required_test_tooling_is_present ... ok`; `/tmp/TASK-SRBGS.1.1-tooling-sentinel.log`.
- RED-THEN-GREEN proof — temporarily added `("test.fake_dispatch", false)` to `PINNED_LEDGER_ROUTES` without a shipped bullet. `shipped_tx_types_match_rust_routes_to_journal` failed with exit 101 and the right-hand set containing `test.fake_dispatch`; `/tmp/TASK-SRBGS.1.1-route-drift-red.log`. The fake entry was reverted; the official daemon gate then passed the shipped-type test.

# Unmet Criteria

- None.

# Residual Risk

- Verification was intentionally focused; no workspace-wide or unfiltered `orgasmic-cli` suite was run, per the dispatch rules.
- The branch-cutover regression injects the required post-branch-creation failure. The additional worktree-removal and source-restore recovery branches are progress-derived but were not separately fault-injected.

## Changed
- Added `orgasmic manager dispatch-status --json` with separate typed branches for torn-close reconciliation, cleanup failures, open dispatches, and managed worktrees.
- JSON preserves all human-status data, uses `null` for absent values, and includes model, effort, preflight, claims, parked tasks, awaiting-merge disposition, gone-run exit/evidence, and main checkout dirtiness.
- Kept the human renderer byte-compatible and added typed round-trip plus exact-render tests.
- Added `shipped/skills/orgasmic/proposals/operations-dispatch.md` through `okfy propose` for the changed CLI surface, per the bundle write policy.

## Verification Gates
- `cargo test -p orgasmic-cli --bin orgasmic`: PASS, 345 passed, 0 failed, 1 ignored (`/tmp/TASK-CS2TM-cli-bin.log`, owner PID 15368).
- `cargo test -p orgasmic-cli --bin orgasmic dispatch_status_`: PASS, 5 passed (`/tmp/TASK-CS2TM-final-unit.log`, owner PID 4026).
- `cargo clippy -p orgasmic-cli --all-targets -- -D warnings`: PASS (`/tmp/TASK-CS2TM-final-clippy.log`, owner PID 6050).
- `cargo fmt --all --check`: PASS.
- Live primary-root probe with the worktree binary: `dispatch-status --task TASK-CS2TM --json` returned valid JSON with all four top-level branches and one open TASK-CS2TM record (`/tmp/TASK-CS2TM-live-json.log`).
- Live human compatibility probe: installed runtime stdout and the worktree binary stdout compared byte-identical (`cmp` exit 0; `/tmp/TASK-CS2TM-human-installed.log`, `/tmp/TASK-CS2TM-human-new.log`).
- `okfy validate shipped/skills/orgasmic --all`: PASS (`ok: true`, 0 errors); one expected stale-package warning remains while the proposal awaits owner acceptance.

## Unmet Criteria
- None.

## Residual Risk
- The OKF change is intentionally a proposal, not a direct concept edit; the bundle owner must accept it and repackage to clear `W_STALE_PACKAGE`.

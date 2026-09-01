# Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:40-69` now migrates only daemon-synced `orgasmic` ledgers with an `origin`: it preserves existing `.orgasmic/.gitignore` bytes, appends a missing `views/` rule, and runs the required cached untrack before the existing stage/commit path.
- `crates/orgasmic-daemon/src/ledger_sync.rs:369-411` seeds a tracked derived view, proves the first sync appends/untracks without deleting the file, and proves a second sync creates no commit.
- `crates/orgasmic-daemon/src/index.rs:395-396,428,841-873,1176-1178` adds the requested dirty-root set and scheduled flag; changed node reloads coalesce for 200 ms, rebuild each project in `spawn_blocking`, and warn on failures.
- `crates/orgasmic-daemon/src/index.rs:5782-5810` proves a new task written through `apply_written_path`, with no claims write, appears in `views/board.org` after the debounce.
- No documentation change was needed: `shipped/skills/orgasmic/references/ledger.md:23` is now true for synced ledgers, and the cited claim is absent from `shipped/entry/router.org`.
- Commit: `49de897f09b57ff64c3bfb2c91ca3206bdb4c9ba` (`TASK-JWHXH.1: fix(daemon): refresh and ignore derived views`).

# Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync views`: `8 passed; 0 failed`; includes both new regressions. Log: `/tmp/TASK-JWHXH.1-test-gate-exact.log`.
- PASS — `cargo clippy -p orgasmic-daemon --all-targets -- -D warnings`: `Finished dev profile`. Log: `/tmp/TASK-JWHXH.1-clippy.log`.
- PASS — `cargo fmt --all --check`: exit 0. Log: `/tmp/TASK-JWHXH.1-fmt.log`.
- PASS — `git diff --check`; committed worktree is clean.
- A development-only focused compile initially failed because a local variable named `display` collided inside `tracing::warn!`; classified as an introduced regression, fixed by renaming it, and all required gates above passed afterward. Log: `/tmp/TASK-JWHXH.1-ledger-test.log`.

# Unmet Criteria

- None.

# Residual Risk

- Debounced rebuild failures are warning-only, as allowed by the brief; a transient failure leaves views stale until the next node write or full refresh.

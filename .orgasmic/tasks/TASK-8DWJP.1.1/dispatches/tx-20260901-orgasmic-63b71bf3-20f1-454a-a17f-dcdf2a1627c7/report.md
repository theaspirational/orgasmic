## Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:104-108,311-468` now checks the UU index before any staging, identifies a matching retained autostash or existing parked ref, reuses parked recovery state across interrupted ticks, verifies `stash@{0}` identity immediately before dropping it, and resets unrecoverable conflicts to fetched `origin/orgasmic` with an explicit status message.
- `crates/orgasmic-daemon/src/ledger_sync.rs:374-403,694-711` fetches before the writer barrier and best-effort pushes the parked ref after it; the barrier closure contains only local Git recovery.
- `crates/orgasmic-daemon/src/ledger_sync.rs:613` records conflict paths with tab separators.
- `crates/orgasmic-daemon/src/ledger_sync.rs:1215-1373` covers interruption after stash drop, leftover UU state under another machine, foreign-stash identity mismatch/no-drop, remote marker exclusion, and parked-byte retention.
- `crates/orgasmic-daemon/src/writer.rs:2427-2430,3613-3622` catches barrier panics, always replies, and proves the writer accepts the next barrier.
- Commit: `bcb516c9 TASK-8DWJP.1.1: fix(ledger-sync): make conflict recovery re-entrant`.

## Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`: 26 passed, 0 failed, 815 filtered out. Log: `/tmp/TASK-8DWJP.1.1-gate-daemon-final.log`.
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: 22 passed, 0 failed, 282 filtered out. Log: `/tmp/TASK-8DWJP.1.1-gate-cli-final.log`.
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`. Log: `/tmp/TASK-8DWJP.1.1-gate-clippy-final.log`.
- PASS — `cargo fmt --all --check`. Log: `/tmp/TASK-8DWJP.1.1-gate-fmt-final.log`.
- The first clippy attempt found one new test-only needless borrow; it was fixed before the final passing gate. Failed-attempt log: `/tmp/TASK-8DWJP.1.1-gate-clippy.log`.
- Worktree is clean after commit.

## Unmet Criteria

- None.
- Optional doctor recovery text was skipped because it is outside the two assigned source files.
- Optional conflict-status propagation of a failed best-effort parked-ref push was skipped; the failure remains warned in tracing.

## Residual Risk

- Parked-ref publication remains best-effort by design: a network failure after local recovery leaves the local parked ref durable but requires a later recovery tick or manual push to publish it remotely.
- `project.org` and `gotchas.org` were not present in this dispatched checkout; the task record was read through `orgasmic task get --project orgasmic TASK-8DWJP.1.1` and implementation stayed within the referenced files.

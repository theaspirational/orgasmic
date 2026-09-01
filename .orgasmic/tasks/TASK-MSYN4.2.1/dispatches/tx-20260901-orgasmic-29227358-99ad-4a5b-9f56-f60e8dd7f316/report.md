## Changed
- `crates/orgasmic-daemon/src/ledger_sync.rs:109-144` untracks all three writer-sidecar glob families before staging and corrects the torn-window ceiling comment to name both orders and successful-sync/backoff limits.
- `crates/orgasmic-daemon/src/ledger_sync.rs:339-352,444-484` preserves `last_success_at` on `Idle` and prunes statuses not present in the current ledger set each tick.
- `crates/orgasmic-daemon/src/ledger_sync.rs:739-828` covers tracked-sidecar deletion/clean-tree behavior, idle success timestamps, and removed-ledger pruning.
- `crates/orgasmic-cli/src/doctor.rs:144-150,273-313,738-768` decodes the shared ledger-sync status map and emits one warning per failed, backed-off, or conflicted ledger using the daemon-status line shape and first error line.
- `crates/orgasmic-cli/src/daemon_lifecycle.rs:102-108` keeps the existing shared `LedgerSyncStatus` usable in doctor's comparable daemon status value; no duplicate status type was added.
- Commit: `be2f3868ef8e6c0a12288eb5d9c0bf05f1b5c622`.

## Verification Gates
- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync status`: `test result: ok. 19 passed; 0 failed; ... 814 filtered out`; log `/tmp/TASK-MSYN4.2.1/daemon-ledger-sync-status.log` (`EXIT:0`).
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- doctor daemon_lifecycle`: `test result: ok. 46 passed; 0 failed; ... 257 filtered out`; log `/tmp/TASK-MSYN4.2.1/cli-doctor-daemon-lifecycle.log` (`EXIT:0`).
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: `Finished dev profile`; log `/tmp/TASK-MSYN4.2.1/clippy-daemon-cli.log` (`EXIT:0`).
- PASS — `cargo fmt --all --check`: log `/tmp/TASK-MSYN4.2.1/fmt-check.log` (`EXIT:0`).
- PASS — `git diff --check`; committed worktree is clean.

## Unmet Criteria
- None.

## Residual Risk
- The documented two-command staging window remains until the next successful sync; backoff can extend it to `MAX_BACKOFF`, and a wedged ledger indefinitely. The requested barrier/lease redesign remains intentionally out of scope.
- No live daemon was started and no workspace-wide suite was run, as explicitly prohibited by the dispatch brief.

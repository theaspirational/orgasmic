# Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:46-467` adds `SyncOutcome::Conflict`, parses only Git `CONFLICT (...) ... in <path>` pull failures, aborts the rebase, salvages/stages with the existing excludes, parks a collision-safe machine/timestamp ref, best-effort pushes it, fetches and hard-resets the daemon-owned ledger worktree to `origin/orgasmic`, records conflict status without backoff, and appends one `ledger.sync_conflict` event through `WriterHandle`.
- `crates/orgasmic-daemon/src/ledger_sync.rs:706-857` proves two conflicting writers survive (remote live plus local parked), status is `conflict` with zero failures, the event carries `PARKED_REF`, the next tick syncs, and a missing remote remains an ordinary failed/backed-off path.
- `crates/orgasmic-daemon/src/lib.rs:1105-1111` threads the existing writer into the sync loop.
- `crates/orgasmic-cli/src/main.rs:2790-2800` prints conflict status and its parked ref on one daemon-status line; `crates/orgasmic-cli/src/daemon_lifecycle.rs:1234-1259` pins conflict status decoding.
- `crates/orgasmic-daemon/src/writer.rs:1715-1718`, `shipped/skills/orgasmic/references/ledger.md:27-40`, and `shipped/schema/tx.org:74-84` replace the false claim-barrier premise and document the machine-routed event.
- Implemented the existing recorded decision `dec_EWY0K` (already folded with TASK-AS0FS). Commit: `fa8ef1f9`.

# Verification Gates

- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict`: 16 passed, 0 failed. Log: `/tmp/TASK-8DWJP-daemon-gate-literal.log`.
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: 22 passed, 0 failed. Log: `/tmp/TASK-8DWJP-cli-gate.log`.
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`. Log: `/tmp/TASK-8DWJP-clippy.log`.
- PASS — `cargo fmt --all --check`. Log: `/tmp/TASK-8DWJP-fmt.log`.
- Development-only first compile failed on three test assertion reference types; corrected and the focused retry passed 9/9. Logs: `/tmp/TASK-8DWJP-ledger-sync-test.log`, `/tmp/TASK-8DWJP-ledger-sync-test-retry.log`.

# Unmet Criteria

None.

# Residual Risk

A writer can still land new bytes after the conflict salvage commit and before `reset --hard origin/orgasmic`; that narrow write-loss window is not fenced by this task. The parked commit itself remains durable locally, while its remote side-ref push is intentionally best-effort.

## Changed
- `crates/orgasmic-daemon/src/ledger_sync.rs:99-115` now checks `origin`, aborts an interrupted `orgasmic` rebase before the detached-HEAD idle gate, and continues through the normal conflict path.
- `crates/orgasmic-daemon/src/ledger_sync.rs:349-417` makes parked-ref matching require a present stage 3, preserves the identity-verified autostash fallback for delete/modify conflicts, and excludes salvage refs from parked candidates.
- `crates/orgasmic-daemon/src/ledger_sync.rs:420-604` records parked-ref push failures, drops an identity-matched orphan autostash on re-entry, and snapshots allowed ledger paths through a scratch index into a local `*-salvage` ref before reset.
- `crates/orgasmic-daemon/src/ledger_sync.rs:645-783` names salvage refs and unpushed parked refs in status and emits `SALVAGE_REF` in the conflict event.
- `crates/orgasmic-daemon/src/ledger_sync.rs:1274-1815` covers mid-rebase recovery, tracked post-pull task/tx salvage, strict delete/modify matching, orphan stash cleanup, push-status diagnostics, non-empty parked refs, and tracked post-conflict writes.
- Commit: `b273c46514ac4fd4a43d1e80b636073aa2e8008e` (`TASK-8DWJP.1.2: fix(ledger-sync): preserve conflict recovery writes`).

## Verification Gates
- PASS — `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`: `30 passed; 0 failed`; `/tmp/TASK-8DWJP.1.2-daemon-gate.log`.
- PASS — `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: `22 passed; 0 failed`; `/tmp/TASK-8DWJP.1.2-cli-gate.log`.
- PASS — `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: finished successfully; `/tmp/TASK-8DWJP.1.2-clippy.log`.
- PASS — `cargo fmt --all --check`: exit 0; `/tmp/TASK-8DWJP.1.2-fmt.log`.

## Unmet Criteria
- None.

## Residual Risk
- Salvage refs are intentionally local as required; an operator must inspect them in the affected ledger clone.
- A process kill can leave a uniquely named scratch-index file in the system temp directory; normal success and error returns remove it, and ledger refs/worktree data are unaffected.

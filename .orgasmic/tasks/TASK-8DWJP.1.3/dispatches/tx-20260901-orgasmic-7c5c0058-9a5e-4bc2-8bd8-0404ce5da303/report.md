# Changed

- `crates/orgasmic-daemon/src/ledger_sync.rs:103-116,208-212,349-372`: both owned-ledger `rebase --abort` paths now call one `abort_rebase_with_salvage` helper first. The helper reads `rebase-merge/orig-head` or `rebase-apply/orig-head`, falls back to `ORIG_HEAD`, snapshots against that explicit base, then aborts.
- `crates/orgasmic-daemon/src/ledger_sync.rs:61-65,509-554`: `ConflictSource::Worktree` carries the pre-abort salvage ref into the existing conflict outcome, status, and tx-event path; an unchanged tree against the rebase base keeps an empty salvage ref.
- `crates/orgasmic-daemon/src/ledger_sync.rs:405-409,705-708`: removed the unreachable entry-path rebase branch and changed status wording to `raw worktree snapshot at <ref> (conflicted paths carry markers)`.
- `crates/orgasmic-daemon/src/ledger_sync.rs:1309-1425,1508-1617`: moved the `conflicting_two_writer_tick` tracked write before its conflict tick and added a real interrupted-rebase regression that writes both a tracked task node and the machine tx file during the outage, then proves the status-named salvage ref contains both.
- Commit: `5846e9bb TASK-8DWJP.1.3: fix(ledger-sync): salvage before rebase abort`.

# Verification Gates

- Red probe: `cargo test -p orgasmic-daemon --lib -- ledger_sync::tests::mid_rebase_tick_aborts_and_recovers_instead_of_idling` failed before the production change because the salvage ref contained `tracked base` instead of `tracked during outage`; `/tmp/TASK-8DWJP.1.3-red-mid-rebase.log` (PID 76071).
- Green probe: the same exact test passed: `1 passed; 0 failed`; `/tmp/TASK-8DWJP.1.3-green-mid-rebase.log` (PID 82572).
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`: `30 passed; 0 failed`; `/tmp/TASK-8DWJP.1.3-gate-daemon-final.log` (PID 10874).
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`: `22 passed; 0 failed`; `/tmp/TASK-8DWJP.1.3-gate-cli.log` (PID 21408).
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`: passed (`Finished dev profile`); `/tmp/TASK-8DWJP.1.3-gate-clippy.log` (PID 23394).
- `cargo fmt --all --check`: passed (exit 0); `/tmp/TASK-8DWJP.1.3-gate-fmt.log` (PID 24967).
- Two development gate attempts exposed the now-invalid expectation that every in-tick modify/delete conflict creates a salvage ref even when its raw tree equals `orig-head`; classified as implementation-test regressions and fixed by the requested LOW 3 move. Evidence: `/tmp/TASK-8DWJP.1.3-gate-daemon.log`, `/tmp/TASK-8DWJP.1.3-gate-daemon-pass.log`. The final exact gate is green.
- Post-commit `git status --short --branch`: clean branch `task-8dwjp.1.3-impl`; `git show --check HEAD`: clean.

# Unmet Criteria

None.

# Residual Risk

The regression exercises the `rebase-merge/orig-head` path used by the real conflicting pull. The `rebase-apply/orig-head` and `ORIG_HEAD` fallback variants are implemented but not independently fixture-tested.

# TASK-8DWJP.1.3 — salvage the worktree before every `rebase --abort` (narrow round)

Read the task first: `orgasmic task get --project orgasmic TASK-8DWJP.1.3`. Scope is one move
plus three small LOWs. Do not widen it. Line numbers are approximate; read the current
`crates/orgasmic-daemon/src/ledger_sync.rs`.

## The move
`git rebase --abort` hard-resets the worktree. Two call sites do it with uncommitted tracked
writes possibly present: the entry-path abort (~:102-107, added last round) and the in-tick
abort after a conflicting pull (~:200-202). Before EACH abort, call the existing
`salvage_worktree` with an explicit base commit = the rebase's orig-head (read
`rebase-merge/orig-head` / `rebase-apply/orig-head` via `rev-parse --git-path`, else
`ORIG_HEAD`). Skip when the salvage tree equals the base tree. Carry the salvage ref into
the status/event exactly as the conflict path already does (`SALVAGE_REF`, status text).
If the two sites can share one small helper (`abort_rebase_with_salvage`), do that.

Test: real mid-rebase interruption (run a conflicting `pull --rebase --autostash`, do NOT
abort), then during the "outage" modify a tracked non-conflicted task node AND append a
line to `machines/<id>/tx/<month>.org`; run `sync_once`; assert both are readable from a
salvage ref and the status names it. Reverting the hoist must turn this red.

## LOWs
- Status wording (~:663-671): `raw worktree snapshot at <ref> (conflicted paths carry markers)`.
- Delete the now-unreachable `rebase_in_progress` branch in `conflict_source_on_entry`
  (~:373-378) and fold the `ConflictSource::Worktree` arm if only the in-tick producer remains.
- Optional: move the tracked write in `conflicting_two_writer_tick` to before the conflict
  tick, or say it is covered by `conflict_recovery_salvages_tracked_writes_made_after_pull`.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`
(`two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline; rerun alone before
calling a timeout a failure.)

## Rules
- Work only in your worktree; one commit `TASK-8DWJP.1.3: fix(ledger-sync): <one line>`.
- `git reset --hard` / `git stash drop` / `rebase --abort` appear ONLY inside the sync path
  against the ledger worktree the daemon owns. Never run them anywhere else.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).

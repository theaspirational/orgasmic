# TASK-8DWJP.1.4 — run the rebase abort under the writer barrier (round 6, narrow)

Read the task first: `orgasmic task get --project orgasmic TASK-8DWJP.1.4`. Scope is one MEDIUM
and three LOWs from the 8DWJP.1.3 review (opus-5 high). Do not widen it. Line numbers are
approximate; read the current `crates/orgasmic-daemon/src/ledger_sync.rs`.

## MEDIUM — the abort is not barriered
`abort_rebase_with_salvage` (`~:369-373`) is called from `sync_once_with_park` at the entry
path (`~:107`) and in-tick (`~:211`). Both run on the blocking thread with writers live; only
`park_conflict` runs inside `barrier_writer.run_barrier` (`~:895-903`). A writer `rename()` onto
`machines/<id>/tx/<month>.org` between `salvage_worktree`'s `git add` and `git rebase --abort`
is hard-reset away with no copy in any ref. Fix: run the salvage+abort under the same barrier.
Laziest shape: generalise the `park` closure into one `under_barrier` closure
(`FnMut(Box<dyn FnOnce() -> Result<T> + Send>) -> Result<T>` or two concrete closures — pick the
smaller diff) so both `abort_rebase_with_salvage` and `park_conflict` run inside
`run_barrier`. Tests that call `sync_once_with_park` directly pass an identity closure.
Test: a writer append issued during the barrier must land AFTER the abort and survive (reuse
the barrier test harness in `writer.rs` / the existing two-writer test shape).

## LOWs
- Empty unmerged set at entry (`~:103-117`): keep the minted ref in a `pending_salvage` local.
  If the tick later parks, add it to the status text and to the `ledger.sync_conflict` event
  (extra `PENDING_SALVAGE_REF`); otherwise `tracing::warn!` it and mention it in the status.
- Salvage failure at `~:370`: decide the trade and write it down. Preferred: degrade —
  `tracing::warn!`, `salvage_ref = String::new()`, still abort (keeps 1.2's unwedge guarantee).
  Either way add a `ponytail:` comment naming the choice.
- Restore the two-line `SALVAGE_REF` event assertion in `conflicting_two_writer_tick`
  (`~:1417`); the test still produces a non-empty `salvage_ref` (source `Autostash`).
- NOT in scope: salvage-skip noise / ref retention (LOW d) — leave it.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`
(`two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline; rerun alone before
calling a timeout a failure.)

## Rules
- Work only in your worktree; one commit `TASK-8DWJP.1.4: fix(ledger-sync): <one line>`.
- `git reset --hard` / `git stash drop` / `rebase --abort` appear ONLY inside the sync path
  against the ledger worktree the daemon owns. Never run them anywhere else.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).

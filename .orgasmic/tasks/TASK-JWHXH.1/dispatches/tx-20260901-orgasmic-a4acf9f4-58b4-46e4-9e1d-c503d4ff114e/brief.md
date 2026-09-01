# Review: TASK-JWHXH.1 — views/ on existing ledgers (H4) + coalesced view rebuild (M1)

Fix round for chain-review findings H4 and M1 (whole-chain review tx-1c6d2115). Implementer:
codex gpt-5.6-sol, one commit `49de897f`, merged to main as `c3d779af`.

## What to review

    git diff c3d779af^1 c3d779af

Two files, +149/-3: `crates/orgasmic-daemon/src/ledger_sync.rs` and
`crates/orgasmic-daemon/src/index.rs`.

## The findings this must close

- **H4.** Only the scaffold (`shipped/project-scaffold/.gitignore`) ignores `views/`; an
  existing ledger kept its old `.orgasmic/.gitignore` and the sync loop re-committed
  `views/{board,glossary,decisions}.org` every tick. The live ledger was hand-fixed; the code
  did nothing for any other ledger.
- **M1.** `orgasmic_core::build_views` ran only from `load_project` and from the
  `machines/*/claims.org` arm of `apply_written_path`; `reload_node_dir` never rebuilt, so a
  project without dispatch claim churn served stale views forever after boot.

## What the fix claims

1. `sync_once_inner` (after the `branch == orgasmic && origin exists` early return, i.e. only
   for ledgers the daemon syncs) ensures `.orgasmic/.gitignore` has a `views/` line
   (byte-preserving append, CRLF-tolerant match) and runs
   `git rm -r -q --cached --ignore-unmatch -- .orgasmic/views` before the existing
   `git add --all` — so the ignore rule and the untrack land in the same commit (the loop's
   `pull --rebase --autostash` drops index-only changes, which is how the first manual fix
   attempt failed). Runs every tick; claimed idempotent.
2. `Index::schedule_view_rebuild`: a `Mutex<HashSet<PathBuf>>` of dirty roots + an
   `AtomicBool` drain flag; the first mark spawns a task that sleeps 200 ms
   (`VIEW_REBUILD_DEBOUNCE`), takes the set, runs `build_views` per root in `spawn_blocking`,
   `warn!`s on failure, and loops while new roots arrived; the check-and-clear of the flag is
   done while holding the set's lock. Called at the tail of `reload_node_dir` on the
   changed path only.
3. Tests: `ledger_sync::tests::existing_ledger_views_are_ignored_untracked_and_idempotent`
   and `index::tests::incremental_node_write_rebuilds_views_without_claim_churn`.

## Attack these specifically

- **Coalescer liveness and races.** Walk `schedule_view_rebuild` against a mark that arrives
  (a) during `spawn_blocking`, (b) between `mem::take` and the final `is_empty` check,
  (c) exactly while the drain holds the lock and stores `false`. Can a root be marked and
  never built? Can two drain tasks run concurrently? What happens if `tokio::spawn` is
  called when the runtime is shutting down (daemon exit mid-burst) — is that a panic or a
  dropped rebuild?
- **Rebuild storms / self-trigger.** `build_views` writes `views/*.org`. Confirm from the
  code (not the brief) that both `apply_written_path` (`index.rs` `Some("tmp" | "views")`
  early return) and the watcher (`watcher.rs` `dropped_views`) drop those writes. Is there any
  OTHER consumer of fs events under `.orgasmic/views` that now fires per burst?
- **Cost.** `build_views` re-reads every node in tasks/glossary/decisions. A dispatch close
  writes N nodes within a few ms — count how many rebuilds the coalescer actually performs for
  that burst, and what a steady 2 s claim-churn tick costs now that BOTH the claims arm
  (synchronous) and the node-reload arm (debounced) rebuild.
- **H4 scope and side effects.** The untrack + ignore now runs on every tick for every synced
  ledger. `create_dir_all(.orgasmic)` runs unconditionally after the early return — does that
  create `.orgasmic/` (and a `.gitignore` commit) in a synced ledger that had none? Is the
  `views/` line match correct for `views`, `/views/`, `**/views/`, and a commented line? Does
  `git rm --cached` on a path that is gitignored AND tracked behave on git ≥ 2.40 as the test
  assumes?
- **Multi-machine.** Machine A untracks `views/` and pushes the deletion; machine B still
  has tracked, locally-modified `views/*.org` and pulls with `--rebase --autostash`. What
  happens on B — clean untrack, conflict, or a resurrected tracked file on the next tick?
  Reason it through the loop in `ledger_sync.rs`; say what you could not verify.
- **Test honesty.** Does the index test prove the rebuild came from the coalescer and not from
  `load_project` during `index.rebuild()`? (Check whether `views/board.org` could already
  contain the task via another path.) Does the ledger_sync test's "second sync creates no
  commit" actually run through the pull/push path, or short-circuit on `diff --cached --quiet`?

Already established — do not re-spend: on the merged tree the manager ran
`cargo test -p orgasmic-daemon --lib -- ledger_sync views` → 8 passed / 0 failed;
`cargo clippy -p orgasmic-daemon --all-targets -- -D warnings` clean; `cargo fmt --all --check`
clean (see the task's Evidence section: `orgasmic task get --project orgasmic TASK-JWHXH.1`).

## Rules

- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` (you may READ it to check the current
  `.orgasmic/.gitignore` and `git ls-files .orgasmic/views` state there).
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-JWHXH.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only (`cargo test -p orgasmic-daemon --lib <name>`); never the workspace;
  never `ORGASMIC_HOME`; do not read `verify/*/injection.patch`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

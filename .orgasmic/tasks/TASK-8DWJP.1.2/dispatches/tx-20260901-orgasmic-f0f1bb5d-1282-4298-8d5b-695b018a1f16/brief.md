# TASK-8DWJP.1.2 — rebase check before the idle gate; salvage tracked writes before reset; strict stage-3 match

Read the task first: `orgasmic task get --project orgasmic TASK-8DWJP.1.2` — each finding with
`file:line`, fix direction and acceptance. Three rounds already landed on this path
(`200892f2`, `a64d5cf8`, `59c351dc`); the review confirmed all of their items and rejected
on what follows. Line numbers are approximate; read the current
`crates/orgasmic-daemon/src/ledger_sync.rs`. Manager decision: dec_EWY0K binds — nothing
this machine wrote may be silently discarded; salvage, do not merely label.

## 1. HIGH — a mid-rebase ledger must never report `idle`
`sync_once_with_park` (~:98) gates on `symbolic-ref --short HEAD == orgasmic` BEFORE the
unmerged guard (~:104). Mid-rebase HEAD is detached → `Idle` forever after a crash between
the conflicting pull and `rebase --abort`. Fix: check `origin` first; then if
`rebase_in_progress` (exists, ~:288) and `rebase-merge/head-name` (or `rebase-apply/head-name`)
reads `refs/heads/orgasmic` → `git rebase --abort` and fall through to the normal tick; only
then apply the branch gate. Test: run a conflicting pull in a test repo and do NOT abort;
the next `sync_once` must not be `Idle` and must recover (worktree == remote, parked ref
holds the local bytes).

## 2. HIGH — salvage tracked writes before `reset --hard`
Only the `Worktree` branch of `park_conflict_inner` stages the worktree (~:428-437);
`Parked`/`Autostash`/`Unrecoverable` reset immediately (~:462), discarding every write to a
tracked file that landed after the conflicting pull — including `machines/<id>/tx/<month>.org`
appends. Fix, all local git inside the fence, right before the reset, using a scratch index so
the UU index cannot block it:

    GIT_INDEX_FILE=<tmp> git read-tree origin/orgasmic
    GIT_INDEX_FILE=<tmp> git add -A -- <same pathspecs + excludes as stage_ledger>
    GIT_INDEX_FILE=<tmp> git write-tree              → <tree>
    git commit-tree <tree> -p origin/orgasmic -m "ledger: conflict salvage <machine>"  → <sha>
    git update-ref refs/orgasmic/conflicts/<machine>/<ts>-salvage <sha>   (skip if <tree> == origin/orgasmic^{tree})

Name the salvage ref in the conflict status error string and as event extra `SALVAGE_REF`.
Test: after the conflicting pull, modify a tracked non-conflicted file AND append a line to
the machine tx file; run the recovery tick; assert both are in the salvage ref and the
status names it.

## 3. MEDIUM — absent stage 3 must not match a parked ref
`commit_matches_conflict_side` (~:320-329) compares `Option == Option`; delete/modify has no
`:3:`, so `None == None` lets a stale parked ref match. Fix: for parked-ref candidates require
at least one path with `Some` on both sides and treat an absent `:3:` as non-matchable; the
identity-verified autostash may still take the all-absent case. Test: delete/modify shape
with a stale parked ref present → the autostash is parked, not the stale ref.

## 4. LOWs
- Re-entry `Parked` path: if `stash@{0}` is our identity-verified autostash, drop it too;
  extend `foreign_stash_on_top_is_not_dropped` with a next tick asserting the autostash is
  gone and the foreign stash remains.
- Failed parked-ref push → "parked ref not yet on origin" in the conflict status error string.
- Tests: assert `parked_ref` non-empty in `leftover_foreign_machine_conflict_does_not_wedge…`;
  make the post-conflict write in `conflicting_two_writer_tick…` a TRACKED modified file.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
- `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`
(`two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline; rerun alone before
calling a timeout a failure.)

## Rules
- Work only in your worktree; one commit `TASK-8DWJP.1.2: fix(ledger-sync): <one line>`.
- `git reset --hard` / `git stash drop` / `rebase --abort` appear ONLY inside the conflict path
  against the ledger worktree the daemon owns. Never run them anywhere else.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).

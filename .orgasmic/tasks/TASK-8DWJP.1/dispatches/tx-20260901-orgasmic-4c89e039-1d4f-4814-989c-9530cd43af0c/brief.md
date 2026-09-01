# Review: TASK-8DWJP.1 — conflict-path fix round (after the 8DWJP REJECT) + the MSYN4.2.1 hunk

Implementer: codex gpt-5.6-sol, one commit `6692c2e6`, merged to main as `a64d5cf8`.
This round answers your predecessor's REJECT of `200892f2` (tx-eb858d4c: HIGH missing `tx/`,
HIGH autostash-pop conflict reported `synced`, MEDIUM prose-scraped paths, MEDIUM unfenced
salvage→reset window). Read that verdict first:
`orgasmic task get --project orgasmic TASK-8DWJP.1` (task body = the findings) and the
decision `orgasmic decision get --project orgasmic dec_EWY0K`.

## What to review — two diffs, one file

    git diff a64d5cf8^1 a64d5cf8      # this round: ledger_sync.rs (+259/-41), writer.rs (+95)
    git diff 9909a41e^1 9909a41e      # TASK-MSYN4.2.1, merged unreviewed on the promise that
                                      # you would read it here: ledger_sync.rs sidecar
                                      # untracking + status hygiene, doctor.rs warning

File findings for the second diff against `--task TASK-MSYN4.2.1`, for the first against
`--task TASK-8DWJP.1`.

## What this round claims
1. After every `pull --rebase --autostash`, regardless of exit code, `git diff --name-only
   --diff-filter=U -z` decides "conflict". Active rebase (rebase dir present) → read paths,
   `rebase --abort`, salvage commit, park HEAD, fetch, reset. No rebase in progress (retained
   autostash) → park the retained stash commit, drop it, fetch, reset.
2. `conflict_paths` prose scrape is gone; paths come from the unmerged index.
3. `WriterCommand::Barrier` + `WriterHandle::run_barrier`; `park_conflict` runs inside it; the
   `ledger.sync_conflict` append happens after the barrier returns.
4. Tests (`ledger_sync.rs:~922-1120`, `writer.rs:~3515`): literal
   `.orgasmic/machines/<id>/tx/<month>.org` route, modify/delete PATHS, retained-autostash
   bytes parked, remote reset, clean second tick, no markers on the remote, an append queued
   during the barrier lands after the reset. LOW 5 (conflict-ref count in status) skipped.

## Attack these specifically
- **Which stash gets parked and dropped?** The ledger is a git WORKTREE of the source
  checkout, and `refs/stash` is NOT per-worktree — it is shared with the operator's main
  checkout. Manager pre-check (verify, then size it): `ledger_sync.rs:177-188` — when
  `unmerged_paths` is non-empty and no rebase is in progress, the code takes
  `git rev-parse stash@{0}` blindly; the string `Created autostash` is parsed nowhere. So if
  that branch is ever entered for a reason other than a fresh autostash-pop conflict (an
  unmerged index left by a failed earlier tick, an operator merge in the ledger worktree, or
  a pull that made no autostash), the OPERATOR's newest stash from the main checkout gets
  parked and dropped. Say how reachable each entry path is and whether the parked ref makes
  it "recoverable but silently gone from `git stash list`" (MEDIUM) or worse. Fix direction to
  confirm: parse `Created autostash: <sha>` from the pull stdout and require
  `rev-parse stash@{0}` == that sha before any drop; otherwise `failed` + backoff, no drop.
- **Manager pre-check 2:** `stage_ledger` (`ledger_sync.rs:164`) runs BEFORE the pull with no
  unmerged-index guard. Trace what happens on the tick after `park()` fails midway (see next
  bullet). Also `:160-162` still promises a future "writer-published quiescence barrier" — the
  barrier now exists for `park_conflict`; say whether that comment describes a different
  (staging) window or is stale.
- **Failure ordering inside the conflict path.** Enumerate every early `?` between "conflict
  detected" and `reset --hard`: fetch failure after `stash drop`, `update-ref` failure, push of
  the parked ref (must be best-effort), `rebase --abort` failure. For each: what state is the
  worktree/index left in, and what does the NEXT tick do? Specifically: if the index is still
  unmerged (UU) when the next tick's `stage_ledger` runs `git add --all`, the markers get
  committed and pushed — the original HIGH 2 through a side door. Is there a guard (stage
  refuses / re-enters the conflict path while `--diff-filter=U` is non-empty)? If not, MEDIUM
  or HIGH depending on reachability.
- **Barrier semantics.** Is the writer actor a dedicated thread or a tokio task? If a tokio
  task, running seconds of blocking git inside `Barrier` stalls a runtime worker — does the
  arm use `spawn_blocking`/`block_in_place`, or is the actor already on its own thread?
  Can anything inside `park_conflict` call back into the writer (deadlock)? Does the barrier
  reply on panic/error inside `run` (a poisoned barrier = writer wedged forever)? What
  happens to API requests queued behind a 30-second conflict (timeouts, 503s)?
- **Cached tx handles after `reset --hard` inside the barrier.** The identity check
  (`tx_handles_detached_from_paths`) runs before each append — confirm it also covers the
  case where the file's inode is unchanged but its length shrank (reset to an older remote).
- **Autostash test vector honesty.** Does the test really produce the exit-0 stash-pop shape
  (a dirty tracked file under a foreign `machines/<other>/` dir plus a remote change to the
  same file), and does it assert on the BARE REMOTE after a second tick that no `<<<<<<<`
  appears? Does it prove the parked ref's tree holds the LOCAL bytes (not the remote's)? Is
  the test pinned to git behaviour that differs across versions (say which git you ran)?
- **Detection breadth.** `--diff-filter=U` after a rebase conflict, after a stash-pop
  conflict — and after a modify/delete (unmerged entry present? yes for the modified side —
  verify) and a rename/rename. Does `-z` parsing handle a path with a space?
- **MSYN4.2.1 hunk** (`9909a41e`): the per-tick `git rm -r -q --cached --ignore-unmatch --
  ':(glob).orgasmic/**/*.tmp'` (+ `*.tmp.*`, `*.bak.*`): cost on a large ledger every 2 s;
  can the glob untrack a LEGITIMATE tracked file (a node or artifact whose name ends in
  `.tmp` or contains `.bak.`)? Does untracking a sidecar committed by ANOTHER machine cause
  ping-pong commits between machines? `Idle` preserving `last_success_at`; status-map prune
  keyed by exactly the same `PathBuf` as the insert; `doctor.rs` warning shape matches
  `daemon status` and does not double-print with it.
- **Left-overs.** Is the `ponytail:` ceiling comment now true given the barrier (the window
  it described is gone — does it still claim one exists)? Any dead code from the old prose
  parser or from `conflict_paths`?

Already established — do not re-spend: the implementer ran the four gates (23 daemon tests,
22 cli, clippy, fmt) and the manager re-ran the same four on merged main `a64d5cf8` — see
`orgasmic task get --project orgasmic TASK-8DWJP.1` Evidence. Targeted re-runs are fine
(`cargo test -p orgasmic-daemon --lib -- ledger_sync barrier`); never the workspace. The
`two_daemon_loops_converge_through_the_bare_remote` test has a 10 s deadline and is
load-sensitive — a timeout there under parallel cargo is not a finding unless it fails serially.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` except read-only `git config/log/stash list/
  ls-files`. The live daemon on :4848 runs the PRE-fix runtime — not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop` anywhere outside a
  throwaway temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task <TASK-8DWJP.1|TASK-MSYN4.2.1>
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence for TASK-8DWJP.1:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

# Review: TASK-8DWJP.1.1 — re-entrant conflict recovery (third round of the dec_EWY0K conflict path)

Implementer: codex gpt-5.6-sol, one commit `bcb516c9`, merged to main as `59c351dc`.
This round answers the 8DWJP.1 REJECT (tx-4c89e039): HIGH park_conflict failure mid-way →
next tick pushes markers or wedges; MEDIUM positional `stash drop` on a shared `refs/stash`;
MEDIUM network git inside the writer barrier; LOW panic-in-barrier; LOW PATHS join. Read
that verdict (`orgasmic task get --project orgasmic TASK-8DWJP.1.1`, task body) and the
decision `orgasmic decision get --project orgasmic dec_EWY0K`.

## What to review

    git diff 59c351dc^1 59c351dc      # ledger_sync.rs (+441/-94 incl. tests), writer.rs (+13)

The two earlier rounds (`200892f2`, `a64d5cf8`) are already reviewed; only re-check them where
this diff touches the same lines.

## What this round claims
1. The UU index is checked BEFORE any staging (`ledger_sync.rs:~104-108`). Non-empty on entry
   → recovery: identify a matching retained autostash or an existing parked ref, reuse parked
   recovery state across interrupted ticks, otherwise reset unrecoverable conflicts to the
   fetched `origin/orgasmic` with an explicit status message (`~311-468`).
2. `stash@{0}` identity is verified immediately before the drop; mismatch → no drop, `failed`.
3. `git fetch` runs before `run_barrier`, the best-effort parked-ref push after it; the barrier
   closure contains only local git (`~374-403, ~694-711`).
4. `PATHS` tab-separated (`~613`). Barrier arm wraps `run()` in `catch_unwind` and always
   replies (`writer.rs:~2427-2430`), with a test that the writer accepts the next barrier.
5. Tests (`~1215-1373`): interruption after stash drop, leftover UU under another machine,
   foreign-stash identity mismatch/no-drop, no markers on the remote, parked bytes retained.
   Skipped by design: doctor recovery text; push-failure in the conflict status.

## Attack these specifically
- **"Matching" parked ref / retained autostash.** Manager pre-check: `conflict_source_on_entry`
  (`ledger_sync.rs:331-372`) is content-based — a candidate (parked refs sorted `-refname`,
  then `stash list -1` if its subject is `autostash`) is reused only when
  `commit_matches_conflict_side` (`:320-329`) finds `commit:path` == `:3:path` for EVERY
  conflicted path; otherwise `Unrecoverable`. Verify: (a) stage 3 is the LOCAL side in both
  shapes (rebase: the replayed local commit; stash pop: "Stashed changes") — if stage 2/3 are
  swapped in either shape, the match is against the remote and a stale ref could be reused
  wrongly; (b) a path that is add/add or modify/delete with no `:3:` entry — does
  `rev-parse :3:path` returning `None == None` make a FOREIGN commit "match"? (c) what
  `Unrecoverable` discards: tracked-modified local bytes not in any candidate are lost on
  `reset --hard`; is that stated in the status error and is it the right call under dec_EWY0K?
  (d) `created_autostash` (`:311-318`) parses `Created autostash: <short>` from stdout — confirm
  git prints it on stdout (not stderr) in the exit-0 pop-conflict case.
- **Fresh writes on the re-entry path.** When the tick enters via the top guard, local files
  may hold fresh daemon writes since the last tick that were never staged (a UU index blocks
  `commit`). What happens to them: salvage-committed some other way, parked, or discarded by
  the reset? If discarded, is that the "unrecoverable" branch and does the status message say
  so? Distinguish tracked-modified (lost on reset) from untracked-new (kept).
- **Every failure seam, not just one.** The injected-failure test covers "after stash drop,
  before reset". Walk the other seams — after `update-ref`, after the pre-barrier `fetch`
  fails, after `reset --hard` but before the event append, a crash inside the barrier — and
  state the next tick's behaviour for each. Is the parked-ref name stable across re-entries
  (same conflict → same ref) or minted anew each attempt (ref litter but safe)?
- **Stash identity.** Where does the expected sha come from (`Created autostash:` parse of the
  pull stdout, or a pre-pull `rev-parse`)? Is the verify-then-drop itself racy (still a
  positional `stash drop` after the compare — acceptable window, but say so)? On mismatch:
  is the autostash left on the stack and does the NEXT tick's top-guard handle "UU index +
  autostash not on top" without dropping the wrong entry?
- **Barrier contents.** Confirm by reading the closure that no `push`/`fetch`/`ls-remote` runs
  inside it. The `reset --hard origin/orgasmic` inside uses a ref fetched BEFORE the barrier
  wait — fine unless the code assumes it is the latest; say whether anything does.
- **catch_unwind.** `AssertUnwindSafe` correct; is the panic logged and does the caller of
  `run_barrier` get an error (not a silent `Ok`)? Does the writer's own state stay consistent
  after a panic mid-`park_conflict` (half-done git sequence → next tick's guard handles it)?
- **Test honesty.** For each new test, say whether the failure is injected through a real seam
  in the production flow or by hand-crafting the on-disk state. Both are acceptable; a test
  that asserts on the bare remote after the recovery tick is the one that matters — name it.
- **Nothing regressed from the earlier rounds:** literal `machines/<id>/tx/<month>.org` event
  route, modify/delete PATHS, autostash-pop detection, barrier ordering test — still asserted?

Already established — do not re-spend: implementer ran 4 gates (26 daemon tests, 22 cli,
clippy, fmt); the manager re-ran the same four on merged main `59c351dc` — see `orgasmic task
get --project orgasmic TASK-8DWJP.1.1` Evidence. Targeted re-runs are fine
(`cargo test -p orgasmic-daemon --lib -- ledger_sync barrier`); never the workspace.
`two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline — a timeout under
parallel cargo is not a finding unless it fails alone.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only `git config/log/stash list`. The
  live daemon on :4848 runs the PRE-fix runtime — not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop` outside a throwaway
  temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-8DWJP.1.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

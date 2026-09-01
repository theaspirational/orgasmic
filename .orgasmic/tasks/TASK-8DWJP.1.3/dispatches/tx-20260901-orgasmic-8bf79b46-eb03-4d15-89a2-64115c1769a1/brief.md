# Review: TASK-8DWJP.1.3 — salvage before every `rebase --abort` (round 5, narrow)

Implementer: codex gpt-5.6-sol, one commit `5846e9bb`, merged to main as `1ff48c3a`.
This round answers the single HIGH of the 8DWJP.1.2 review (tx-878e798d): the entry-path
`rebase --abort` hard-reset the worktree before any salvage. Read
`orgasmic task get --project orgasmic TASK-8DWJP.1.3` and `dec_EWY0K`.

    git diff 1ff48c3a^1 1ff48c3a      # ledger_sync.rs only, +92/-29

Rounds 1–4 are reviewed. Keep this review to the diff and its direct neighbours.

## What this round claims
- One helper `abort_rebase_with_salvage` (`~:349-372`) used at both abort sites (`~:103-116`
  entry path, `~:208-212` in-tick): reads `rebase-merge/orig-head` or `rebase-apply/orig-head`,
  falls back to `ORIG_HEAD`, runs `salvage_worktree` against that base, then aborts. An
  unchanged tree yields an empty salvage ref.
- `ConflictSource::Worktree` carries the pre-abort salvage ref into the existing conflict
  outcome / status / event (`~:61-65, ~:509-554`).
- Dead entry-path rebase branch removed; status wording now `raw worktree snapshot at <ref>
  (conflicted paths carry markers)` (`~:405-409, ~:705-708`).
- Tests: `conflicting_two_writer_tick` tracked write moved before the conflict tick; a real
  interrupted-rebase regression writes a tracked task node AND the machine tx file during the
  outage and proves the status-named salvage ref holds both (red before the change, green
  after — logs cited in the report).

## Attack these specifically
- **Base choice.** `orig-head` = the local pre-pull tip. Confirm `salvage_worktree` with that
  base seeds the scratch index from the base tree (so files untouched during the outage do
  not appear as changes) and that the "unchanged → empty ref" skip compares against the
  base tree, not `origin/orgasmic`. Is `ORIG_HEAD` fallback ever wrong (e.g. ORIG_HEAD set by
  an earlier `reset --hard` in the same worktree)? Bounded or not?
- **In-tick site.** At `~:208` the salvage now runs between the failed pull and the abort,
  BEFORE the barrier (that path is outside `park_conflict`). It is local git only — confirm.
  Does the salvage ref made here get merged with, or superseded by, the one `park_conflict`
  makes later in the same tick (two refs, one named)? Status must name what exists.
- **Entry-path site.** Manager pre-check (`:103-117`): `unmerged_paths` is read BEFORE the
  abort (correct), then `abort_rebase_with_salvage`, then — only if paths is non-empty —
  `recover_conflict(… ConflictSource::Worktree(salvage_ref) …)`, which carries the ref into
  status/event. If the interrupted rebase has an EMPTY unmerged set (killed between applying
  commits, or after an operator resolved-and-staged), the salvage ref is minted and the tick
  falls through to the normal pull with the ref never reported. Size it: how reachable is an
  empty unmerged set for a daemon-driven `pull --rebase` (it only stops on conflict), and is
  "created but unreported salvage ref" a LOW (bounded, data safe, discoverable via
  `for-each-ref refs/orgasmic/conflicts/`) or more? If LOW, name the one-line fix (log +
  status note when a salvage ref was made on a non-conflict tick) rather than a round.
- **Nothing else moved.** Diff-check that no behaviour outside the two abort sites, the
  wording, the dead branch and the tests changed.
- **Test honesty.** The mid-rebase regression: does it drive `sync_once` on a real
  interrupted `pull --rebase`, write BOTH files during the outage, and read them back from
  the ref the status names (not a ref the test derives)? Which assertion went red pre-fix?

Classify precisely: if only LOWs remain, say so plainly and APPROVE (with follow-ups if any);
this path has had five rounds and the operator will decide on residuals.

Already established — do not re-spend: implementer ran 4 gates (30 daemon, 22 cli, clippy,
fmt) plus the red/green probe; the manager re-ran the same four on merged main `1ff48c3a` —
see `orgasmic task get --project orgasmic TASK-8DWJP.1.3` Evidence. Targeted re-runs are fine;
never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs the PRE-fix runtime — not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop` outside a throwaway
  temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-8DWJP.1.3
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

# Review: TASK-8DWJP.1.4 — rebase abort under the writer barrier (round 6, narrow)

Implementer: opencode / zai-coding-plan/glm-5.3 (variant max), one commit `641ec6c8`, merged to
main as `62f986e0`. This round answers the MEDIUM + three LOWs of the 8DWJP.1.3 review
(tx-8bf79b46). Read `orgasmic task get --project orgasmic TASK-8DWJP.1.4` and `dec_EWY0K`.

    git diff 62f986e0^1 62f986e0

Rounds 1–5 are reviewed. Keep this review to the diff and its direct neighbours.

## What this round claims
- `abort_rebase_with_salvage` (salvage + `git rebase --abort`) now runs inside the writer
  barrier at BOTH call sites (entry path and in-tick), the same way `park_conflict` does.
- The empty-unmerged entry case carries its pre-abort salvage ref (`pending_salvage`) into
  the tick's status/event instead of dropping it.
- The salvage-failure trade at the abort site is decided and pinned with a `ponytail:` comment.
- The `SALVAGE_REF` event assertion is back in `conflicting_two_writer_tick`.

## Attack these specifically
- **Barrier scope.** Is the abort really inside `run_barrier`, or did the closure shape move
  and the abort still runs on the blocking thread? Trace `sync_once_with_park`'s closure(s)
  to `ledger_sync.rs` production loop (`barrier_writer.run_barrier`). Does a writer append
  issued during the barrier land AFTER the abort and survive (test present, does it fail
  when the barrier is removed)?
- **Barrier hazards.** Network git must stay OUT of the barrier (round 2 finding). Does the
  barriered region now contain a fetch/push/pull? Can the barrier deadlock (writer loop
  waiting on itself; `block_on` inside a tokio worker) — confirm `spawn_blocking` shape held.
- **pending_salvage.** Is it named in BOTH the status text and the `ledger.sync_conflict`
  event when the tick later parks? Warned + noted when the tick syncs cleanly? Or dropped?
- **Salvage-failure trade.** Which way did it fall (wedge loudly vs abort and lose)? Is the
  comment honest about it? Is the 8DWJP.1.2 unwedge guarantee kept or knowingly reversed?
- **Nothing else moved.** Diff-check that no behaviour outside the barrier hoist, the
  pending_salvage carry, the trade comment and the tests changed.
- **Test honesty.** Which assertion goes red when the barrier hoist is reverted?
- **The LOW (c) deviation.** The brief asked to restore the `SALVAGE_REF` event assertion in
  `conflicting_two_writer_tick`. The implementer says the premise is false: that test's pull
  stops mid-rebase (source `Worktree`, not `Autostash`) and the abort-time salvage tree equals
  orig-head's tree, so `salvage_ref` is empty; they relocated the two-line assertion to
  `mid_rebase_tick_aborts_and_recovers_instead_of_idling` and pinned the negative (no
  `SALVAGE_REF` extra) in `conflicting_two_writer_tick`. Verify the claim (read the fixture's
  git sequence); if the premise holds, the relocation is the right call — say so. If the
  premise is wrong, that is a finding.
- **Status surface.** The synced-with-pending-salvage note rides `LedgerSyncStatus.error` with
  outcome `"synced"`; the implementer notes CLI `status`/`doctor` render only conflict/failed
  outcomes, so it surfaces via the daemon log + API payload only. Size that: LOW or worse?

Classify precisely: if only LOWs remain, say so plainly and APPROVE (with follow-ups if any);
this path has had six rounds and the operator will decide on residuals.

Already established — do not re-spend: implementer ran 4 gates; the manager re-ran the same
four on merged main `62f986e0` — see `orgasmic task get --project orgasmic TASK-8DWJP.1.4`
Evidence. Targeted re-runs are fine; never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only git reads. The live daemon on
  :4848 runs the PRE-fix runtime — not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop` outside a throwaway
  temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-8DWJP.1.4
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

# Review: TASK-8DWJP.1.3 — salvage before every `rebase --abort` (round 5)

Scope: `git diff 1ff48c3a^1 1ff48c3a` (`crates/orgasmic-daemon/src/ledger_sync.rs`, +92/-29) and its
direct neighbours. Rounds 1–4 not re-reviewed.

## Verdict

**APPROVE WITH FOLLOW-UPS.** The HIGH is genuinely fixed. No HIGH remains. One MEDIUM and four
LOWs, all bounded and none blocking.

Acceptance, item by item:

| Criterion | Status |
| --- | --- |
| Mid-rebase outage writes survive in a salvage ref named by the status (test) | met — `mid_rebase_tick_aborts_and_recovers_instead_of_idling` (`:1509-1615`) |
| In-tick abort path salvages too | met — same helper `abort_rebase_with_salvage` at `:208` and `:105` |
| Status wording | met — `raw worktree snapshot at <ref> (conflicted paths carry markers)` (`:705-708`) |
| Dead `conflict_source_on_entry` rebase branch gone | met — deleted; `conflict_source_on_entry` still reachable for the autostash/parked entry cases |
| Existing ledger_sync/barrier tests green | not independently re-run in full (manager already did on merged main); I ran the 3 salvage tests, all pass |

The base choice is sound and, notably, robust to being wrong: `salvage_worktree` seeds a scratch
index from the base tree but then `stage_ledger` re-adds **every** `.orgasmic` path from the
worktree, so the base only governs paths outside `.orgasmic` and other machines' dirs plus the
commit parent. A stale `ORIG_HEAD` fallback therefore cannot lose a ledger write. The
`rebase-merge`/`rebase-apply` → `ORIG_HEAD` lookup mirrors the existing `rebase_in_progress` /
`rebase_head_name` path handling exactly, including the relative-vs-absolute `--git-path` join —
verified both forms occur in practice (`.git/rebase-merge` in a plain clone,
`/…/.git/worktrees/ledger/rebase-merge` in the real linked ledger worktree).

Only one salvage ref per tick: the `ConflictSource::Worktree(ref)` payload short-circuits
`park_conflict_inner`'s own `salvage_worktree(remote_head)` call (`:551-554`), so the status names
the ref that actually holds the outage writes. Confirmed by reading, and by the new test asserting
the status string against the outcome's own `salvage_ref`.

The entry-path `recover_conflict` short-circuit (`:106-115`) is new behaviour beyond "move the
salvage", and it is the right call: without it the tick would fall through to a second `pull
--rebase`, conflict again, and mint a *second* post-abort salvage ref that no longer holds the
outage writes — and the status would name that one. Everything else in the diff is confined to the
two abort sites, the wording, the dead branch, and tests.

## Findings

### MEDIUM — `ledger_sync.rs:370-372`: the abort still runs outside the writer barrier

`abort_rebase_with_salvage` does `salvage_worktree(...)` then `git rebase --abort`. Neither is under
the writer barrier — only `park_conflict` is wrapped (`:889-903`, `barrier_writer.run_barrier`).
Writers keep appending to `.orgasmic/machines/<id>/tx/<month>.org` per dec_EWY0K rule 1, so a tx
append that lands between the salvage `git add` and the abort is hard-reset away with no copy in
any ref — the exact failure the round set out to close, at a smaller scale.

Failure scenario: a writer's `rename()` onto the tracked tx file completes after `salvage_worktree`
finishes `write-tree` and before `git rebase --abort` returns; that entry exists nowhere afterwards.

Severity reasoning: pre-existing in shape (the abort was unbarriered in 1.2 as well), and this round
shrinks the window from the whole outage (minutes to hours) to two git invocations (tens of ms). It
is not "likely data loss", so not HIGH — but every other destructive reset in this file
(`reset --hard origin/orgasmic`, `:557`) *is* barriered, so the omission is a real inconsistency.

Fix direction: hoist the barrier so it wraps `abort_rebase_with_salvage` too, or move the abort into
the barriered `park_conflict` and have the entry/in-tick sites hand it the pre-abort state.

### LOW — `ledger_sync.rs:103-116`: empty unmerged set mints an unreported salvage ref, and the reported one is then wrong

Manager pre-check asked me to size this. Bigger than "unreported":

1. `paths` is empty → `abort_rebase_with_salvage` still mints `S1` (non-empty in practice, see the
   last LOW) and aborts, discarding the outage writes from the worktree.
2. The tick falls through to the normal path: commit, `pull --rebase`. That pull conflicts again
   (same two commits) → in-tick site mints `S2` **after** the outage writes were destroyed.
3. Status names `S2`. An operator following the status recovers a snapshot that does not contain
   what they lost. `S1` holds it, unnamed.

If the second pull instead succeeds, the outcome is `Synced` and nothing is said at all.

Reachability, measured rather than assumed — I built a throwaway repo, forced a rebase conflict,
then `git add`-ed the resolution without `--continue`:

```
unmerged now: []
rebase in progress: yes
```

and confirmed the premise of the whole round while I was there: a tracked write to a
*non-conflicted* file made in that state is gone after `git rebase --abort` (`d/g` reverted from
`outage` to `other`). So the empty-unmerged state is reachable via an operator who resolves and
walks away, and via a daemon/machine death between applied commits of a multi-commit rebase. Not
reachable from an uninterrupted daemon-driven `pull --rebase`, which only stops on conflict.

Data is safe — `refs/orgasmic/conflicts/<machine>/<ts>-salvage` is a real ref, gc-proof, and
discoverable via `git for-each-ref refs/orgasmic/conflicts/`. Hence LOW.

One-line fix (as the manager framed it): keep the minted ref in a `pending_salvage` local; if the
tick later parks, append it to the status/event alongside the tick's own ref; otherwise
`tracing::warn!` it and add a status note. Do not just log — the misleading-`S2` case is the part
that needs the status to carry both.

### LOW — `ledger_sync.rs:370-372`: a salvage failure now wedges the ledger instead of unwedging it

`abort_rebase_with_salvage` propagates any error from `rebase_orig_head` or `salvage_worktree`
*before* reaching `git rebase --abort`. 8DWJP.1.2 added the entry-path abort precisely so a
mid-rebase ledger stops idling forever; now a persistent salvage error (unwritable `TMPDIR` for the
scratch index, a `git add` failing on an unreadable path, a stuck `.lock`) leaves the rebase in
place and the tick fails, so the ledger backs off to `MAX_BACKOFF` and never syncs again.

Bounded and visible: `sync_ledger_at_with_park`'s `Err` arm sets `outcome: "failed"` with the full
anyhow chain and warns, so `daemon status` shows it. Arguably the right trade (keep the data, stay
loud) — but it is an undocumented reversal of the previous round's guarantee. Fix direction: either
a `ponytail:` comment naming the trade at `:370`, or degrade to `salvage_ref = String::new()` +
`tracing::warn!` and abort anyway. Pick deliberately; do not leave it implicit.

### LOW (test) — `ledger_sync.rs:1417`: SALVAGE_REF event assertion deleted, now uncovered

The round removed `conflicting_two_writer_tick`'s
`assert!(events[0].extra.iter().any(|(k, v)| k == "SALVAGE_REF" && v == salvage_ref))`. `grep -n
SALVAGE_REF` over the file now returns exactly one hit — the producer at `:806`. Both new/updated
salvage tests assert the *status string* only; nothing asserts the `ledger.sync_conflict` tx entry
carries the ref. For a round whose entire point is "the salvage ref must be discoverable", losing
the event-side assertion is the wrong coverage to drop. Restore the two lines in
`conflicting_two_writer_tick` (that test still produces a non-empty `salvage_ref` — its source is
`Autostash`, unchanged by this round).

The *other* deletion in that test is fine: the follow-up "next tick reaches synced and pushes to the
remote" assertion is still covered by `conflict_reenters_after_failure_between_stash_drop_and_reset`
(`:1497-1506`). On LOW 3 of the assignment — the implementer moved the tracked write before the
conflict tick rather than dropping it, which is the better of the two options offered, since it now
proves the write lands in `parked_ref`.

### LOW — `ledger_sync.rs:606-608`: the unchanged-tree skip effectively never fires on a rebase-sourced conflict

`salvage_worktree` returns an empty ref when `tree == remote_tree`, where the base is now
`orig-head`. During a `pull --rebase` conflict the worktree is `origin/orgasmic` + partially replayed
local commits + markers, which never equals the pre-pull local tip's tree. So a salvage ref is minted
and named in the status on essentially *every* rebase-sourced conflict, including when the machine
wrote nothing during the outage — and the snapshot's contents are then mostly remote bytes the
operator never authored. Under the previous base (`origin/orgasmic`, post-abort) an empty ref
genuinely meant "nothing to salvage".

Impact is noise and ref accumulation, not loss: `grep -rn "refs/orgasmic/conflicts"` shows no pruning
anywhere in the repo, so `<ts>-salvage` refs pile up per conflict forever. Pre-existing, amplified
here. Fix direction (only if the noise bites): compare the salvage tree against the tree the abort
will restore (`orig-head`'s tree *plus* the machine's own paths) rather than the plain base, or add a
retention sweep for `refs/orgasmic/conflicts/<machine>/`.

## Open Questions

- Is the MEDIUM barrier gap worth a round 6, or a `ponytail:` comment naming the residual window?
  Five rounds in, that is the operator's call; the data-loss window is now ~two git invocations.
- Which way should a salvage failure fall — wedge loudly (today) or abort and lose? The code makes
  the choice silently either way.

## Verification Notes

What I actually ran:

- `git diff 1ff48c3a^1 1ff48c3a` — full read, every hunk accounted for above.
- Read `ledger_sync.rs` `:93-232` (both abort sites), `:297-372` (`unmerged_paths`,
  `rebase_in_progress`, `rebase_head_name`, `rebase_orig_head`, `abort_rebase_with_salvage`),
  `:405-560` (`conflict_source_on_entry`, `recover_conflict`, `park_conflict_inner`), `:594-638`
  (`salvage_worktree`), `:658-830` (status text, `record_sync_conflict`), `:855-935` (the production
  loop and where the barrier actually wraps), and the four affected tests.
- `cargo test -p orgasmic-daemon --lib -- mid_rebase_tick conflict_recovery_salvages
  conflicting_two_writer` → **3 passed, 0 failed** (1.03s). Independent confirmation the merged tree
  is green on the tests this round touches.
- Throwaway temp repo (`/tmp/rev8dwjp.4rH00j`, created by me, no ledger contact): proved
  rebase-in-progress-with-empty-unmerged-set is reachable, that `git rev-parse --git-path
  rebase-merge` returns a **relative** path in a plain clone, and that `git rebase --abort` discards
  a tracked write to a non-conflicted file.
- Read-only `git rev-parse` against the live ledger to confirm `--git-path` returns an **absolute**
  path in a linked worktree (the real deployment shape). Both branches of the code's
  `is_absolute()` handling are therefore exercised in reality.
- `grep -n SALVAGE_REF` and `grep -rn "refs/orgasmic/conflicts"` for the coverage and pruning claims.

What I did **not** check:

- Did not re-run the four gates (30 daemon / 22 cli / clippy / fmt) — the manager re-ran them on
  merged main `1ff48c3a` and the brief marks that established. Residual risk: a test outside my
  3-test filter regressed. Low; the diff touches only `ledger_sync.rs`.
- Did not execute the red-before-fix probe. Reasoned instead: pre-fix, the entry path aborted with no
  salvage, so the follow-up in-tick salvage would snapshot a worktree in which `tracked_path` reads
  `tracked base`; `assert_eq!(show {salvage_ref}:{tracked_path}, "tracked during outage")` therefore
  cannot pass. The implementer's red/green claim is consistent with the code.
- Did not review rounds 1–4, `commit_matches_conflict_side`'s stage-3 logic, or the barrier
  implementation in `writer.rs` beyond confirming where `run_barrier` is applied.
- No production-path probe against the live daemon: it runs the pre-fix runtime (per the brief), so
  it cannot exercise this change.

## Fix Directions

Ranked, all optional for this round:

1. Wrap `abort_rebase_with_salvage` in the writer barrier (MEDIUM) — closes the last window in which
   a tracked ledger write can be destroyed by an abort.
2. Carry a `pending_salvage` ref out of the empty-unmerged entry case into the tick's status/event
   (LOW) — stops the status naming a snapshot that lacks the lost writes.
3. Restore the two-line `SALVAGE_REF` event assertion in `conflicting_two_writer_tick` (LOW).
4. Decide and record the salvage-failure trade at `:370` (LOW).
5. Only if the noise is felt: tighten the empty-salvage skip and/or add conflict-ref retention (LOW).

APPROVE WITH FOLLOW-UPS.

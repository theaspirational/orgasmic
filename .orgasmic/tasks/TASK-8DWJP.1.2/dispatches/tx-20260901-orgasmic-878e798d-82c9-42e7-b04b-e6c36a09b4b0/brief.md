# Review: TASK-8DWJP.1.2 — round 4 of the dec_EWY0K conflict path (rebase-first idle gate, scratch-index salvage, strict stage 3)

Implementer: codex gpt-5.6-sol, one commit `b273c465`, merged to main as `a4372f03`.
This round answers the 8DWJP.1.1 REJECT (tx-6a92d428): HIGH mid-rebase ledger reports idle
forever; HIGH tracked writes after the conflicting pull discarded by `reset --hard`; MEDIUM
`None == None` stage-3 match; LOW orphan autostash; LOW push warn only; LOW test gaps. Read
`orgasmic task get --project orgasmic TASK-8DWJP.1.2` (task body = the findings) and
`orgasmic decision get --project orgasmic dec_EWY0K`.

    git diff a4372f03^1 a4372f03      # ledger_sync.rs only, +492/-49 (about half tests)

Rounds 1–3 (`200892f2`, `a64d5cf8`, `59c351dc`) are reviewed; re-check them only where this
diff touches the same lines.

## What this round claims
1. `~:99-115` checks `origin` first, aborts an interrupted rebase whose head-name is
   `refs/heads/orgasmic` BEFORE the detached-HEAD idle gate, then continues into the normal
   tick (unmerged guard → conflict path).
2. `~:349-417` parked-ref matching requires a present stage 3 on at least one path; the
   identity-verified autostash keeps the all-absent (delete/modify) fallback; `*-salvage` refs
   are excluded from parked candidates.
3. `~:420-604` before `reset --hard`: snapshot the allowed ledger paths through a scratch
   index (`GIT_INDEX_FILE`, `read-tree origin/orgasmic`, `add -A` with the stage pathspecs,
   `write-tree`, `commit-tree`, `update-ref refs/orgasmic/conflicts/<machine>/<ts>-salvage`);
   drop an identity-matched orphan autostash on re-entry; record parked-ref push failures.
4. `~:645-783` status names the salvage ref and an unpushed parked ref; event carries
   `SALVAGE_REF`. Tests `~:1274-1815` (30 in the gate): mid-rebase recovery, tracked
   post-pull task/tx salvage, strict delete/modify match, orphan stash cleanup, push-status
   text, non-empty `parked_ref`, tracked post-conflict write.

## Attack these specifically
- **Idle-gate reorder safety.** Manager pre-check (verified by reading `:100-110` and
  `rebase_head_name` `:321-338`): origin check → abort ONLY when `rebase_in_progress` AND
  head-name (from `rebase-merge` then `rebase-apply`, via `rev-parse --git-path`, so
  worktree-correct) trims to exactly `refs/heads/orgasmic` → then the `symbolic-ref` gate.
  A missing head-name yields `None` → no abort; a foreign-branch worktree falls through to
  `Idle`. Only re-check: a head-name read error other than NotFound turns the tick into
  `Err` (+backoff) rather than `Idle` — acceptable? And the abort itself failing (`?` at
  `:106`) — next tick retries the same abort; can that loop (e.g. a rebase state git refuses
  to abort) and does status show it as `failed` rather than `idle`?
- **Salvage tree contents.** Which pathspecs/excludes feed the scratch-index `add -A` — both
  of `stage_ledger`'s adds (node dirs AND `machines/<self>`)? Are `views/`, sidecars and
  `.orgasmic/tmp` excluded the same way? Do CONFLICTED paths enter the salvage tree with
  marker text (`<<<<<<<`)? That is acceptable as a record only if the status text says the
  salvage is a raw worktree snapshot — check. Is `commit-tree -p` the pre-fence fetched
  `origin/orgasmic` (fine) and is the salvage skipped when the tree equals
  `origin/orgasmic^{tree}`? Is everything inside the fence still local-only?
- **Re-entry idempotence with salvage.** Crash between the salvage `update-ref` and
  `reset --hard`: next tick — does it create a second salvage ref (litter, acceptable) or skip
  parking the real local side because a `*-salvage` ref now exists? The implementer says
  salvage refs are excluded from parked candidates — verify the exclusion is by name suffix
  and cannot be fooled by a real conflict ref that happens to end in `-salvage`.
- **Scratch index hygiene.** `GIT_INDEX_FILE` must be set ONLY on the scratch commands, never
  leak into the following `reset --hard` or the writer's later git calls; temp file removed on
  success AND error; a stale temp file after a kill is harmless (say so or not).
- **Strict stage-3 rule.** Read `commit_matches_conflict_side` (or its replacement): "at least
  one `Some`/`Some` equal, no `Some`/`Some` unequal, absent stage 3 non-matchable for parked
  refs" — is that exactly what it implements? Does the autostash fallback still verify identity
  (`Created autostash:` sha) before being trusted for the all-absent case?
- **Orphan autostash drop on re-entry.** Is the drop still guarded by the identity check
  (never a foreign stash), and does the extended `foreign_stash_on_top_is_not_dropped` prove
  the foreign entry survives the NEXT tick?
- **Status and event honesty.** With a salvage present, does the conflict status say local
  bytes were salvaged and where; without one, does it say nothing was discarded (and is that
  true)? Is `SALVAGE_REF` only emitted when a salvage ref exists?
- **Test honesty.** For each new test say whether it hand-crafts state or drives a real seam,
  and which assertion would go red if the fix were reverted. The mid-rebase test must run a
  real conflicting pull and NOT abort before calling `sync_once`.
- **Regressions.** Literal `machines/<id>/tx/<month>.org` route, modify/delete PATHS, barrier
  ordering, `conflict_reenters_after_failure_between_stash_drop_and_reset` — still asserted?

This is round 4. Classify precisely: if only LOWs remain, say so plainly; if a MEDIUM is
pre-existing and bounded, label it "pre-existing, bounded" so the operator can decide to
accept it with a doctor note rather than a fifth round.

Already established — do not re-spend: implementer ran 4 gates (30 daemon tests, 22 cli,
clippy, fmt); the manager re-ran the same four on merged main `a4372f03` — see `orgasmic task
get --project orgasmic TASK-8DWJP.1.2` Evidence. Targeted re-runs are fine; never the
workspace. `two_daemon_loops_converge_through_the_bare_remote` has a 10 s deadline — a
timeout under parallel cargo is not a finding unless it fails alone.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` beyond read-only `git config/log/stash list`. The
  live daemon on :4848 runs the PRE-fix runtime — not a defect.
- Never run `git reset --hard`, `git rebase`, `git pull`, `git stash drop` outside a throwaway
  temp repo you created.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-8DWJP.1.2
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

# Review: TASK-8DWJP.1.2 — round 4 (rebase-first idle gate, scratch-index salvage, strict stage 3)

Commit `b273c465`, merged `a4372f03`. Diff is `ledger_sync.rs` only, +492/-49.

## Verdict

REJECT.

Five of the six assigned items land correctly and are well tested. The sixth
(HIGH 1) is fixed as specified but the fix itself opens a new instance of the
data-loss class HIGH 2 was opened to close: the abort added at `:106` destroys
uncommitted tracked ledger writes, and no salvage runs before it. That is a
dec_EWY0K rule-2 violation on the exact path this round added, so it blocks.

Everything else is LOW. If the operator would rather accept it, HIGH 1 is a
one-move fix (hoist the existing `salvage_worktree` above the abort) — this
does not need a broad fifth round, just that hoist plus a test.

## Findings

### HIGH — `crates/orgasmic-daemon/src/ledger_sync.rs:102-107` (bug, data loss, new regression)

```rust
if rebase_in_progress(ledger)?
    && rebase_head_name(ledger)?.as_deref() == Some("refs/heads/orgasmic")
{
    git(ledger, &["rebase", "--abort"])?;   // :106
}
```

`git rebase --abort` is a hard reset to `ORIG_HEAD`. It discards *every*
uncommitted modification to a tracked file, including files that have nothing
to do with the conflict. No salvage runs before it: `salvage_worktree` is
only called from `park_conflict_inner:518`, which is reached via the
`unmerged_paths` guard at `:112` — and after the abort at `:106` there are no
unmerged paths, so that tick never enters `recover_conflict` at all. It falls
through to the normal path, re-pulls, and salvages the *post-abort* worktree.

The window this abort covers is precisely the long one HIGH 1 described:
daemon killed mid-rebase by launchd restart, in-place binary swap SIGKILL, or
sleep. Per dec_EWY0K rule 1 writers keep appending to tracked ledger files
(`.orgasmic/tasks/*/node.org`, `machines/<id>/tx/<month>.org`) throughout that
outage. The first recovery tick throws all of it away.

Reproduced end to end with the daemon's own command sequence (temp repo, not
the live ledger):

```
mid-rebase, head-name=refs/heads/orgasmic
=== writers keep working during the outage ===
tx base
SESSION WORK WRITTEN WHILE MID-REBASE
=== next tick, ledger_sync.rs:106 ===
git rebase --abort
=== what survived ===
--- worktree T2 ---
tx base
--- any ref/object anywhere holding that line? ---
(empty)          # git log --all -S ... : no match
(empty)          # git stash list        : no entries
```

Zero surviving copies — same proof shape the 8DWJP.1.1 reviewer used for
HIGH 2, on the path this round added.

`mid_rebase_tick_aborts_and_recovers_instead_of_idling:1481` makes no write
during the interruption window, so the gate is green over the loss.

Two same-shape aborts, both **pre-existing (rounds 2-3), bounded**, listed so
the fix covers them once:
- `:200-202` — in-tick pull conflict. Window is only the `pull` duration
  (`stage_ledger`+`commit_staged` ran at `:185-186`), but a network pull is
  seconds and writes drain during it.
- `:376-377` — see LOW 2; now unreachable.

**Fix direction.** Hoist salvage above the abort — `salvage_worktree` already
does exactly the right thing and needs no new machinery, only a base commit
argument (`origin/orgasmic` may be stale at `:106`; `HEAD`'s tree or the
rebase `orig-head` is the honest base there). Do the same at `:201`. Name the
resulting ref in the status the same way the conflict path does. Test: the
repro above driven through `sync_once`, asserting the mid-rebase write is
readable from the salvage ref afterwards.

### LOW — `:663-671` (docs/honesty)

The salvage snapshot records the conflicted path with raw conflict markers.
Verified by replaying the scratch-index sequence:

```
<<<<<<< Updated upstream
remote-bytes
=======
local-uncommitted
>>>>>>> Stashed changes
```

That is fine as a record, but the brief required the status to say so. It
currently reads `; tracked worktree salvage at <ref>`. One word fixes it:
`raw worktree snapshot (conflicted paths carry markers)`.

### LOW — `:373-378` and `:486-494` (dead code)

With the abort moved ahead of the idle gate, the `rebase_in_progress` branch
in `conflict_source_on_entry` can no longer fire from the entry path: a
head-name of `refs/heads/orgasmic` was already aborted at `:106`, and any
other head-name means detached HEAD, which the `symbolic-ref` gate at `:108`
turns into `Idle`. `ConflictSource::Worktree` is now only produced by the
in-tick path at `:202`. Harmless, but it makes it hard to tell which abort
actually ran when reading a postmortem.

### LOW — `:1477` (test)

LOW 6 asked for a tracked post-conflict write in
`conflicting_two_writer_tick` so a regression to the discard goes red. The
write to `.orgasmic/tasks/T2/node.org` now lands *after* the conflict tick has
returned, then a second tick syncs it — a discard would never touch it. The
assertion that actually guards the discard is
`conflict_recovery_salvages_tracked_writes_made_after_pull:1557`, which is
strong. Consider this item covered in substance, not in the named test.

## What is correct

Checked by reading the code and, where noted, by probe:

- **Idle-gate reorder.** Origin check first; abort only when
  `rebase_in_progress` AND head-name is exactly `refs/heads/orgasmic`;
  `symbolic-ref` gate last. `rebase_head_name:321` resolves the state dir via
  `rev-parse --git-path`, so it is worktree-correct. I confirmed both git
  backends write `head-name` (`rebase-merge` and `rebase --apply`), so the
  "missing head-name re-wedges to Idle" hole the manager asked about is
  theoretical, not reachable — not filed.
- **Abort/read failures are `Err`, not `Idle`.** Both `?` at `:105`/`:106`
  propagate into the `Err` arm of `sync_ledger_at_with_park`, which sets
  outcome `failed` with backoff. A rebase git refuses to abort retries under
  backoff and shows as `failed` — the answer to the manager's question is
  yes, correct, and it cannot masquerade as idle.
- **Salvage pathspecs.** `salvage_worktree:661` calls the same `stage_ledger`
  the normal tick uses, so both adds run: `.orgasmic` minus `machines`, minus
  `*.tmp`/`*.tmp.*`/`*.bak.*`, plus `machines/<self>` with the same excludes.
  Other machines' dirs come from the `read-tree origin/orgasmic` seed and are
  untouched. `views/` is gitignored by the normal tick's own write, so it is
  excluded on any ledger that has ticked once.
- **Salvage base and skip.** `commit-tree -p` is the pre-fence fetched
  `origin/orgasmic`; salvage returns `""` when the tree equals
  `origin/orgasmic^{tree}`, so no empty refs and no `SALVAGE_REF` event key.
  Everything between `before_reset()` and `reset --hard` is local-only.
- **Scratch index hygiene.** `GIT_INDEX_FILE` is set only by
  `git_output_with_index`, and only `read-tree`/`add`/`write-tree` pass it —
  `commit-tree`, `update-ref` and the following `reset --hard` all go through
  plain `git_optional`/`git`. The temp index and its `.lock` are removed on
  both success and error (the closure result is bound before cleanup). A stale
  temp file after a kill is inert: unique UUID name, never read again.
- **Salvage-ref exclusion.** `:392-398` skips candidates whose last path
  segment contains `-salvage`. Parked refs are `%Y%m%dT%H%M%SZ` or
  `<ts>-<n>`, which can never contain that substring, so a real conflict ref
  cannot be swallowed. Re-entry after a crash between the salvage `update-ref`
  and `reset --hard` re-matches the real parked ref and mints a second salvage
  ref — litter, accepted per brief.
- **Strict stage 3.** `commit_matches_conflict_side:349` implements the
  assignment literally for parked candidates: any path with an absent `:3:`
  makes the candidate non-matchable, and at least one `Some`/`Some` equality
  is required. Autostash keeps `require_stage_three=false`, and is still
  identity-gated by the `Created autostash:` sha at `created_autostash:340`.
  Note the bounded cost: a *mixed* conflict (one modify/modify + one
  delete/modify) on re-entry now rejects a genuinely correct parked ref and
  reports "no recoverable local state" while that ref still sits on disk
  unnamed. No data loss (salvage covers the worktree, the old ref persists),
  misleading status only. Matches the assignment as written, so not filed —
  flagging it as **pre-existing-by-spec, bounded**.
- **Orphan autostash drop.** `drop_matching_autostash:544` scans the whole
  stash list and drops only an entry whose sha equals the parked commit AND
  whose message is `autostash`. Because the parked ref was minted from the
  autostash sha in the earlier tick, this is the right entry, and a foreign
  stash can never match. The extended
  `foreign_stash_on_top_is_not_dropped:1751` drives a real NEXT tick and
  asserts `stash list` is exactly `On operator: foreign`.
- **Status/event.** `SALVAGE_REF` emitted only when non-empty; the request-id
  falls back to the salvage ref when there is no parked ref, so an
  unrecoverable-but-salvaged conflict gets a stable dedupe key.
  `parked ref not yet on origin` covers LOW 5.
- **Test honesty.** `mid_rebase_tick_...` runs a real conflicting
  `pull --rebase` and does NOT abort before `sync_once` — goes red (let-else
  panic) if the reorder is reverted. `conflict_recovery_salvages_...` drives
  the real `sync_ledger_at` seam with real post-pull writes to both a tracked
  task node and the literal `machines/<id>/tx/2026-09.org` route; reverting
  salvage makes `assert!(!salvage_ref.is_empty())` red.
  `delete_modify_conflict_...` hand-crafts the stale parked ref via
  `update-ref` (fair) but drives a real delete/modify conflict; reverting the
  strict rule makes `assert_ne!(parked_ref, stale_ref)` red.
  `parked_ref_push_failure_...` uses the `park` hook to break the origin URL
  — synthetic seam, but the status assertion is the real one.
- **Regressions.** `conflict_reenters_after_failure_between_stash_drop_and_reset`
  still asserts (salvage sits after `before_reset()`, so the injection point is
  unchanged); barrier ordering, sidecar exclusion and the `views/` idempotence
  tests untouched and green.

## Open questions

1. What base should the `:106` salvage use? `origin/orgasmic` has not been
   fetched yet at that point. `HEAD^{tree}` (the mid-rebase detached head) or
   the rebase `orig-head` are both defensible; the manager should pick, since
   it changes what a reconciling operator diffs against.
2. Should the `:201` in-tick abort get the same salvage in this round, or be
   deferred? It is the same bug with a much shorter window.

## Verification notes

- Ran: `cargo test -p orgasmic-daemon --lib -- ledger_sync status sync_conflict barrier`
  → **30 passed, 0 failed** (8.40s), log `/tmp/8dwjp12-daemon-tests.log`.
- Ran three throwaway temp-repo probes (`/tmp/rbabort.*`, `/tmp/ledgerloss.*`,
  `/tmp/salvmark.*`, `/tmp/rbapply.*`), all created by me, none touching the
  live ledger. They established, in order: `rebase --abort` discards
  uncommitted tracked writes; the full daemon sequence leaves zero surviving
  copies; the salvage tree carries conflict markers; both git rebase backends
  write `head-name`.
- Read only `ledger_sync.rs` (current tree + full `a4372f03^1..a4372f03`
  diff), plus `dec_EWY0K`.
- **Not checked** (already established per the brief, not re-spent): clippy
  `-D warnings` on daemon+cli, `cargo fmt`, and
  `cargo test -p orgasmic-cli --bin orgasmic -- daemon_lifecycle`.
- **Not checked**: behaviour against the live daemon on :4848 (runs the
  pre-fix runtime by design), and whether an operator-facing `orgasmic daemon
  status` renders the lengthened conflict error string readably — the string
  can now carry parked ref + salvage ref + push note in one line.

## Fix directions

1. **HIGH.** Move salvage above the abort at `:106` (and `:201`), pass it an
   explicit base commit, and name the ref in the status. Add a test: real
   mid-rebase interruption, write a tracked non-conflicted file, run
   `sync_once`, assert those bytes are readable from a salvage ref.
2. **LOW.** Say "raw worktree snapshot" in the salvage status text.
3. **LOW.** Delete `:373-378` and, if `ConflictSource::Worktree` keeps only
   the `:202` producer, fold the arm accordingly.
4. **LOW.** Optional: move the tracked write in `conflicting_two_writer_tick`
   to before the conflict tick, or drop the item as covered by
   `conflict_recovery_salvages_tracked_writes_made_after_pull`.

REJECT.

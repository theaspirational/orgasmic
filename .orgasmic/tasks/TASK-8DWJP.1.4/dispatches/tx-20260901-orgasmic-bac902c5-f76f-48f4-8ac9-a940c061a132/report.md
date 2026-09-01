# Review: TASK-8DWJP.1.4 — rebase abort under the writer barrier (round 6)

Commit `641ec6c8`, merged `62f986e0`. One file, 691 diff lines
(`crates/orgasmic-daemon/src/ledger_sync.rs`). The MEDIUM and all three LOWs of the
8DWJP.1.3 review are answered. Only LOWs remain.

## Verdict

**APPROVE WITH FOLLOW-UPS.** No HIGH, no MEDIUM. Four LOWs, all residual-shaped;
the operator can carry them or drop them.

## Findings

### LOW (correctness) `ledger_sync.rs:123` — entry-abort pending salvage is still dropped on the tick's error paths
`pending_salvage` is set at :123 and consumed only on the two success returns
(`Conflict` merge at :221-229, `Synced` at :241). Every `Err` return between those
points drops it: the pull-failure `bail!` at :235, the push-exhaustion `bail!` at :247,
and any `?` in `stage_ledger` / `commit_staged` / `ls-remote`.
`sync_ledger_at_with_park`'s `Err` arm (:811-830) records `outcome: "failed"` with the git
error only — the minted ref is named in no status, no event and no log line. It is not
re-minted later either: the rebase is already aborted, so the next tick's entry guard at
:107 is false. Concrete: daemon crashes mid-rebase, restarts while origin is unreachable
→ abort mints a ref holding the outage writes → `git pull --rebase` fails → :235 bails →
the only copy of those writes is an unnamed ref. Not data loss (the ref exists), but the
same "minted and never reported" shape this round set out to close, on the paths that
were not covered.

### LOW (usability) `ledger_sync.rs:730` — the synced+pending note is invisible to the CLI and self-erases
The note is attached to `LedgerSyncStatus.error` under `outcome: "synced"`.
`orgasmic status` (`crates/orgasmic-cli/src/main.rs:2791-2812`) and `orgasmic doctor`
(`crates/orgasmic-cli/src/doctor.rs:301-310`) both `match` on the outcome and render only
`conflict` / `failed` / `backed_off`, so a `synced` row with an `error` string prints
nothing. It is also a one-tick window: the next tick recomputes status with
`pending_salvage: None`, so the payload note disappears. What actually survives is the
`tracing::warn!` at :804 and, for one tick, the `/status` API JSON. The implementer
disclosed this; sizing it: LOW, since the ref itself persists and the acceptance
criterion ("named in status/event **or** warned + noted") is met literally.

### LOW (test) `ledger_sync.rs:986` — the production barrier wiring is unpinned
`ledger_sync::spawn` is called from exactly one place, `crates/orgasmic-daemon/src/lib.rs:1105`,
and no test calls it. The new test constructs its own `run_barrier` closure and calls
`sync_ledger_at_with_park` directly. So reverting the production hunk at :982-989 to a
bare `abort_rebase_with_salvage` compiles and leaves all 21 `ledger_sync` tests green.
The test proves the *semantics* (see Verification), not the daemon's wiring. This is the
same pre-existing gap the park barrier has had since round 2/3, so it is not a regression
introduced here — but it means the answer to "which assertion goes red when the hoist is
reverted?" is *none*.

### LOW (correctness, narrow) `ledger_sync.rs:884` — `request_id` ignores `pending_salvage`
In the unrecoverable branch (`parked_ref` empty **and** `salvage_ref` empty) the dedup key
is `remote_head + paths`. A later tick with the same remote head and paths but a *different*
`PENDING_SALVAGE_REF` dedups against the first event and its ref is never recorded. Requires
park to have produced no ref at all, so narrow.

## What checked out

- **Barrier scope — correct at both sites.** `sync_once_with_park` now takes `abort` and
  calls it at :112 (entry) and :219 (in-tick). Production `spawn` (:982-989) passes a
  closure that is `abort_runtime.block_on(abort_writer.run_barrier(...))`, structurally
  identical to the park closure at :973-981. Both call sites share the one closure, so both
  are barriered. The two non-barriered defaults, `sync_once_inner` (:80) and `sync_ledger_at`
  (:676), are both `#[cfg(test)]`.
- **No network git inside the barrier.** The barriered body is `abort_rebase_with_salvage`
  → `rebase_orig_head` (`rev-parse`), `salvage_worktree` (:629-673: `read-tree`,
  `stage_ledger`, `write-tree`, `commit-tree`, `update-ref`, all local), then
  `git rebase --abort`. No fetch/push/pull. The round-2 finding holds.
- **No deadlock.** Same `spawn_blocking` + `Handle::block_on` shape as park; the abort and
  park barriers are sequential within a tick, never nested. Confirmed by reading
  `writer.rs:856-876` — `run_barrier` enqueues and awaits a oneshot, and the barriered body
  needs nothing from the writer.
- **`pending_salvage` is carried on the two paths that matter.** Merged into the in-tick
  `Conflict` at :221-229 → status at :760-764 and `PENDING_SALVAGE_REF` at :878-882; carried
  onto `Synced` at :241 → status at :725-733 + warn at :798-806. The entry path with a
  non-empty unmerged set names the ref as the outcome's own `salvage_ref`, unchanged.
- **Salvage-failure trade: degrade, and the comment is honest.** :387-404 warns, hands back
  an empty ref, and still aborts. The `ponytail:` comment names the wedge it is avoiding by
  ticket (8DWJP.1.2) and states plainly that the outage writes then live in no ref. The
  1.2 unwedge guarantee is kept, not reversed. An empty `salvage_ref` is already a legal
  value (`salvage_worktree` returns it when tree == remote tree), so downstream handles it.
- **LOW (c) deviation is correct — the implementer's premise holds.** In
  `conflicting_two_writer_tick_parks_recovers_and_records_event` the tick commits both local
  writes, then `pull --rebase` hits a delete/modify conflict with the rebase in progress →
  `ConflictSource::Worktree(abort(...))`. `salvage_worktree` reads ORIG_HEAD (which already
  contains both writes) and the stopped rebase's worktree adds nothing, so the trees match
  and `salvage_ref` is empty. Proof, not just reasoning: `record_sync_conflict` pushes
  `SALVAGE_REF` only when non-empty (:873-877), the new negative assertion at :1507 passes,
  and the same positive assertion form passes in `mid_rebase_tick_...` at :1727-1730. A
  verbatim restore in `conflicting_two_writer_tick` would have been a permanently red test.
  Relocation is the right call.
- **Nothing else moved.** One file; every production hunk is the barrier hoist, the
  `pending_salvage` carry, or the trade comment. `SyncOutcome` variants gained one field
  each; the only ripple is `push_retries, ..` at :1249 and the four test call sites gaining
  an `abort` argument.

## Verification notes

Everything below was run read-only from the review worktree.

- `cargo test -p orgasmic-daemon --lib -- ledger_sync` → **21 passed, 0 failed** (6.78s),
  including all three new/changed tests. This is a targeted re-run, not the manager's gates.
- Read `writer.rs:856-876` (`run_barrier`), `ledger_sync.rs:629-673` (`salvage_worktree`),
  `:256-292` (`stage_ledger` — machine dir staged in a second `add`, which is why the test's
  parked-ref tx assertion is sound), `:840-914` (`record_sync_conflict`),
  `cli/main.rs:2785-2812`, `cli/doctor.rs:290-312`.
- `grep -rn "ledger_sync::spawn" crates/` → one hit, `daemon/src/lib.rs:1105`. Basis for the
  unpinned-wiring finding.
- The new test does prove the barrier semantics: it gates the first abort inside
  `run_barrier`, waits for `writer.status().queue_depth == 1` (so the append is provably
  queued *behind* the barrier — this times out and fails if the barrier is not held), then
  asserts the pending-salvage ref carries the pre-append outage writes **and** that the
  append survives in both the parked ref and the bare remote. That is the acceptance
  criterion, on the real `park_conflict`/`abort_rebase_with_salvage` code.

### Not checked

- I did not re-run the manager's four gates (clippy, fmt, `orgasmic-cli daemon_lifecycle`,
  the `status`/`sync_conflict`/`barrier` filters). Taken from the task Evidence.
- I did not empirically revert the production hunk to observe the green run — read-only.
  The claim rests on the grep plus the test's use of its own closure.
- No live-daemon probe. The daemon on :4848 runs the pre-fix runtime, as the brief states.
- Salvage/conflict-ref retention and GC behaviour: explicitly out of scope for this round.

## Open questions

1. Finding 1: is a one-line `tracing::warn!` for an orphaned `pending_salvage` in the `Err`
   arm worth a round 7, or does the ref-still-exists-in-git backstop make it a note?
2. Finding 2: should the synced+pending note get its own outcome (e.g. `"synced_pending"`)
   so the CLI renders it, or is the daemon log the intended surface?

## Fix directions

- **Finding 1** — wrap the tail of `sync_once_with_park` so `pending_salvage` is logged on
  the error path, or return it in the error context. Smallest version: capture it in a guard
  and `tracing::warn!` in `sync_ledger_at_with_park`'s `Err` arm at :811.
- **Finding 2** — either add a `"synced"`-with-error arm to `main.rs:2791` and
  `doctor.rs:301`, or persist the note across ticks by keeping `pending_salvage` in
  `LedgerSyncStatus` until an operator clears the ref. The first is one match arm in each
  file; the second is a real state machine and probably not worth it.
- **Finding 3** — if it is worth closing at all, close it for park and abort together by
  extracting the two closures into one named `fn barrier_closures(writer, runtime)` that a
  test can call, rather than duplicating the wiring in a test.
- **Finding 4** — append the pending ref to the unrecoverable `request_id` at :886-889.

APPROVE WITH FOLLOW-UPS.

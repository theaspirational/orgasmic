# TASK-QGWK7.1.1.1.1 verify artifact

Pins B-1: a `dispatch-close` during a `git revert` stands down from persisting
the record, exactly as it does during a rebase, am, merge, cherry-pick or
bisect.

TASK-QGWK7.1.1.1 shipped the sequencer guard with five markers and no
`REVERT_HEAD`. `git revert` is the same sequencer machinery as `cherry-pick`, so
the record commit landed on the branch mid-revert while the close reported `ok`.
The exposure is not only a conflicted revert: a clean `git revert -n` leaves
`REVERT_HEAD` present too, so any staged revert was in the same state.

**Corrected by TASK-QGWK7.1.1.1.1.1 C-1 — what that costs.** This artifact was
authored against a measured symptom that does not hold for this code: that the
operator's `git revert --abort` afterwards exits 128 with `Untracked working
tree file … would be overwritten by merge`. Re-measured in the production
shape — the real-index `git add` in `commit_promoted_dispatch_record` first
(`manager.rs:5730`, the one guarded by the refusal above it), then the throwaway
index, `commit-tree` and `update-ref` — the abort exits **0**, clears
`REVERT_HEAD`, and the record survives in `HEAD` and on disk. The exit-128 wedge
reproduces only when that real-index `git add` is skipped, which production
never does. `merge --abort`, `cherry-pick --abort` and `am --abort` end the same
way — exit 0, record intact — but not by one mechanism (TASK-QGWK7.1.1.1.1.1.1
D-3): the single-pick `revert`/`cherry-pick` abort and `merge --abort` are a
`reset --merge` to the CURRENT HEAD, so they rewind and the record survives only
because it already *is* that HEAD; only `am` declines outright (`You seem to have
moved HEAD … Not rewinding`). The rewinding aborts discard the manager's own
conflict resolution; the `am` abort, having declined to rewind, leaves it staged
(measured). `rebase --abort` is the one guarded operation whose abort
really destroys the record, and a rebase detaches HEAD, so the detached-HEAD
refusal catches it first.

The refusal is still right, for the reason the corrected convention now gives:
the record must not land in the middle of an operation the manager has not
finished, on a branch whose shape they are still deciding. It enters history
once, cleanly, at a point they chose, for a promote plus a re-run.

`expect-red` still carries the superseded claim in its comment block. That is
deliberate: the file is treated as immutable so the pinned signature stays
byte-stable, and its `exit:`/`contains:` directives are correct either way. Read
this file, not that comment, for why the guard exists. It also names the test
`…_and_leaves_the_abort_working`, which is why the test keeps that name even
though the abort assertion is a regression fence rather than the discriminator.

The injection removes the `REVERT_HEAD` row, reproducing the blindness
TASK-QGWK7.1.1.1 shipped. Six rows remain. The red run's first failing
assertion is the refusal itself — the close reports `ok` where it must report
`partial`. The `git revert --abort` assertion lives in the same test, further
down, and is never reached under the injection: a close that stands down never
gets that far.

**The injection moved file, and why the red is unchanged
(TASK-QGWK7.1.1.1.1.1.1 D-2).** The marker array now lives in
`crates/orgasmic-cli/src/sequencer_markers.rs` rather than inline in
`manager.rs`, so that `tests/shipped_conventions.rs` — an integration test
against a bin-only crate — can compare it against the shipped convention in both
directions. The injection deletes the `REVERT_HEAD` row there instead. It
leaves the guard blind to `REVERT_HEAD` exactly as the old patch did, so it misses the same state
and the test fails on the same assertion. `cmd` and `expect-red` are unchanged.

TASK-QGWK7.1.1.1.1 also added a `("sequencer", …)` entry to the guard.
TASK-QGWK7.1.1.1.1.1 C-2 removed it, arguing that a stopped range never reaches
the guard without a `*_HEAD` marker of its own.
**That argument is false and the entry is back** (TASK-QGWK7.1.1.1.1.1.1 D-1,
measured on git 2.52.0): resolve a stopped pick's conflict and `git commit` it by
hand and git clears `REVERT_HEAD` while leaving the todo list, from which
`git revert --continue` still resumes. The latch C-2 measured is real —
`.git/sequencer` survives the `git reset --hard` that abandons a range — but the
latch was the MESSAGE, not the entry, so the refusal now names `git revert
--quit` for that state. The injection does not touch the `sequencer` row: a
single-pick revert writes no `.git/sequencer` at all (measured), so it plays no
part in this artifact's red.

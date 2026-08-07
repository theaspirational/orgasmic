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
shape — the real-index `git add` at `manager.rs:5712` first, then the throwaway
index, `commit-tree` and `update-ref` — the abort exits **0**, clears
`REVERT_HEAD`, and the record survives in `HEAD` and on disk. The exit-128 wedge
reproduces only when that real-index `git add` is skipped, which production
never does. `merge --abort`, `cherry-pick --abort` and `am --abort` behave the
same way: git declines to rewind a HEAD that moved. `rebase --abort` is the one
guarded operation whose abort really destroys the record, and a rebase detaches
HEAD, so the detached-HEAD refusal catches it first.

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

The injection restores the five-marker set. The red run's first failing
assertion is the refusal itself — the close reports `ok` where it must report
`partial`. The `git revert --abort` assertion lives in the same test, further
down, and is never reached under the injection: a close that stands down never
gets that far.

TASK-QGWK7.1.1.1.1 also added a `("sequencer", …)` entry to the guard; C-2
removed it, because a stopped range always carries its own `*_HEAD` marker
(so the entry added no coverage) while `.git/sequencer` survives the
`git reset --hard` that abandons such a range (so the entry latched, refusing
every close indefinitely). The injection no longer mentions it.

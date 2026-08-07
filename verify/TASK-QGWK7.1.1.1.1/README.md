# TASK-QGWK7.1.1.1.1 verify artifact

Pins B-1: a `dispatch-close` during a `git revert` stands down from persisting
the record, exactly as it does during a rebase, am, merge, cherry-pick or
bisect.

TASK-QGWK7.1.1.1 shipped the sequencer guard with five markers and no
`REVERT_HEAD`. `git revert` is the same sequencer machinery as `cherry-pick`,
so the record commit landed on the branch mid-revert — and the reviewer measured
what that costs: `git revert --abort` afterwards exits **128** with `error:
Untracked working tree file '.orgasmic/dispatch-records/<tx>/last.txt' would be
overwritten by merge` / `fatal: Could not reset index file to revision
<record-commit>`, leaving the revert still in progress and the index holding
`D .../last.txt` + `UU`. A guarded operation, by contrast, aborts cleanly and
keeps the record on disk for the re-run. The exposure is not only a conflicted
revert: a clean `git revert -n` leaves `REVERT_HEAD` present too, so any staged
revert was in the same state.

The injection restores the five-marker set (and drops the `sequencer` todo-list
entry that covers a multi-commit range stopped between picks). The red run's
first failing assertion is the refusal itself — the close reports `ok` where it
must report `partial` — pinned in `expect-red`. The `git revert --abort` claim
lives in the same test, one assertion later, and is unreachable under the
injection by construction: a close that stands down never gets the chance to
wedge the abort.

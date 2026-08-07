# TASK-QGWK7.1.1 verify artifact

Pins M-0: after `dispatch-close` promotes a report, the `git merge --no-ff`
that the review gate's own refusal message tells the manager to run next must
still run.

TASK-QGWK7.1 made the close `git add` the promoted record and stop there. A
staged path — **any** staged path, not only one the merge touches — makes
`git merge` refuse with "your local changes to the following files would be
overwritten by merge", so a manager who followed the gate's instruction
literally (close the reviewer, then merge) hit a hard failure. The fix commits
the record instead, on its own, through a throwaway index seeded from `HEAD`.

The injection restores the stage-and-stop behaviour. The red run's first
failing assertion is the merge inside `run_git`, pinned below.

Measured pre-fix on the unfixed tree (`eb772a6`): the same test fails **alone**
in 3.13 s, exit 101, with the same stderr.

**Re-authored under TASK-QGWK7.1.1.1 (2026-08-07), context only.**
`commit_promoted_dispatch_record` grew a sequencer-state refusal, a detached-HEAD
refusal and a reported rollback, so the patch's context lines no longer matched.
It still restores the same stage-and-stop behaviour. `cmd` and `expect-red` are
byte-unchanged, because what M-0 claims is unchanged.

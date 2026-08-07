# TASK-QGWK7.1.1.1 verify artifact

Pins F-1: a `dispatch-close` whose record persist fails must leave a state a
manager can repair with the command they already run — the close itself.

TASK-QGWK7.1.1 made the close *commit* the promoted record, which is correct on
the happy path. Its error path was not: promotion unlinks the tmp artifacts as
soon as the copies succeed, so a failed commit left the record on disk,
untracked, with tmp gone. `promote_dispatch_artifacts_in_place` then bails on
`last_path missing`, re-running the close hits `CloseTarget::AlreadyClosed` and
returns a no-op by design, and there is no `--repersist` verb. The one path
where the durability guarantee fails was the one path with no repair, and its
only signal was a `warning:` line.

The fix routes the already-closed no-op into `commit_promoted_dispatch_record`,
which needs only the promoted directory — only its *caller* was ever gated on
tmp. The injection removes that route, restoring the unrecoverable state.

The test drives the real binary through `cmd_dispatch_close` twice, with a
`.git/index.lock` held across the first close: the M-1 trigger, one step later.
The red run's first failing assertion is the recovery claim, pinned in
`expect-red`; every assertion before it passes under the injection, which is
what makes the red the right red rather than merely a red.

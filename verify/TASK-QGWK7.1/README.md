# TASK-QGWK7.1 verify artifact

Pins F-1: after `dispatch-close` promotes a report, the destination directory
is in git — durability no longer depends on which `git add` form a manager
happens to use.

The injection no-ops the persistence step. The red run's first failing
assertion is the pinned index message.

**Re-authored under TASK-QGWK7.1.1 (2026-08-07).** The injected function moved,
not the property: F-1 originally shipped as `stage_promoted_dispatch_record`
(`git add` only), which met "in the index" but made the manager's next
`git merge` refuse (M-0). Persistence is now `commit_promoted_dispatch_record`,
and this patch no-ops that instead. The pinned assertion and `expect-red` are
unchanged, because what F-1 claims is unchanged.

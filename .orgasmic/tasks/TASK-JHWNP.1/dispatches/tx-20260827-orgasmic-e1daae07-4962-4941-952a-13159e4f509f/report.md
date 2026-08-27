# TASK-JHWNP.1 re-review — chain-hold release paths (cd43bf91..3503b86b)

## Verdict

**APPROVE** — 0 HIGH, 1 MEDIUM, 2 LOW.

All eight round-1 findings are addressed: six fixed, one (MEDIUM-4) answered
with a stated policy, one (LOW-7) documented rather than fixed. Both named
regression tests are red against cd43bf91 by the exact guard each one names,
and the abandonment path I was asked to re-trace now works end to end: default
abort salvages and removes again, and `abort --no-worktree-remove` -> cancel ->
`worktree-prune` reclaims.

The fresh sweep of the NEW code found one real defect the first round could not
have seen: `chain_hold_has_pending_round` requires **every** task to still be
implementer-dispatchable, while `release_chain_worktree_holds` releases on
**any** intersection. That asymmetry lets a multi-task hold be born already
expired, so the very next prune deletes the checkout the close just promised to
hold. Not ship-blocking (no data loss — prune salvages — and single-task chains,
the common case, are unaffected), but it should land before the feature is
leaned on for bundles.

Gates run by me, both GREEN, quoted in Verification Notes.

---

## Per-finding verdicts

| # | Verdict | Evidence |
|---|---------|----------|
| HIGH-1 | **FIXED** | `manager.rs:1281` + `manager.rs:7228-7249` + `manager.rs:5822-5832`; tests `aborted_chain_cancelled_task_prune_reclaims_worktree`, `dispatch_close_uses_fix_subtask_property_and_abort_backlog` |
| HIGH-2 | **FIXED** | `manager.rs:7317` (intersection), `manager.rs:6006-6010` (set equality); test `multi_task_chain_closed_one_task_at_a_time_releases_hold` |
| MEDIUM-3 | **RESOLVED** (arm kept, and it is genuinely load-bearing) — but its stated justification is wrong; see LOW-B |
| MEDIUM-4 | **ADEQUATELY PUSHED BACK** | policy comment `manager.rs:1006-1008`, operator-facing text `manager.rs:1009-1012`; untested — accepted |
| MEDIUM-5 | **FIXED** | `manager-dispatch.org:298-313`, `:500-503`; `references/dispatch.md:29-40` |
| LOW-6 | **FIXED** | `manager.rs:7463-7474`; asserted in `implementer_round_refuses_a_dirty_chain_worktree` |
| LOW-7 | **STATED** (not fixed, as the finding allowed) | `manager-dispatch.org:306-308`, `references/dispatch.md:37-40` |
| LOW-8 | **FIXED** | `manager.rs:7190-7201`; `-z` record framing confirmed by probe on git 2.52.0 |

---

## Findings

### MEDIUM-A (bug) `crates/orgasmic-cli/src/manager.rs:7228` — a chain hold whose tasks are not *all* BACKLOG/TODO is treated as expired, so a multi-task hold can be born already expired and be deleted by the next prune

`chain_hold_has_pending_round` ANDs across the lock reason's tasks
(`pending &= dispatchable_stage(...)`, manager.rs:7242), and
`dispatchable_stage(Implementer, _)` is true only for `Backlog | Todo`
(manager.rs:9424-9426). `release_chain_worktree_holds` ORs
(`record.tasks.iter().any(...)`, manager.rs:7317). Take is all-or-nothing;
release is any. Two reachable failure scenarios:

**1. Mixed multi-task close.** Dispatch `--task A --task B` implementer. Close
`--task A --status done --merge-sha ... --no-worktree-remove` (a partial close;
`missing_close_tasks`/`partial_closed_annotation` explicitly model this).
A -> `in_review`. Then close `--task B --status aborted --no-worktree-remove`
to continue the chain: `keep_chain_worktree` is true, and `hold_chain_worktree`
locks with `&open.tasks` — the full list `A B` (manager.rs:1348-1352). The
record is now fully closed, so nothing else claims the tree. On the next
`manager worktree-prune`, `chain_hold_has_pending_round` reads A =`in_review`
-> `pending = false` -> `expired_chain_hold` -> `release_chain_hold`
(manager.rs:3961-3964, 4032) -> the tree is unlocked and **reclaimed**. The
close printed no warning; the operator was told the checkout was kept.

**2. Partial lifecycle failure on a reused chain tree.**
`apply_task_lifecycle_transitions` posts per task in a loop and returns on the
first error (manager.rs:9181-9188), so a two-task dispatch can leave A at
`in_progress` and B unmoved. The failure handler at manager.rs:961-984 calls
`retain_reused_worktree_after_failed_dispatch` — which re-locks and reports
"reused worktree retained and re-locked after dispatch failure"
(manager.rs:7477-7490) — and then bails **without** calling
`restore_task_lifecycle_stages` (unlike the post-send path, manager.rs:1039).
A sits at `in_progress`, so the freshly restored hold is expired on arrival and
the next prune deletes the checkout the error message just said was retained.

No data is lost either way — `reclaim_managed_worktree` salvages onto a ref —
but the chain silently ends and the printed state is a lie.

**Fix direction.** Make the two sides symmetric: keep the hold while **any**
task in the reason can still enter another implementer round
(`pending |= dispatchable_stage(...)`, seeded `false`), mirroring the `any`
already used at manager.rs:7317. Independently, add
`restore_task_lifecycle_stages(&client, &plan.project_id, &pre_dispatch_stages)`
to the lifecycle-failure arm at manager.rs:965 so a partial apply does not
survive the bail. Test: dispatch `A B`, close A `done` partial, close B
`aborted --no-worktree-remove`, then `worktree-prune` must SKIP.

### LOW-B (docs) `crates/orgasmic-cli/src/manager.rs:9349-9352` — the comment defending the `manager.dispatch_started` arm cites evidence that does not hold; the real justification is a different, stronger one

The arm is correct and load-bearing — I verified that independently, so this is
not a request to revert. But the comment says "The reviewer-chain integration
test and the unit case below both require this". The integration test does not.
In `torn_close_candidates`, every close arm runs
`pending.retain(|(_, pending)| pending.task != task)` *before* pushing
(manager.rs:9335), so a later close for the same task already displaces the
earlier pending entry. In
`reviewer_for_a_warm_implementer_chain_still_gets_a_fresh_worktree` the round-2
`done` close displaces the round-1 abort entry with or without the arm; the test
passes either way. The unit case added at manager.rs:11543-11559 is circular —
it asserts the new behavior rather than forcing it.

The justification that *does* hold, and that the comment should carry:
`post_task_dispatch_close_commit` deliberately writes **no** second ledger
append ("the derived transition must not become a second ledger append",
`crates/orgasmic-daemon/src/api.rs:18076-18078`), so a successful abort close
leaves its `LIFECYCLE_FROM/TO` entry pending forever. `manager dispatch-status`
(manager.rs:2918) and `dispatch-close` (manager.rs:1083) both run
`reconcile_torn_closes_best_effort`. Run either during a live round 2 and,
without the arm, round 1's intent `in_progress -> todo` is replayed; the daemon
guard `current_state != from_state && current_state != to_state`
(api.rs:18136-18142) sees `current_state == from_state == in_progress` and
**accepts it**, reverting a live round-2 task to TODO. That is the regression
the arm prevents, and it is exactly what the chain feature made reachable.

Residual (unchanged from round 1, still open, no fix asked): the daemon's own
`POST /projects/:p/tasks/:t/dispatch` appends `manager.dispatch_started`
(api.rs:7355-7366) and performs no lifecycle write, so a non-CLI client of that
endpoint would produce this evidence without the write it stands in for. I
grepped: the CLI (`daemon_client.rs:175`) is the only in-repo caller today, so
this is latent, not live.

### LOW-C (portability) `crates/orgasmic-cli/src/manager.rs:7190` — `git worktree list --porcelain -z` raises the effective minimum git version, and no floor is documented

`-z` for `git worktree list --porcelain` is not present in very old git. On such
a host `git_capture` returns `Err`, and `git_worktree_registrations` is now on
the critical path of `worktree-prune` (classification), `dispatch-close
--no-worktree-remove` (`hold_chain_worktree`) and reuse (`prepare_worktree`,
`create_worktree`) — so three verbs fail rather than degrade. I grepped
`AGENTS.md`, `docs/`, and `shipped/` and found no declared git floor to check
this against. Local git is 2.52.0, so nothing is broken here; the ask is one
line of documented requirement, or a fallback to the non-`-z` form on error.

---

## Open Questions

1. MEDIUM-A path 1: is a mixed `done`-then-`aborted` multi-task close a shape
   the manager convention actually sanctions? If it is not, the fix is still
   the symmetric `any`, but the priority drops.
2. MEDIUM-4's policy is now stated but not exercised. The implementer named "no
   transport-timeout fault injection" as a residual. I agree it is acceptable
   *as a residual*, because the change is a string append on an already-tested
   branch — but the branch itself (ambiguous failure destroying a reused chain
   tree) still has no test in any suite.

---

## Verification Notes

**Gate 1 — clippy.** `cargo clippy --workspace --all-targets -- -D warnings`
-> `Finished dev profile ... in 35.84s`, `CLIPPY_EXIT=0`, zero warning lines.
Log `/tmp/jhwnp1/clippy.log`.

**Gate 2 — dispatch suite.**
`ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch`:

```
VERDICT
======================================================================
  suite    : cargo test -p orgasmic-cli --test dispatch --no-fail-fast -- --skip legacy_drivers_and_explicit_pairs_emit_equivalent_start_events
  registry : verify/flake-registry.toml (7 entries, every owner open)
  billed   : legacy_drivers_and_explicit_pairs_emit_equivalent_start_events — NOT RUN (--skip applied to every invocation)
  failures : 0
  crashed  : none — every failing target reported a failure list
  cargo    : exited 0 (0 all green, 101 libtest reported failures)
  ignored  : 0 test(s) carrying #[ignore]
  cfg-off  : passed — the pause rendezvous hooks are compiled out without debug_assertions
  environ  : complete — no tool requirement was waived
  host     : calm (threshold syspolicyd_rate>=1.50; load corroborating only)
             rate    0.0507

verdict: GREEN — no failures.
======================================================================
```

`verify/flake-registry.toml` untouched. No flakes observed. Log
`/tmp/jhwnp1/dispatch.log`.

**HIGH-1 red-against-cd43bf91 — CONFIRMED by inspection with the named guard.**
The fix adds `&& args.no_worktree_remove` at manager.rs:1281. Against cd43bf91
that conjunct is absent, so:
- `dispatch_close_uses_fix_subtask_property_and_abort_backlog`: the close at
  dispatch.rs:1780-1799 passes no `--no-worktree-remove`, and its dispatch is
  `--kind implementer` (dispatch.rs:1684-1694) with an existing worktree dir
  (asserted dispatch.rs:1744). Old code -> `keep_chain_worktree = true` ->
  `remove_worktree = false` -> `cleanup_dispatch` skips the salvage arm
  entirely, so the new asserts `abort_stdout.contains("cleanup: worktree
  salvaged sha=")` and `!abort_worktree.exists()` (dispatch.rs:1803-1808) both
  fail. This is the "abort path writes salvage refs again" half of HIGH-1, and
  it is the assertion that proves it.
- `aborted_chain_cancelled_task_prune_reclaims_worktree`: cd43bf91 has no
  `chain_hold_has_pending_round`, no `expired_chain_hold`, no
  `ManagedWorktree::release_chain_hold` and no unlock in `worktree_prune`; the
  lock arm classifies any chain-prefixed lock `Held` unconditionally, so after
  `task update --state cancelled` the second prune still prints `SKIP` and the
  assert on `RECLAIMED PATH=` (dispatch.rs:4157-4161) fails.

**HIGH-1 abandonment path — re-traced end to end.** abort
`--no-worktree-remove` -> `hold_chain_worktree` locks with reason
`orgasmic: ... for TASK-DISPATCH`; close transitions the task to `Todo`
(`close_lifecycle_transitions`, manager.rs:8994-8999); `dispatchable_stage
(Implementer, Todo) = true` -> prune SKIPs. Then `task update --state
cancelled` -> stage `Cancelled` -> `pending = false` -> `expired_chain_hold`
-> `release_chain_hold` -> `unlock_chain_worktree` under the daemon
reservation and after both `assert_path_names` checks (manager.rs:5822-5832)
-> RECLAIMED. Dry-run is unaffected: `worktree_prune` returns at
manager.rs:5731 before the unlock, so `--dry-run` still mutates nothing.
The same release fires for the `tx record`-recorded final close the round-1
review named, because that also moves the task out of BACKLOG/TODO.

**HIGH-2 red-against-cd43bf91 — CONFIRMED by inspection with the named guard.**
`release_chain_worktree_holds` was `record.tasks == tasks` (ordered whole-list
equality) and is now `record.tasks.iter().any(|task| tasks.contains(task))`
(manager.rs:7317). In `multi_task_chain_closed_one_task_at_a_time_releases_hold`
the hold's record carries `[TASK-BUNDLE-A, TASK-BUNDLE-B]` and the releasing
close passes only `--task TASK-BUNDLE-B`, so the old equality is `false`, the
unlock loop matches nothing, and the assert
`!registration.lines().any(|line| line.starts_with("locked"))`
(dispatch.rs:4305-4308) fails. The reuse half is covered by
`implementer_round_reuses_the_chain_worktree_and_warm_cache`, now dispatching
round 2 as `--task TASK-BUNDLE-B --task TASK-BUNDLE-A` — reordered, which the
old ordered equality at manager.rs:6009 would have missed, silently creating a
cold checkout.

**LOW-8 — verified by probe, not inference.** Throwaway repo, git 2.52.0,
`git worktree list --porcelain -z | od -c`: fields are NUL-terminated and
records are separated by a second NUL (`...refs/heads/main\0\0worktree ...`),
so `split("\0\0")` frames records exactly and the trailing empty element is
dropped by `filter_map`. A reasoned lock emits
`locked orgasmic: next implementer round for TASK-A TASK-B\0`; a reasonless
lock emits bare `locked\0`. Both parse. `git_capture` ends with `.trim()`
(manager.rs:8577), which does **not** strip NUL (`\0` is not
`char::is_whitespace`), so the record framing survives the capture. The
round-1 blank-line and `worktree `-prefix forgeries are now unrepresentable.

**MEDIUM-5 — read, not assumed.** `manager-dispatch.org:298-313` states the
explicit-flag rule, the lock, task-order independence, the `--fresh-worktree
--worktree <new-path>` pairing, the Ctrl-C window with its recovery ("re-run
the dispatch before `worktree-prune`"), partial-close release, and the prune
reclaim after abandonment; `:500-503` adds the prune-side rule.
`references/dispatch.md:29-40` carries the same three points. Both replace
the stale text the round-1 review quoted.

**RMA18 / reviewer freshness — unchanged by this commit.**
`git diff --numstat cd43bf91..3503b86b` is `95/14` on manager.rs. No refusal
was deleted or reordered: the only edits in the classification path are the
lock-reason hoist at manager.rs:3957-3963 (pure refactor of the same
`registrations.iter().find(...)` lookup) and the `.filter(|_| !expired_chain_hold)`
on the existing lock arm. The new `release_chain_hold` field is gated on
`disposition_is_reclaimable` (manager.rs:4032), so it can never upgrade a
`Held` from the cwd, open-dispatch or live-run arms — those still win, in that
order, above the lock arm. The unlock in `worktree_prune` sits **after** the
cleanup file lock, the ledger re-read, the daemon reservation, and both
`assert_path_names` calls. Reuse remains gated on
`args.kind == Implementer && record.kind == "implementer"`
(manager.rs:6003-6008), untouched by this commit;
`reviewer_for_a_warm_implementer_chain_still_gets_a_fresh_worktree` still
passes.

**Residual risks the implementer named — my classification.**
- *No transport-timeout fault injection (MEDIUM-4):* **accepted.** The change
  is additive string state on a branch that already existed; the fencing order
  (`ambiguous` before `plan.reuse_worktree`) is unchanged and now explained.
  See Open Question 2.
- *Documented Ctrl-C window (LOW-7):* **accepted.** The finding itself offered
  "state or fix"; both docs now state it *and* name the recovery, which is
  more than the finding required.

**What I did not verify.** I did not mutate the shipped source — review is
read-only — so both HIGH red-claims rest on inspection naming the exact guard
each test depends on, plus the gate run. I did not run the full workspace
suite; the brief scoped me to clippy and the dispatch suite. MEDIUM-A path 2
(partial `apply_task_lifecycle_transitions` failure) is derived from the code,
not reproduced — it needs a daemon that accepts one task's transition and then
fails.

---

## Fix Directions

1. **MEDIUM-A** — seed `pending = false` and OR across the lock reason's tasks
   in `chain_hold_has_pending_round` (manager.rs:7234-7243), matching the `any`
   at manager.rs:7317. Add `restore_task_lifecycle_stages` to the
   lifecycle-failure arm at manager.rs:965. Test: dispatch `A B`, partial-close
   A `done`, close B `aborted --no-worktree-remove`, assert `worktree-prune`
   prints `SKIP` and the tree survives.
2. **LOW-B** — rewrite the comment at manager.rs:9349-9352 to name the real
   evidence: the close commit writes no second append (api.rs:18076), so the
   pending entry outlives a healthy close, and `dispatch-status` mid-round-2
   would otherwise replay `in_progress -> todo` onto a live task. Consider
   promoting the unit case to that scenario so the test forces the arm instead
   of restating it.
3. **LOW-C** — document a minimum git version, or fall back to the non-`-z`
   parse when `git worktree list --porcelain -z` errors.

# TASK-JHWNP review — worktree-per-task-chain (cd43bf91)

## Verdict

**REJECT** — 2 HIGH, 3 MEDIUM, 3 LOW.

The core mechanism is sound: reuse is fail-closed, reviewer freshness is
structurally intact, and no RMA18 refusal was removed, weakened, or reordered.
What blocks ship is the *release* half of the hold. `--status aborted` is
overloaded to mean "next round is coming", but the documented meaning of an
aborted implementer close is *abandonment*. Every abandonment now leaves a
`git worktree lock`ed directory that `worktree-prune` refuses forever and that
no orgasmic command can release.

Gates run (both GREEN, quoted in Verification Notes): clippy `-D warnings`,
`run-tests.sh -p orgasmic-cli --test dispatch` (96 passed, 0 failed).

---

## Findings

### HIGH-1 (bug) `crates/orgasmic-cli/src/manager.rs:1272` — every aborted implementer close keeps and locks its worktree; there is no way to remove it and no way for `worktree-prune` to reclaim it

`keep_chain_worktree` is true for *any* `--status aborted` implementer close
whose worktree directory exists (manager.rs:1272-1274). It then forces
`remove_worktree` false (manager.rs:1275) — `--worktree-remove` is silently
ignored, and there is no flag that turns the keep off. manager.rs:1336-1345
then takes a native `git worktree lock`. The **only** release is
manager.rs:1473-1475, on a `--status done` implementer close of the same task
list.

**Failure scenario.** An implementer worker dies. The manager convention
(`shipped/prompt-studio/conventions/manager-dispatch.org:171-173` and `:217`)
says to close it `--status aborted` and describes abort as "worktree cleanup".
The task is then cancelled or re-scoped, so no `implementer.done` close is ever
recorded. Result:

- the worktree stays on disk, locked;
- `manager worktree-prune` classifies it `Held` (manager.rs:3966-3982) and
  skips it on every future run;
- `WorktreePruneArgs` (manager.rs:455-462) has only `--dry-run` and `--task` —
  no force, no unlock, no release verb;
- recovery requires a hand-run `git worktree unlock`, which appears in no
  orgasmic doc, no CLI help, and no refusal message.

The implementer's own test proves the first half: dispatch.rs:4059-4090 aborts
a round, runs prune, and asserts `SKIP` + `worktree.is_dir()`. Reclamation is
only ever demonstrated *after* adding a `done` close (dispatch.rs:4104-4141) —
the case that never arrives for an abandoned chain.

Same dead end when the chain's final close is recorded with `orgasmic tx
record` instead of `dispatch-close --status done`, which
`manager-dispatch.org:287` explicitly sanctions for manager-rescued merges.

**Second-order effect.** Salvage of uncommitted worker output to
`refs/orgasmic/salvage/<sha>` lives inside the `if remove_worktree` arm
(manager.rs:7871-7889). An aborted implementer close no longer salvages
anything. The tree is left on disk so nothing is destroyed *now* — but the
durable salvage ref that TASK-2BPWM/TASK-D0GA3 exist to write is no longer
produced on the abort path, and nothing states that.

**Fix direction.** Do not derive "chain continues" from `aborted`. Gate the
keep on an explicit signal — a `--keep-chain-worktree` flag, or treat an
operator-supplied `--no-worktree-remove` as the chain signal (the assignment's
"without the operator remembering `--no-worktree-remove`" is satisfied by a
*named* flag just as well). Independently, give prune a release path: a
chain-prefixed lock with no closed-and-unmerged implementer round behind it
should be reclaimable, or add `manager worktree-prune --release-chain-holds`.

---

### HIGH-2 (bug) `crates/orgasmic-cli/src/manager.rs:1474` — a partial or reordered multi-task close never releases the chain hold

`release_chain_worktree_holds` is called with `&tasks` — the `--task` values of
*this* close, order-preserving and not necessarily the full set
(`normalize_tasks`, manager.rs:8760-8777). It matches `record.tasks == tasks`
(manager.rs:7264), an ordered, whole-list equality against the recorded task
list. The hold itself is taken with `&open.tasks`, the full list
(manager.rs:1338).

**Failure scenario.** Dispatch `--task A --task B` as implementer. Abort →
tree locked, reason names `A B`. Later close the chain `done`, either one task
at a time — a state `missing_close_tasks` (manager.rs:1348-1352) and
`partial_closed_annotation` (manager.rs:10265-10281) explicitly model and
`dispatch-status --partial-closed` reports — or with `--task B --task A`. The
equality fails, the unlock loop matches nothing, and the tree is a permanently
locked orphan with the HIGH-1 dead end.

The same ordered equality gates reuse (manager.rs:5977): round 2 issued as
`--task B --task A` silently creates a fresh tree instead of reusing. That
direction is safe, but it is silent — the operator gets a cold checkout with no
explanation.

**Fix direction.** Compare task lists as sets on both sides, and release any
chain hold whose recorded task list *intersects* the closing tasks.

---

### MEDIUM-3 (correctness / scope) `crates/orgasmic-cli/src/manager.rs:9287` — `torn_close_candidates` now discards a pending torn close on `manager.dispatch_started`, untested and unrelated to this task

The arm was `"task.state_transitioned"`; it is now
`"task.state_transitioned" | "manager.dispatch_started"`. Torn-close
reconciliation (task_EP3H1) exists to finish a close whose lifecycle leg never
landed; dropping a pending entry means that repair is abandoned, not performed.

The justifying comment says "dispatch writes its lifecycle transition before
the daemon can append manager.dispatch_started". That holds for `cmd_dispatch`
(manager.rs:960 before manager.rs:989) but is a property of *one* caller. The
daemon appends `manager.dispatch_started` itself
(`crates/orgasmic-daemon/src/api.rs:7358`) and performs no lifecycle write of
its own, so any other client of `POST /projects/:p/tasks/:t/dispatch` produces
exactly the evidence this arm now trusts, without the write it is standing in
for.

No test in this commit covers the arm, and the commit message does not mention
it. **Open question for the implementer:** which test forced this? If the
answer is "none", it should be reverted; if a test did force it, that test
belongs in the diff.

---

### MEDIUM-4 (correctness) `crates/orgasmic-cli/src/manager.rs:1007` — the ambiguous daemon-failure rollback destroys the reused chain worktree, and the reuse arm is unreachable on that path

In `cmd_dispatch`'s dispatch-failure handler the `ambiguous` branch is taken
first (manager.rs:1000-1019); `plan.reuse_worktree` is only consulted in the
`else if` at manager.rs:1020. `dispatch_failure_needs_daemon_cleanup` returns
true for **any** post-send error that is not a `daemon returned` rejection
(daemon_client.rs:202-210), i.e. every timeout.

**Failure scenario.** Round 2 reuses the chain tree. The dispatch POST times
out (the TASK-WTJ5V / TASK-NW4WV scenario the branch exists for). The CLI asks
the daemon to clean up, and the daemon removes the worktree and deletes the
branch — a tree it did not create, holding round 1's warm target directory and
every ignored cache the feature exists to preserve. The chain silently ends;
the operator sees only the transport error.

Fencing correctly wins over warmth here, so this may be deliberate — but it is
neither stated in a comment nor covered by a test, and it means the feature can
evaporate on a hiccup while the printed error talks only about the daemon.

---

### MEDIUM-5 (docs) manager-facing conventions still describe the old behavior

`shipped/prompt-studio/conventions/manager-dispatch.org:217` still tells the
manager that abort means "worktree cleanup", and
`shipped/skills/orgasmic/references/dispatch.md:29` still says only "a second
round cannot reuse the derived branch name". Neither mentions chain reuse, the
between-round lock, or `--fresh-worktree`. A manager following its own
conventions will abort a dispatch expecting the tree to be cleaned up, get a
locked tree instead, and have no documented way back.

The task's write scope was `crates/**`, so this is plausibly out of scope by
instruction — but then it is an unclosed follow-up, not a non-issue, and the
HIGH-1 leak is undiscoverable without it.

---

### LOW-6 (usability) `crates/orgasmic-cli/src/manager.rs:5981` — `--fresh-worktree` alone cannot work while the chain tree exists, and the error does not say so

`--fresh-worktree` disables reuse; `worktree_path` then falls back to
`default_worktree`, which is the same path the chain tree occupies, so
`create_worktree` bails `worktree path already exists: <path>`
(manager.rs:7409). The flag's own help says to pair it with `--worktree
<new-path>`, but the refusal the operator actually hits names neither flag.

---

### LOW-7 (correctness) `crates/orgasmic-cli/src/manager.rs:7339` — the window between unlock and dispatch registration silently ends a chain

`prepare_worktree` unlocks the chain hold before `git checkout -b`
(manager.rs:7339-7341). If the CLI is interrupted between that unlock and the
daemon writing `manager.dispatch_started`, the tree is unlocked and claimed by
nothing, so the next `worktree-prune` reclaims it. Round 1 is committed so no
work is lost, but the warm chain is gone with no diagnostic. Not stated
anywhere.

---

### LOW-8 (correctness) `crates/orgasmic-cli/src/manager.rs:7155` — registration parsing splits porcelain records on `"\n\n"`

`git_worktree_registrations` splits `git worktree list --porcelain` on blank
lines and takes the first `worktree ` / `locked` line per record. I confirmed
the format on git 2.52.0 (`locked <reason>` for a reason, bare `locked`
without), so the generated chain reason parses correctly. An operator-set lock
reason containing a blank line, or a line starting with `worktree `, splits or
fabricates a record — which in the fabricated case is a phantom `Held` and in
the split case drops a real one back to `Unclaimed`. Narrow, but the parser is
one `--porcelain -z` away from being exact.

---

## Open Questions

1. Which test forced the `manager.dispatch_started` arm in
   `torn_close_candidates` (MEDIUM-3)? If none, revert it.
2. Is the ambiguous-failure destruction of a reused chain tree (MEDIUM-4)
   intended fencing, or an overlooked ordering? Either way it needs a comment.
3. `worktree_prune_keeps_a_chain_hold_and_reclaims_after_final_close`
   (dispatch.rs:4132) passes `--no-worktree-remove` on the final close. That is
   necessary to leave something for prune to reclaim, but it means no test
   covers the *default* final close of a reused chain tree. I traced it by hand
   — the tree is unlocked from round 2's `prepare_worktree`, so
   `remove_worktree_required` is not fighting a lock — but it is untested.

## Verification Notes

**Gate 1 — clippy.** `cargo clippy --workspace --all-targets -- -D warnings`
→ `CLIPPY_EXIT=0`, `Finished dev profile ... in 36.36s`, no warnings emitted.
Log: `/tmp/jhwnp-review/gate.log`.

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
             rate    0.0515

verdict: GREEN — no failures.
======================================================================
```

96 passed, 0 failed. `verify/flake-registry.toml` untouched; no flakes observed.

**RMA18 integrity — CONFIRMED intact.** `git diff --numstat 9de4aa0f..cd43bf91`
shows `365/26` on manager.rs, and every one of the 26 deleted lines is
accounted for: the `create_worktree(...)` call replaced by `prepare_worktree`,
one `cleanup_created_resources` call moved into an `if/else`, the old
`remove_worktree` and `worktree_path` bindings, the folder-trust comment moved
below `init_worktree_submodules`, and the `task.state_transitioned` match arm.
No refusal in `worktree_submodule_refusal`, `git_would_remove_worktree`, or
`reclaim_managed_worktree` was deleted, reordered, or re-guarded — the only
edit in that region is a blank doc-comment line (manager.rs:5464). The new
classifier arm (manager.rs:3966-3982) is inserted *below* the cwd, open-dispatch
and live-run `Held` arms and *above* the `worktree_repo_state` match, so it can
only add holds, never a delete. It is load-bearing rather than cosmetic:
without it a locked chain tree classifies `Unclaimed`, and
`reclaim_managed_worktree` would run `settle_as_initialized_submodules` and
`salvage_worktree_onto` — deiniting submodules, staging the index and detaching
HEAD — before git's lock refusal finally stopped it.

**Anchored-identity invariant.** The new arm compares
`registration.path == normalized` rather than going through `claims()`/
`identity_of_path`. Both sides pass through `normalize_path`, which
canonicalizes (manager.rs:9667), so the comparison is sound; and because the
arm only produces `Held`, a mismatch degrades to the pre-existing classification
plus git's own lock refusal. Classified = reserved = deleted is unchanged.

**Reuse safety — no silent fallback found.** I traced every exit of
`prepare_worktree` (manager.rs:7297-7385): unregistered → bail; `git status`
failure → bail; non-empty `status --porcelain --ignore-submodules=none
--untracked-files=normal` → bail with the tree state and `--fresh-worktree`;
foreign lock → bail; `checkout -b` failure → re-lock, then bail. Every message
names the path and the escape. There is no arm that falls back to
`create_worktree`.

**Concurrency — safe, via a pre-existing guard.** A second same-kind dispatch
for an overlapping task is refused at manager.rs:5878-5907, above the reuse
computation at manager.rs:5970, and the reported-handoff exemption only admits a
*different* kind — which never reuses. So no path lets two implementer
dispatches select the same chain tree. Reuse also requires `record.closed`, and
`manager.dispatch_orphaned` is not terminal, so an orphaned round is not
reusable.

**Reviewer freshness — structurally intact.** Reuse is gated on
`args.kind == DispatchKind::Implementer` (manager.rs:5970) *and*
`record.kind == "implementer"` (manager.rs:5975). A reviewer passing
`--worktree <chain-tree>` reaches `create_worktree`, which bails on
`path.exists()` (manager.rs:7409). Verified by
`reviewer_for_a_warm_implementer_chain_still_gets_a_fresh_worktree`.

**RED claims — verified, with one caveat.** `/tmp/TASK-JHWNP-red-tests.log`
(11 378 bytes, 2026-08-28 01:42) shows all four tests FAILED against pre-change
code, and each failure message is consistent with the old behavior rather than
with a broken harness: `worktree path already exists` for the dirty case,
`PRUNE_SUMMARY RECLAIMED=0 ... SKIPPED=0` for the prune case. Caveat: test 4
(reviewer freshness) goes red on its *setup* precondition ("round 2 must be
using the warm checkout", dispatch.rs:4208), not on the reviewer assertion — so
the log does not by itself prove the reviewer assertion bites. I checked by
inspection that it does: deleting the `args.kind == Implementer` guard would
send `worktree_path` to the implementer tree and fail
`assert!(reviewer.is_dir())`.

**Warm-cache claim — verified.** `private_worktree_target_policy`
(manager.rs:7124-7145) is a pure no-op reporter; on a reused tree it returns
`private-target-present` and touches nothing, so the compiled `target/`
directory genuinely survives. `.git/info/exclude` lives in the common git dir,
so the test's ignore marker is shared with linked worktrees as the test assumes,
and `--untracked-files=normal` correctly excludes it from the dirty check.

**Wire surface — unchanged.** `reuse_worktree` was added to `DispatchPlan` but
is deliberately absent from `build_dispatch_request` (daemon_client.rs:270-293),
so nothing new crosses to the daemon and no daemon-side deserialization changed.
The one-line daemon_client.rs edit is a test fixture only.

**Lock lifecycle — traced, per the brief's list.**
- Dispatch startup failure after unlock: **handled** for lifecycle-update failure
  (manager.rs:965) and non-ambiguous daemon rejection (manager.rs:1020) via
  `retain_reused_worktree_after_failed_dispatch`, which re-locks and reports
  `Partial`. **Not handled** on the ambiguous path — see MEDIUM-4.
- Abort mid-round (Ctrl-C between unlock and registration): **leaks the other
  way** — unlocked and unclaimed, prune reclaims it. See LOW-7.
- Final close with `--no-worktree-remove`: **handled** —
  `release_chain_worktree_holds` is unconditional on the removal flags
  (manager.rs:1473).
- Chain abandoned / task cancelled: **leaks a locked orphan, and it is
  accidental, not stated.** This is HIGH-1.
- Close failure after the hold is taken: **safe** — `hold_chain_worktree`
  returns `Ok` early on an existing chain-prefixed lock (manager.rs:7199-7202),
  so re-running the close is idempotent.

**Probe (git behavior).** Throwaway repo, git 2.52.0: `git worktree lock
--reason "orgasmic: next implementer round for TASK-A"` yields the porcelain
line `locked orgasmic: next implementer round for TASK-A`; a reasonless lock
yields bare `locked`; `git worktree remove` on a locked tree refuses with
`fatal: cannot remove a locked working tree ... use 'remove -f -f' to override
or unlock first`; `git worktree prune` leaves locked entries alone. This
confirms both the parser and the "prune can never reclaim it" half of HIGH-1.

**What I did not verify.** I did not run a mutation against the shipped source
(review is read-only), so the "each test bites" claim rests on the RED log plus
inspection, with the test-4 caveat above. I did not run the full workspace suite
— the brief scoped me to clippy and the dispatch suite.

## Fix Directions

1. **HIGH-1** — stop deriving "chain continues" from `--status aborted`. Add an
   explicit `--keep-chain-worktree` (or key on an operator-supplied
   `--no-worktree-remove`), so an abandonment abort keeps today's remove +
   salvage behavior. Then give the lock a release path that does not require a
   future `implementer.done`: either let `worktree-prune` reclaim a
   chain-prefixed lock when no closed-and-unmerged implementer round is pending,
   or add an explicit release verb. Test: abort → cancel the task → prune
   reclaims.
2. **HIGH-2** — compare task lists as sets in both `release_chain_worktree_holds`
   (manager.rs:7264) and the reuse selector (manager.rs:5977), and release any
   hold whose recorded tasks intersect the closing tasks. Test: multi-task
   implementer chain closed one task at a time still releases the hold.
3. **MEDIUM-3** — revert the `manager.dispatch_started` arm unless a test forced
   it; if one did, land that test.
4. **MEDIUM-4** — decide and state the policy. If fencing must win, say so in a
   comment and make the printed error mention that the chain worktree was
   removed. If not, check `plan.reuse_worktree` before the `ambiguous` branch.
5. **MEDIUM-5** — once HIGH-1 lands, document the chain flag, `--fresh-worktree`,
   and the between-round lock in `manager-dispatch.org` and
   `references/dispatch.md`.
6. **LOW-6** — have `create_worktree`'s "path already exists" refusal name
   `--fresh-worktree --worktree <new-path>` when the path is a chain tree.
7. **LOW-8** — switch `git_worktree_registrations` to `--porcelain -z`.

# TASK-JHWNP.1 implementation report

## Changed

- **HIGH-1 fixed:** `--status aborted` again means abandonment by default: it salvages and removes the implementer worktree. Existing `--no-worktree-remove` is now the explicit chain-continuation signal; only that combination keeps and locks the checkout. `worktree-prune` treats an orgasmic chain lock as expired when its full task set can no longer enter another implementer round, then unlocks it only after the existing anchored identity checks and daemon cleanup reservation. Regression: `aborted_chain_cancelled_task_prune_reclaims_worktree`.
- **HIGH-2 fixed:** reusable multi-task rounds compare task lists as sets; final close releases every chain hold whose recorded tasks intersect the close tasks. `implementer_round_reuses_the_chain_worktree_and_warm_cache` now exercises reordered tasks; `multi_task_chain_closed_one_task_at_a_time_releases_hold` exercises partial closes.
- **MEDIUM-3 retained with evidence:** reverting the `manager.dispatch_started` arm made `reviewer_for_a_warm_implementer_chain_still_gets_a_fresh_worktree` fail because torn-close reconciliation moved the task back to `todo`. The arm is therefore required by an existing integration path; `torn_close_candidates_yield_to_any_later_lifecycle_event` now explicitly covers it.
- **MEDIUM-4 fixed:** fencing explicitly wins on ambiguous POST failure. A reused tree is not re-locked; it is handed to daemon cleanup, and the surfaced error now says so.
- **MEDIUM-5 fixed:** both manager dispatch documents now describe the explicit `--no-worktree-remove` chain signal, `--fresh-worktree --worktree <new-path>`, the native between-round lock, expired-hold pruning, and the Ctrl-C window.
- **LOW-6 fixed:** an occupied chain path now names `--fresh-worktree --worktree <new-path>` in the refusal.
- **LOW-7 stated:** documented the interrupt window between reuse unlock and daemon registration, plus the safe operator response. No broader signal/RAII redesign was introduced.
- **LOW-8 fixed:** worktree registrations now parse `git worktree list --porcelain -z` as NUL-delimited records and fields.

Commit: `3503b86b` (`fix(cli): chain-hold release paths — explicit keep signal, set-based release, prune reclaim (TASK-JHWNP.1)`).

## Red evidence against cd43bf91

- HIGH-1: `/tmp/TASK-JHWNP.1-high1-red-20260828.log` — `aborted_chain_cancelled_task_prune_reclaims_worktree` failed because prune printed `SKIP ... held for the next implementer round` and `RECLAIMED=0` after cancellation.
- HIGH-2: `/tmp/TASK-JHWNP.1-high2-red2-20260828.log` — `multi_task_chain_closed_one_task_at_a_time_releases_hold` failed with the retained `locked orgasmic: next implementer round for TASK-BUNDLE-A TASK-BUNDLE-B` registration.

## Verification Gates

- `cargo fmt --all` — exit 0. Unrelated pre-existing rustfmt churn outside the four scoped files was excluded from the commit.
- Focused regressions: `/tmp/TASK-JHWNP.1-focused-green2-20260828.log` — 5 chain tests green, the default-abort salvage/removal test green, and the explicit torn-close unit test green.
- `cargo clippy --workspace --all-targets -- -D warnings` — exit 0; log `/tmp/TASK-JHWNP.1-clippy-20260828.log`.
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch` — log `/tmp/TASK-JHWNP.1-dispatch-gate-20260828.log`:

```
VERDICT
======================================================================
  suite    : cargo test -p orgasmic-cli --test dispatch --no-fail-fast -- --skip legacy_drivers_and_explicit_pairs_emit_equivalent_start_events
  log      : /var/folders/9p/823z6j817xj9ts2xpvnx1q_40000gn/T//orgasmic-run-tests.kwZNWQ/suite.log
  registry : verify/flake-registry.toml (7 entries, every owner open)
  billed   : legacy_drivers_and_explicit_pairs_emit_equivalent_start_events — NOT RUN (--skip applied to every invocation)
  failures : 0
  crashed  : none — every failing target reported a failure list
  cargo    : exited 0 (0 all green, 101 libtest reported failures)
  ignored  : 0 test(s) carrying #[ignore]
  cfg-off  : passed — the pause rendezvous hooks are compiled out without debug_assertions
  environ  : complete — no tool requirement was waived
  host     : calm (threshold syspolicyd_rate>=1.50; load corroborating only)
             before  load=7.22 syspolicyd_time=109:20.77
             after   load=42.46 syspolicyd_time=109:29.58
             delta   load=7.22 syspolicyd_cpu=8.8 wall_s=100
             rate    0.0880

verdict: GREEN — no failures.
======================================================================
```

The full dispatch suite includes the RMA18 refusal regressions; reviewer freshness also passed in the focused chain run. `verify/flake-registry.toml` was untouched.

## Unmet Criteria

None.

## Residual Risk

- The ambiguous daemon-failure policy is stated in code and surfaced in errors, but this fix round did not add a transport-timeout fault-injection test.
- The documented Ctrl-C window remains by the assignment's explicit “state or fix” option; a broader interrupt guard was intentionally not added.
- No full workspace test suite was run; the brief named clippy and the dispatch suite as the required gates.

# TASK-JHWNP review brief — reviewer persona compiled in

## Under review
- Diff: `9de4aa0f..cd43bf91` (one commit, branch `task-jhwnp-impl`) — worktree-per-task-chain: same-task implementer rounds reuse the chain worktree; reviewers always get a fresh checkout.
- Task spec + acceptance: `orgasmic task get --project orgasmic TASK-JHWNP`.
- Implementer report: `.orgasmic/tasks/TASK-JHWNP/dispatches/tx-20260827-orgasmic-641e58ea-dc8f-4ac4-9cbf-170ae7a53975/` (report + last.txt).

## What to grill hardest
- **RMA18 integrity.** The implementer's between-rounds hold is a native `git worktree lock`, classified as held by the prune scanner and released at final close. Verify no refusal in `worktree_submodule_refusal`, `git_would_remove_worktree`, or `reclaim_managed_worktree` was weakened or re-ordered, and the anchored-identity invariant (classified = reserved = deleted) holds across a reuse round.
- **Reuse safety.** Dirty / unregistered / unexpectedly-locked candidates must refuse with actionable messages — prove there is NO path where reuse silently falls back to a fresh tree or, worse, proceeds on a dirty tree. What happens when the previous round's dispatch is still open? When two dispatches race for the same chain worktree?
- **Lock lifecycle leaks.** A lock taken and never released is a worktree pruned never. Trace every path: dispatch startup failure after unlock, abort mid-round, final close with `--no-worktree-remove`, chain abandoned (task cancelled). Which of these leaves a locked orphan, and is that stated or accidental?
- **Reviewer freshness.** The fresh-checkout-at-commit gate must be structurally intact: reviewer dispatches can never reuse, including via explicit `--worktree` pointing at the chain tree.
- **Acceptance honesty.** All four acceptance tests exist, each was RED against pre-change code (`/tmp/TASK-JHWNP-red-tests.log` claimed) — verify the red claims by reverting mentally or by mutation, not by trust.

## Non-negotiables (chain-standing)
- Daemon is the write authority for `.orgasmic/**`; never hand-edit state files.
- NEVER set `ORGASMIC_HOME` on any orgasmic invocation. `ORGASMIC_DAEMON_URL` on a child is safe.
- NEVER run `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`; never set `ORGASMIC_ALLOW_BILLED_TESTS`.
- Do not touch `verify/flake-registry.toml`; report flakes honestly.

## Gates you run yourself (quote VERDICT blocks, never a raw `test result:` line)
- `cargo clippy --workspace --all-targets -- -D warnings`
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch`
- Any targeted mutation you use to prove a test bites.

## Verdict
APPROVE or REJECT with per-finding severity (HIGH/MEDIUM/LOW), file:line, and the failure scenario. A finding without a concrete failure scenario is an observation, not a finding.

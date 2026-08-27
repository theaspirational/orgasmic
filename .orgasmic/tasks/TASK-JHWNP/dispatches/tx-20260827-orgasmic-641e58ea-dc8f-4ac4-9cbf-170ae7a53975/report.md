## Changed
- Reuse the latest closed same-task implementer worktree unless `--fresh-worktree` is supplied; create each new round branch from `--from` inside that checkout.
- Refuse dirty, unregistered, or unexpectedly locked reuse candidates with the tree state and fresh-worktree escape.
- Keep aborted implementer rounds with a native `git worktree lock`; prune classifies that lock as held, reuse unlocks it, and final implementer close releases remaining chain holds.
- Reviewer dispatches remain fresh and cross-kind default-path refusal is unchanged.
- Preserve initialized submodules on reuse and retain/re-lock reused trees if dispatch startup fails.

## Verification Gates
- RED evidence: all four TASK-JHWNP acceptance regressions failed before the implementation (`/tmp/TASK-JHWNP-red-tests.log`).
- `cargo fmt --all`: exit 0; unrelated pre-existing formatter drift outside the task files was restored.
- `cargo clippy --workspace --all-targets -- -D warnings`: VERDICT exit 0 (`/tmp/TASK-JHWNP-clippy-final-2.log`).
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch`: VERDICT GREEN, failures 0, crashed none, billed test NOT RUN, environment complete, host calm (`/tmp/TASK-JHWNP-dispatch-suite-final.log`).

## Unmet Criteria
- None.

## Residual Risk
- `--fresh-worktree` needs an explicit alternate `--worktree` while the held managed default still exists; the CLI help and refusal name that requirement.

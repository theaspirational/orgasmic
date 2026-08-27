# TASK-JHWNP brief — delta only (task body carries the design + acceptance; implementer persona compiled in)

## Read first
- `crates/orgasmic-cli/src/manager.rs`: `create_worktree` (now also inits submodules with superproject alternates), `cmd_dispatch` (worktree pathing + the TASK-096.1 cross-kind collision refusal), `cleanup_dispatch` / `remove_worktree_required` / `reclaim_managed_worktree` (the RMA18 anchored-removal machinery — read its doc comments IN FULL before touching anything near it), `settle_as_initialized_submodules` (new; chain reuse must not break it).
- `crates/orgasmic-cli/tests/dispatch.rs`: the `worktree_prune_*` family and `dispatch_rejects_cross_kind_default_worktree_reuse` — extend, don't duplicate.
- The task body (`orgasmic task get --project orgasmic TASK-JHWNP`) is the spec: same-kind implementer rounds reuse the chain worktree; reviewers always get fresh; between-rounds prune protection; final close reclaims.

## Design constraints (non-negotiable)
- A fresh checkout at the worker commit stays the merge gate: reviewer worktrees are NEVER reused.
- Reuse only a CLEAN tree (previous round committed). Dirty/wedged → refuse with the state and the escape named; no silent fresh-worktree fallback.
- Do not weaken ANY refusal in `worktree_submodule_refusal`, `git_would_remove_worktree`, or `reclaim_managed_worktree`. The between-rounds hold must be a state those verbs READ, not a carve-out inside them.
- One managed directory per chain: the anchored-identity invariants (classified = reserved = deleted) must hold across rounds.

## Non-negotiables (chain-standing)
- Daemon is the write authority for `.orgasmic/**`; never hand-edit state files.
- NEVER set `ORGASMIC_HOME` on any orgasmic invocation. `ORGASMIC_DAEMON_URL` on a child is safe.
- NEVER run `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`; never set `ORGASMIC_ALLOW_BILLED_TESTS`.
- Do not touch `verify/flake-registry.toml`; report flakes honestly.

## Gates (quote VERDICT blocks, never a raw `test result:` line)
- `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`
- `scripts/run-tests.sh -p orgasmic-cli --test dispatch` — all green (the suite is timing-sensitive under load; a lone timeout rerun-passing is a flake, say so honestly)
- New tests per the task's acceptance list, each red against the pre-change code path it pins.

## Commit discipline
Commit early, commit often. Final commit message:

`feat(cli): worktree-per-task-chain — implementer rounds reuse the chain worktree, reviewers stay fresh (TASK-JHWNP)`

## Report
Design decisions (reuse detection, the between-rounds hold mechanism, branch handling in a reused tree), per-acceptance-item outcome, gates with VERDICT quotes, surprises measured.

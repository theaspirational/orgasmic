# TASK-JHWNP.1 brief — fix round on cd43bf91 (task body carries all eight findings)

## Read first
- The full review (verbatim findings, file:line, probes): /Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tmp/dispatch/task-jhwnp-review/task-jhwnp-review-9d90899c9a3b4b9185b139b6d3eb1d69-last.txt
- Your round-1 chain: branch `task-jhwnp-impl` at cd43bf91 — build ON it (`--from` is cd43bf91), do not restart the design.
- Task body: `orgasmic task get --project orgasmic TASK-JHWNP.1`.

## Priorities
Fix both HIGHs with their named regression tests (red against cd43bf91) first; then MEDIUMs; LOWs last. If a MEDIUM/LOW turns out wrong, push back with a concrete reason in the report instead of complying silently.

## Non-negotiables (chain-standing)
- Daemon is the write authority for `.orgasmic/**`; never hand-edit state files.
- NEVER set `ORGASMIC_HOME` on any orgasmic invocation. `ORGASMIC_DAEMON_URL` on a child is safe.
- NEVER run `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`; never set `ORGASMIC_ALLOW_BILLED_TESTS`.
- Do not touch `verify/flake-registry.toml`; report flakes honestly.
- Do not weaken any refusal in `worktree_submodule_refusal`, `git_would_remove_worktree`, `reclaim_managed_worktree`; reviewer worktrees stay fresh.

## Gates (quote VERDICT blocks, never a raw `test result:` line)
- `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch`

## Commit discipline
Commit early, commit often. Final commit message:
`fix(cli): chain-hold release paths — explicit keep signal, set-based release, prune reclaim (TASK-JHWNP.1)`

## Report
Per-finding outcome (fixed-how / pushed-back-with-reason), red-test evidence for both HIGHs, gates with VERDICT quotes, surprises measured.

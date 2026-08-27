# TASK-JHWNP.1 re-review brief — reviewer persona compiled in

## Under review
- Diff: cd43bf91..3503b86b (one commit, fix round for your predecessor's REJECT).
- Round-1 review with all eight findings (verbatim): /Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/tmp/dispatch/task-jhwnp-review/task-jhwnp-review-9d90899c9a3b4b9185b139b6d3eb1d69-last.txt
- Fix-round report: the dispatch record under ~.orgasmic/tasks/TASK-JHWNP.1/dispatches/~ (started_tx tx-20260827-orgasmic-3f912068-b9fb-42c8-b77b-e4debe78b8e1).
- Task bodies: `orgasmic task get --project orgasmic TASK-JHWNP.1` (findings list) and TASK-JHWNP (original design + acceptance).

## Your job
Per-finding verdict: each of HIGH-1, HIGH-2, MEDIUM-3/4/5, LOW-6/7/8 — fixed, adequately pushed back, or still open. For the two HIGHs, verify the regression tests are red against cd43bf91 (mutation or inspection with the exact guard named), and specifically re-trace HIGH-1's abandonment path: abort → task cancelled → prune reclaims, AND the abort path writes salvage refs again. Then a fresh sweep of the NEW code the fix introduced (release verb / keep flag / set-based matching) for holes the first round could not have seen. State residual risks the implementer named (no transport-timeout fault injection; documented Ctrl-C window) as accepted-or-blocking.

## Non-negotiables (chain-standing)
- Daemon is the write authority for ~.orgasmic/**~; never hand-edit state files.
- NEVER set ~ORGASMIC_HOME~ on any orgasmic invocation. ~ORGASMIC_DAEMON_URL~ on a child is safe.
- NEVER run ~legacy_drivers_and_explicit_pairs_emit_equivalent_start_events~; never set ~ORGASMIC_ALLOW_BILLED_TESTS~.
- Do not touch ~verify/flake-registry.toml~; report flakes honestly.

## Gates you run yourself (quote VERDICT blocks, never a raw test-result line)
- `cargo clippy --workspace --all-targets -- -D warnings`
- `ORGASMIC_ALLOW_MISSING_TOOLS=tmux scripts/run-tests.sh -p orgasmic-cli --test dispatch`

## Verdict
APPROVE or REJECT with per-finding severity, file:line, and concrete failure scenarios.

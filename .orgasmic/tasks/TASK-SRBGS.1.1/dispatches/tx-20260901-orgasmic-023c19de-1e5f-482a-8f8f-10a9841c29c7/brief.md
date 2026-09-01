# TASK-SRBGS.1.1 — residuals of the SRBGS.1 review (route-list test, anomalies, phantom owner, cutover recovery, 503 pin)

Read the task first: `orgasmic task get --project orgasmic TASK-SRBGS.1.1` — each finding
with `file:line`, the fix direction and the acceptance list. Line numbers are approximate;
read the current files. Everything below is the minimum.

## 1. MEDIUM — make the route-list drift test honest
`crates/orgasmic-daemon/src/api.rs` (~24292, test `shipped_tx_types…`): today it compares the
shipped bullet block to `DISPATCH_PROJECT_TX_TYPES` filtered by `!event_routes_to_journal`,
and `event_routes_to_journal` (~8783) returns `false` for that very constant — a tautology.
Rebuild it from behaviour: take the exhaustive `(type, routes_to_journal)` pin table already
in the tests (~24150-24240), collect every type whose value is `false`, subtract a small
NAMED exclusion set of non-dispatch types (`ledger.sync_conflict`, `manager.action`,
`manager.correction`, `task.claimed`, anything ending `.deleted`, …), and assert equality
with the parsed shipped block. Delete the dead `DISPATCH_PROJECT_TX_TYPES` short-circuit in
`event_routes_to_journal` (it changed no routing). Prove the test bites: temporarily add a
fake `false` type to the pin table without a doc entry, watch it go red, revert; say so in
the report.

## 2. LOW — anomalies line
`crates/orgasmic-cli/src/project_migrate.rs` (~89, ~204): the only increment is followed by
`bail!`, so the print can only ever say 0. Delete the `anomalies` line and the field if
nothing else reads it. Do NOT make `plan()` survey the whole tree.

## 3. LOW — phantom apply-failure owner
`crates/orgasmic-daemon/src/writer.rs` (~1421 `mutate_file`): the owner is a fresh
`Uuid::new_v4()` nobody can `take_apply_failure` with. Make the owner `Option<&str>` and skip
the `apply_failures` insert when `None` (the path is still queued on `unapplied`), or pass
the request id through. No spurious "belongs to another request" warn on a plain journal
write — assert it in the existing comment/journal tests if a cheap hook exists.

## 4. LOW — branch cutover recovery text
`project_migrate.rs` (~71): `migrate_to_branch` (create orphan branch → `git worktree add` →
`remove_dir_all(root/.orgasmic)`) is unwrapped. Give it its own error context that lists the
exact undo for THIS run based on which steps completed (worktree remove <path>, branch
delete, `git -C <tree> checkout -- .orgasmic` only if the dir was already removed). The L2
checkout/clean text alone is wrong advice here. One test with a failure forced after the
orphan branch exists.

## 5. LOW — owner-side 503 pin
`api.rs` (~32874 `apply_failure_is_not_reported_by_the_next_request`): drive request A through
`refresh_after_tx`, assert A's own committed-503 carries A's tx id, then B gets 200.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-daemon --lib -- shipped_tx_types apply_failure ledger_route comment`
- `cargo test -p orgasmic-cli --bin orgasmic -- project_migrate` (targeted; NEVER unfiltered)
- `cargo clippy -p orgasmic-daemon -p orgasmic-cli --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-SRBGS.1.1: fix(follow-ups): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, the
  red-then-green proof for item 1, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).

# Review: TASK-SRBGS.1 — chain-review L1–L5 (fail closed, recovery text, per-tx apply failure, route-list drift test)

Implementer: codex gpt-5.6-sol, one commit `79caf335`, merged to main as `c56b0bbe`.
Read the task first: `orgasmic task get --project orgasmic TASK-SRBGS.1`. Then:

    git diff c56b0bbe^1 c56b0bbe

Nine files, +277/-66: `crates/orgasmic-core/src/identity_lint.rs` + `id_repair.rs`,
`crates/orgasmic-daemon/src/{writer.rs,api.rs,index.rs,ledger_sync.rs}`,
`crates/orgasmic-cli/src/project_migrate.rs`, `crates/orgasmic-cli/tests/id_collision_repair.rs`,
`shipped/schema/tx.org`.

## What the fix claims
- **L1** `identity_lint.rs:~220-263` `collect_identity_occurrences` / `collect_reference_occurrences`
  return `Result` with context; callers in `id_repair.rs:~187,225` and `index.rs:~3482,3507,3839`
  surface the error instead of a clean lint. Unix unreadable-dir regression test.
- **L2** `project_migrate.rs:~379-389,650-676` wraps a non-branch apply failure with the exact
  `git -C <tree> checkout -- .orgasmic` / `git -C <tree> clean -fd -- .orgasmic` recovery; a test
  forces a partial apply and checks both commands and the first written node.
- **L3** `writer.rs:~537,857-926` keys apply failures by the owning tx; a successful repair drops
  repaired foreign failures; `api.rs:~8592,8637` take only the current request's failure. Test
  at `api.rs:~32874`: a later request repairs rather than inheriting the earlier 503.
- **L4** `shipped/schema/tx.org:~113-126` gains `implementer.commit_pending` and `fixer.done`;
  `api.rs:~8766-8784` owns the Rust route set and a test (`~24272`) parses the shipped bullet
  block and requires exact equality. Also: `ledger_sync.rs:~403-410` now writes
  `ledger.sync_conflict` under `machines/<id>/tx/` (this is the routing HIGH from the 8DWJP
  review, fixed here in passing).
- **L5** `project_migrate.rs:~51,89,204` counts byte-unstable heading round-trips in
  `Migration::anomalies` and prints that.

## Attack these specifically
- **L3 is the money path.** Walk the writer's apply/commit flow: when the async apply for tx A
  fails, does request A itself still learn of it (or has A already returned 200)? If A has
  returned, who surfaces A's failure now — a log line only? Previously ANY next request
  returned the 503 (wrong request, but loud); now a foreign failure is "logged and left for its
  owner" or "dropped after repair". Can a failure be dropped with NO caller ever seeing a 503
  and NO repair having actually fixed the projection? Is the map bounded (owner never comes
  back → entry lives forever)? Is the key the same identifier at insert and take (request_id
  vs tx id vs generation)? Does the new test prove the ORIGINAL failure is still reported, not
  only that B escapes it?
- **L1 fail-closed scope.** `index.rs:~3482-3519, ~3839-3851`: are those the lint-report paths
  or the index-load/refresh path? If a single unreadable collection dir now fails a whole
  index refresh or project load that previously succeeded, that is a regression (MEDIUM+).
  NotFound must still map to empty (a fresh project has no `decisions/`). Check the Unix test
  cleans up its chmod 000 dir on failure (a leftover read-only dir breaks `cargo clean`).
- **L2 correctness of the recovery commands.** Does `apply()` write ONLY under `.orgasmic`
  (any `views/`, `.gitignore`, or repo-root file?) — if it touches anything else the printed
  `checkout -- .orgasmic` / `clean -fd -- .orgasmic` leaves the tree dirty and `plan()` keeps
  refusing. What is the "branch migration" case that is NOT wrapped, and what does an operator
  see there? Is `<tree>` the absolute path the operator can paste?
- **L4 honesty.** Is the "Rust route set" the actual behaviour of `event_routes_to_journal`
  (e.g. every known type pushed through the function), or a second hand-maintained constant
  that can drift from the function exactly as the doc did? If it is a constant, that is the
  finding. Does the parse of the shipped block break on a reflowed bullet or a trailing
  comment? Confirm the `ledger.sync_conflict` path fix matches the doc and that the updated
  `conflicting_two_writer_tick_parks_recovers_and_records_event` asserts the literal
  `machines/<id>/tx/<month>.org` rather than re-deriving the expression.
- **L5.** Is `anomalies` the thing the round-trip actually checks (headings whose rewrite is not
  byte-stable), counted from real data, or a new always-zero field?
- **`id_collision_repair.rs` (2 lines).** Why did an integration test change — a type change or
  a weakened assertion?

Already established — do not re-spend: the implementer ran 9 gates green; the manager re-ran
on merged main `c56b0bbe`: `cargo test -p orgasmic-core --lib identity_lint`, `cargo test -p
orgasmic-cli --bin orgasmic -- project_migrate`, `cargo test -p orgasmic-cli --test
id_collision_repair`, `cargo test -p orgasmic-daemon --lib -- apply_failure shipped_tx_types
ledger_route ledger_sync`, clippy core+cli+daemon `-D warnings`, fmt — see `orgasmic task get
--project orgasmic TASK-SRBGS.1` Evidence. Targeted re-runs are fine; never the workspace.

## Rules
- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic`. The live daemon on :4848 runs the PRE-fix runtime.
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-SRBGS.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only; NEVER the whole `orgasmic-cli` suite unfiltered; never the workspace;
  never `ORGASMIC_HOME`; never `daemon start`; do not read `verify/*/injection.patch`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

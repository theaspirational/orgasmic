# TASK-SRBGS.1 — five LOW follow-ups from the chain review

Fix round for L1–L5 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-SRBGS.1` — it has the exact
`file:line` per item. Five small, independent edits; one commit is fine, or one per item.

- **L1** `crates/orgasmic-core/src/identity_lint.rs:218,225,245` —
  `collection_node_file_paths(...).unwrap_or_default()` swallows real IO errors (the helper
  already maps NotFound to `Ok(vec![])`). Make `collect_identity_occurrences` and
  `collect_reference_occurrences` return `Result` and propagate with context; update their
  callers. Test: an unreadable collection dir (chmod 000 on Unix, skip on Windows) yields
  `Err`, not an empty clean report.
- **L2** `crates/orgasmic-cli/src/project_migrate.rs:353 apply()` — not atomic, no recovery;
  after a partial failure `plan()` bails (target exists) and `refuse_dirty_tree()` bails, so
  the verb refuses forever unless the operator knows to `git checkout -- . && git clean -fd`.
  Minimum: on `apply()` error, print the exact recovery commands for THIS tree (paths
  included) in the error context, and add a test that injects a failure mid-apply (a
  read-only target dir is enough) and asserts the message names the recovery. Do NOT make
  `apply()` resumable — document, do not engineer.
- **L3** `crates/orgasmic-daemon/src/api.rs:8569,8614` — `writer.take_apply_failure()` is one
  daemon-wide slot; a failure from request A surfaces as a committed-503 on unrelated
  request B, and that early return skips `repair_projection`. Key the slot by the request
  that caused it (the writer already knows the `request_id` / tx it was applying — carry it
  in the failure record) and have the two call sites take only THEIR failure; a foreign
  failure is logged and left for its owner (or dropped after one repair attempt — say
  which). Test: two sequential requests, first fails apply, second must NOT see a 503.
- **L4** `shipped/schema/tx.org` (the "complete routed type set" block; line numbers in the
  task body are stale — TASK-8DWJP merged today and added `ledger.sync_conflict` to that
  block, see `git show 200892f2 -- shipped/schema/tx.org`) — the list omits `fixer.done` and
  `implementer.commit_pending`, both routed to tx/ by `event_routes_to_journal`
  (`crates/orgasmic-daemon/src/api.rs`, `rg -n 'fn event_routes_to_journal'`). Add them to
  the list AND add a test that parses that list block out of the shipped file and asserts it
  equals the set of types the Rust function routes to tx/ (so the two cannot drift again).
  Keep the parse dumb: the bullet lines between the "complete routed type set is:" line and
  the next blank line. If the new test reveals that `ledger.sync_conflict` is listed in
  tx.org but NOT routed to tx/ by the function (or vice versa), fix the CODE side so the
  daemon-originated, task-less `ledger.sync_conflict` event routes to machines/<id>/tx/
  (that is what dec_EWY0K and TASK-8DWJP intend) and say so in the report.
- **L5** `crates/orgasmic-cli/src/project_migrate.rs:86` — `println!("  anomalies 0")` is a
  literal. Print the counted value from the migration struct; if no anomaly count exists,
  compute the one thing the round-trip actually checks (headings whose rewrite was not
  byte-stable) and print that.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --lib identity_lint`
- `cargo test -p orgasmic-cli --bin orgasmic -- project_migrate` (targeted; NEVER unfiltered)
- `cargo test -p orgasmic-daemon --lib -- apply_failure routes_to_journal shipped_tx_types`
  (use your real test names)
- `cargo clippy -p orgasmic-core -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commits `TASK-SRBGS.1: fix(<area>): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`) per item, each gate with its pass/fail line and log
  path, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).

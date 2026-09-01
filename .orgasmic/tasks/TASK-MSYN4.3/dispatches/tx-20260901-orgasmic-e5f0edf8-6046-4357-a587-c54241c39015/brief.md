# TASK-MSYN4.3 — tx ids can collide across machines (M5)

Fix round for finding M5 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-MSYN4.3`.

## What is actually true today (read `crates/orgasmic-daemon/src/writer.rs:2793-2812`)
`prepare_tx_entry` has TWO mint paths under `TxIdPolicy::ProjectSequence`:
- `is_machine_tx_path(&req.tx_path)` (appends to `machines/<id>/tx/`) →
  `tx-{date}-{slug}-{uuid_v4}`. Collision-free. Every dispatch lifecycle event since MSYN4
  takes this path (live ids look like `tx-20260901-orgasmic-bc9860e5-…`).
- everything else (node journals `tasks/<ID>/journal.org`, legacy `.orgasmic/tx/`) →
  `next_project_tx_id` (`:2945`): per-project sequence `tx-{date}-{slug}-{seq:04}` served
  from an in-memory `ProjectTxSeqCache` (`by_project_month`, `project_max`), seeded by
  `scan_project_tx_max_seq` over `tx/` + `machines/*/tx/` (NOT journals) and cleared only
  when a tx handle detaches from its path (`tx_handles_detached_from_paths`). Live ids from
  this path look like `tx-20260901-orgasmic-6829`.

So the dispatch-fold keys the finding worries about (`close_dispatch` CLOSED_TX,
`attach_initial_run` DISPATCH_TX in `crates/orgasmic-core/src/tx.rs:172,217`;
`recorded_close_allows_repair` in api.rs) already run on uuid ids on any post-MSYN4 ledger.
The REAL residual: two machines minting journal entry ids concurrently produce identical
`tx-…-NNNN` ids for different events, and a pull that brings higher sequences does not
refresh `project_max`, so this machine re-mints ids that already exist remotely.

## What to do — the minimum (deletion over addition)
1. Mint uuid ids on BOTH paths: make the `else` branch use the same
   `tx-{date}-{slug}-{uuid}` format. Then delete `next_project_tx_id`,
   `scan_project_tx_max_seq`, `ProjectTxSeqCache` and its clear/invalidation plumbing,
   `test_hooks::record_scan` if nothing else uses it, and every test that only exercised
   the sequence (keep any that pin the uuid shape or the EVENT_ID handling). Existing
   `tx-…-NNNN` ids remain valid references — nothing parses the tail.
2. BEFORE step 1, prove nothing orders or parses by the numeric tail: grep for consumers
   that sort by `tx_id` / `entry_id`, parse the trailing `:04` number, or compare ids as
   sequence (UI included: `rg -n "tx_id|entry_id" ui/src | rg -i "sort|compare|localeCompare"`).
   If something does, report it and switch that consumer to `TIME` (org timestamp, already
   lexicographically ordered) — do not keep the sequence to satisfy a sort.
3. Two-writer collision test in the fold (acceptance): construct two machines' tx files
   whose `manager.dispatch_started` entries would have collided under the old scheme (same
   project, same date, same seq) and assert the fold attributes each `implementer.done`
   (by `CLOSED_TX`) to the right generation with uuid ids — i.e. a test that FAILS if
   someone reintroduces per-project sequences on the machine path. Put it next to the
   existing fold tests in `crates/orgasmic-core/src/tx.rs`.
4. If `TxIdPolicy` has other variants that still carry a sequence (`rg -n "enum TxIdPolicy"
   -A 12 crates/orgasmic-daemon/src/writer.rs`), leave them alone unless they hit the same
   cross-machine path; say which you left.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --lib tx`
- `cargo test -p orgasmic-daemon --lib -- tx_id sequence writer::tests::prepare` (add your
  own test names if these substrings miss them; the point is every test that touched the
  deleted code is gone or green)
- `cargo clippy -p orgasmic-core -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commit as `TASK-MSYN4.3: fix(writer): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER set
  `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`.
- Report: what changed (`file:line`), what you deleted, the consumer grep result from step 2,
  each gate with its pass/fail line and log path, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).

# TASK-TP593.1 — eight tests that cannot fail (M2 + M3)

Fix round for findings M2 + M3 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-TP593.1`.

## The defect
- M2: `node_kernel::real_data::every_migrated_node_parses`
  (`crates/orgasmic-core/src/node_kernel.rs:428`) returns early unless
  `ORGASMIC_MIGRATED_DIR` is set — set nowhere in the tree; the script its doc comment
  names does not exist. It has never asserted anything in any gate.
- M3: `crates/orgasmic-core/tests/fixtures.rs:35 live_ledger_present()` is false on every
  fresh clone since the LBRX7 cutover (no `.orgasmic/project.org` in the source tree), so
  seven tests print "skipping" and pass: `parses_real_done_tasks` (:63),
  `live_state_files_parse_without_retired_property_warnings` (:105),
  `parses_real_decisions` (:247), `parses_real_glossary` (:280), `parses_real_project` (:293),
  `parses_real_tx_file` (:304), `round_trip_through_section_body_rewrite` (:478). They hard
  code live ids: `TASK-VWBDJ`, `dec_R75SW`, `term_YC32J`, and read `.orgasmic/tx/*`.

## Decision (made by the manager — implement, do not re-litigate)
Keep the seven corpus tests and make them REAL by committing a small fixture ledger; delete
the migrated-node test.

1. Create `crates/orgasmic-core/tests/fixtures/ledger/.orgasmic/` by COPYING from the live
   ledger at `/Users/aspirational/.orgasmic/ledgers/orgasmic/.orgasmic/` (read-only source):
   `project.org`; `tasks/TASK-VWBDJ/` (node.org + journal.org); three more task dirs — pick
   two `done` and one `cancelled` so `parses_real_done_tasks` and the retired-property lint
   exercise real shapes; `decisions/dec_R75SW/`; `glossary/term_YC32J/`; ONE legacy
   `tx/2026-08.org` (it is ~large — truncate to its first ~40 entries, keep the header);
   and `.gitignore`. Nothing from `machines/`, `tmp/`, `views/`, nothing else. Keep the
   fixture under ~300 KB total; say the final size in the report.
2. Replace `live_ledger_present()` with `fixture_ledger_root()` returning that path, and
   point every `repo_root().join(".orgasmic/…")` / `collection_node_file_paths(&repo_root(),…)`
   in those seven tests at the fixture root. Delete the "skipping" branch entirely — the
   tests must fail if the fixture is missing.
3. Delete `mod real_data` (`node_kernel.rs:422-451`). The migrator has its own tests in
   `crates/orgasmic-cli/src/project_migrate.rs`; do not move the deleted test there.
4. Fix the file header comment in `fixtures.rs` (it still says the corpus is "committed to
   this repo" under `.orgasmic/`).

If a copied node fails to parse or a test's assumption does not hold for the copied
content, fix the TEST's expectation only when the content is legitimately valid; never edit
the copied files to make a test pass — pick a different node instead and say which.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-core --test fixtures` — must show the seven tests running (0 skips
  printed) and passing
- `cargo test -p orgasmic-core --lib node_kernel`
- `cargo clippy -p orgasmic-core --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commit as `TASK-TP593.1: test(core): <one line>`; the fixture
  files go in the same commit.
- The live ledger is READ-ONLY source material; never write there, never run `orgasmic`
  mutating verbs. NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`.
- Report: what changed (`file:line`), which nodes you copied and why, fixture size, each gate
  with its pass/fail line and log path, unmet criteria, residual risk. Finish with
  `orgasmic dispatch finalize --summary-file <path>` (report only, no `--commit`).

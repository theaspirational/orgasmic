# TASK-EPG6H.1 — torn-close repair guards read a surface `task.state_transitioned` no longer lands on (H3)

Fix round for finding H3 of the whole-chain review (tx-1c6d2115, claude-opus-5 high).
Read the task first: `orgasmic task get --project orgasmic TASK-EPG6H.1`.

## The defect

EPG6H routes every `task.state_transitioned` for task X to `.orgasmic/tasks/X/journal.org`
(measured on the live ledger 2026-09-01: 51 journals carry it, `machines/*/tx/` carry 0).
Two guards still decide "was there a later transition?" by scanning ONLY `.orgasmic/tx/` and
`.orgasmic/machines/*/tx/`:

- CLI `torn_close_candidates` (`crates/orgasmic-cli/src/manager.rs:9764`) over
  `read_tx_entries` (`manager.rs:10748`). Its `"task.state_transitioned"` arm (`:9795`) is
  dead; only `manager.dispatch_started` still clears a candidate. The doc comment at `:9694`
  ("Any later task.state_transitioned — including one an operator made on purpose — clears
  the candidate") is now false.
- Daemon `recorded_close_allows_repair` (`crates/orgasmic-daemon/src/api.rs:18497`) over
  `project_tx_entries` (`api.rs:3773`). Its `"task.state_transitioned" => allowed = false`
  arm (`:18526`) is dead for the same reason.

Scenario: a close moves a task in_progress→in_review; the operator moves it back to
in_progress; the next manager command sees the close as the last lifecycle event with the
task sitting at `LIFECYCLE_FROM`, calls it torn, and drags it to in_review again — and the
daemon accepts the repair. Plus: `repair_allowed` also skips the ART-04FYD Done evidence gate
(`api.rs:18424`), which the acceptance says it must not.

## What to do — the minimum

### 1. Fold the task's own journal into both guards
Each guard only cares about ONE task per decision, so do not widen `read_tx_entries` /
`project_tx_entries` (they back dispatch-status/wait/close and would start reading ~900
journals per command). Instead, per task: read `.orgasmic/tasks/<ID>/journal.org` if it
exists, `orgasmic_core::node_kernel::parse_journal` it, and merge its entries into the scan.

- Daemon: `crate::index::journal_tx_entry` (`index.rs:3690`) already converts
  `JournalEntry → TxEntry`; make it `pub(crate)` and reuse it. Do not write a second converter.
- CLI: the guard only needs `(time, ty)` of journal entries — no conversion needed; a
  `JournalEntry` has `.time` and `.ty`.

**Ordering.** Today both scans rely on file/append order. After folding, order by `time`
(the org timestamp string `[YYYY-MM-DD Day HH:MM:SS]` sorts lexicographically). Use a STABLE
sort over `tx entries ++ journal entries` so that a same-second journal transition sorts
AFTER the close tx — that is the direction that protects the operator's move-back.

`torn_close_candidates` is one pass over all tx entries building `pending` per task; the
simplest fold is a second step: for each pending `(started_tx, transition)`, read that task's
journal and drop the candidate if any `task.state_transitioned` (or `manager.dispatch_started`)
entry has `time >= close.time`. Keep the close tx's `time` in what you push to `pending`
(add a field to the tuple or to `CloseTransition`; either is fine). Same shape in
`recorded_close_allows_repair`: find the matching close tx, then refuse if the task journal
has a `task.state_transitioned` at or after it.

### 2. Evidence gate
`api.rs:18424`: change `if to_state == LifecycleStage::Done && !repair_allowed` to gate on
`to_state == Done` alone, and fix the comment above it. `repair_closed_tx` is set only by the
CLI reconciler (`manager.rs:9676`); the atomic dispatch-close path is a different endpoint
(`ap971_dispatch_close_is_one_tx_append_plus_one_node_rewrite`, `api.rs:22429`) and is not
affected — verify that, do not assume it.

### 3. Tests must exercise the LIVE layout
- CLI `torn_close_candidates_yield_to_any_later_lifecycle_event` (`manager.rs:12307`) writes
  the later transition into a tx FILE (`:12318`). Add the journal case: close tx in
  `machines/<uuid>/tx/2026-08.org`, later `task.state_transitioned` ONLY in
  `tasks/<ID>/journal.org` → no candidate. Keep the old case if it still means something;
  the journal case is the one that matters.
- Daemon: a test for `recorded_close_allows_repair` where the only later transition is a
  journal entry → returns `false`; and one where the repair path targets `done` with an empty
  Evidence section → 400 (extend `task_close_requires_a_nonempty_evidence_section`,
  `api.rs:32892`, or add a sibling).
- The existing tests in both crates around these functions must still pass.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-cli --lib -- torn_close` (targeted; NEVER the whole crate)
- `cargo test -p orgasmic-daemon --lib -- repair evidence` (add your test names if they do
  not match these substrings)
- `cargo clippy -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; commit as `TASK-EPG6H.1: fix(...): <one line>`; one commit
  preferred.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command (this
  laptop reboots); NEVER set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live
  ledger at `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).

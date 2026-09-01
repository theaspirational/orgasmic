# Review: TASK-EPG6H.1 — torn-close repair guards fold the task journal (H3)

Fix round for chain-review finding H3 (whole-chain review tx-1c6d2115). Implementer: codex
gpt-5.6-sol, one commit `1139bc0f`, merged to main as `beee263b`.

## What to review

    git diff beee263b^1 beee263b

Three files, +199/-16: `crates/orgasmic-cli/src/manager.rs`,
`crates/orgasmic-daemon/src/api.rs`, `crates/orgasmic-daemon/src/index.rs` (one visibility
change).

## The finding this must close (H3)

EPG6H routes every `task.state_transitioned` to `.orgasmic/tasks/<ID>/journal.org`
(live ledger: 51 journals carry it, `machines/*/tx/` carry 0). Both torn-close guards
decided "was there a later transition?" by scanning only `tx/` + `machines/*/tx/`, so their
`task.state_transitioned` arms were dead: an operator move-back after a close was re-dragged
by the next manager command, and the daemon accepted the repair. Plus: `repair_allowed`
skipped the ART-04FYD Done evidence gate.

## What the fix claims

1. CLI `torn_close_candidates` (`manager.rs:9762`) keeps each pending close's `time`; after
   the tx pass it reads ONLY that task's `tasks/<ID>/journal.org`
   (`node_kernel::parse_journal`) and drops the candidate if any `task.state_transitioned`
   or `manager.dispatch_started` journal entry has `time >= close.time` (string compare of
   org timestamps; same-second journal wins — the direction that protects the operator).
2. Daemon `recorded_close_allows_repair` (`api.rs:18497`): same shape — resolves the
   matching close's time from the tx scan (a later non-matching close or tx-surface
   transition still clears it, as before), then refuses if the task journal has a
   `task.state_transitioned` at or after it. Reuses `crate::index::journal_tx_entry`, now
   `pub(crate)`.
3. Evidence gate (`api.rs:18420`): `to_state == Done` alone; the repair exception is gone.
   `repair_closed_tx` is set only by the CLI reconciler (`manager.rs:9676`); the atomic
   dispatch-close is a separate endpoint (`ap971_dispatch_close_is_one_tx_append_plus_one_node_rewrite`).
4. Tests: CLI `torn_close_candidates_yield_to_any_later_lifecycle_event` now seeds the tx
   in `machines/test-machine/tx/` and adds a task whose only later transition is a journal
   entry; daemon `recorded_close_repair_yields_to_later_journal_transition` (same-second
   journal → refused); `task_close_requires_a_nonempty_evidence_section` extended with a
   repair to `done` on an empty Evidence section → 400 naming "Evidence" (the close tx is
   dated year 9999 so no journal entry can outrank it — check that trick is sound).

## Attack these specifically

- **Timestamp ordering.** Both guards compare `time` strings. Are ALL producers of `TIME`
  in tx files and journals the same fixed-width `[YYYY-MM-DD Day HH:MM:SS]` shape (check
  `TxEntry::new` callers, `append_entry`, the migrator's output for pre-EPG6H journals, and
  the CLI-written test fixtures)? A timezone suffix, missing weekday, or a `<…>` active
  timestamp anywhere breaks lexicographic order silently. Grep the live ledger's journals
  (read-only) for a second shape.
- **Same-second semantics.** `>=` means a journal transition at the exact second of the
  close clears the candidate. Does the ATOMIC close path (AP971: one tx append + node
  rewrite) also write a `task.state_transitioned` journal entry at the same second? If yes,
  every new close is immediately "cleared" — harmless for new closes (they do not tear) but
  confirm the repair still fires for a genuinely torn LEGACY close whose journal has no
  such entry, and say whether a same-second legitimate close could ever be refused.
- **Asymmetry.** The CLI arm also honours `manager.dispatch_started` from the journal; the
  daemon arm honours only `task.state_transitioned`. Does `manager.dispatch_started` ever
  land in a journal (it is dispatch lifecycle → `machines/*/tx/` by the AP971.5 table)? If
  it cannot, the CLI arm is dead-but-harmless; if it can, the daemon arm is incomplete.
- **Error handling.** A malformed journal now makes `torn_close_candidates` return `Err`,
  which `reconcile_torn_closes` propagates up to `reconcile_torn_closes_best_effort` → the
  whole reconciliation is skipped with a warning. Before, a bad journal was irrelevant to
  this path. Is that the right failure mode, or should one unreadable journal only skip
  that task? Same question for the daemon: `ApiError::internal` on a bad journal now blocks
  a state update that carries `repair_closed_tx`.
- **Evidence gate blast radius.** With the exception removed, is there any remaining
  legitimate caller that reaches `update_task_state` with `to_state == Done` and
  `repair_closed_tx` set for a task whose Evidence is legitimately empty (e.g. the
  reconciler replaying an OLD pre-ART-04FYD torn close to `done`)? If so, that torn close is
  now permanently unrepairable by the CLI — state whether that is acceptable and what the
  operator sees.
- **Test honesty.** In the CLI test, verify the journal task's close time vs. the journal
  entry time actually exercises `>=` and not just "journal exists"; in the daemon test,
  verify the FIRST assertion (`true`) proves the tx-side match still works after the
  refactor from `allowed: bool` to `close_time: Option<String>`.

Already established — do not re-spend: on the merged tree the manager ran
`cargo test -p orgasmic-cli --bin orgasmic -- torn_close` (1 passed),
`cargo test -p orgasmic-daemon --lib -- repair` (5 passed), `-- evidence` (6 passed),
`cargo clippy -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings` clean,
`cargo fmt --all --check` clean (see `orgasmic task get --project orgasmic TASK-EPG6H.1`).

## Rules

- READ-ONLY. No edits, no git writes, no mutating `orgasmic` verbs, nothing against the live
  ledger at `~/.orgasmic/ledgers/orgasmic` (reading journals there is fine).
- File each finding as it appears:
  `orgasmic tx record --project orgasmic --type reviewer.finding --task TASK-EPG6H.1
  --reason "HIGH|MEDIUM|LOW <file:line> — <one sentence>"` (single line).
- Targeted tests only (`cargo test -p orgasmic-daemon --lib <name>`,
  `cargo test -p orgasmic-cli --bin orgasmic <name>`); NEVER the whole `orgasmic-cli` test
  suite unfiltered; never the workspace; never `ORGASMIC_HOME`; do not read
  `verify/*/injection.patch`.
- Say what you did not check. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only) and end with the explicit verdict sentence:
  APPROVE / APPROVE WITH FOLLOW-UPS / REJECT.

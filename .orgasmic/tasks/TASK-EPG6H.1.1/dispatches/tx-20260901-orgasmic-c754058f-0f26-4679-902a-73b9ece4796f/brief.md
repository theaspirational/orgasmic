# TASK-EPG6H.1.1 — residuals of the EPG6H.1 review (time compare, coverage, error scope)

Fix round 2 for TASK-EPG6H.1 (merged `beee263b`). The review (claude-opus-5 high,
tx-723be31b) approved with follow-ups. Read the task first:
`orgasmic task get --project orgasmic TASK-EPG6H.1.1` — it carries the exact `file:line`
and the acceptance list. Everything below is the minimum.

## 1. MEDIUM — parsed TIME compare, fail closed
`torn_close_candidates` (`crates/orgasmic-cli/src/manager.rs:~9814`) and
`recorded_close_allows_repair` (`crates/orgasmic-daemon/src/api.rs:~18541`) compare raw
org `TIME` strings. The ledger already holds date-only stamps (`[2026-06-05 Fri]`) on
close-type entries, and `' ' < ']'` makes such a close outrank every same-day full stamp →
the guard fails open. Fix: ONE small helper in `orgasmic-core` next to the existing
timestamp code (`rg -n "fn tx_time_string_utc|fn parse_org_timestamp|NaiveDateTime"
crates/orgasmic-core/src` — reuse a parser if one exists) that turns an org stamp into
`Option<chrono::NaiveDateTime>` (full `[YYYY-MM-DD Day HH:MM:SS]` only; date-only → `None`).
Both guards: if EITHER side fails to parse, treat it as "clears the candidate" / "refuse the
repair". Test: a date-only close stamp with a same-day full journal stamp → not a
candidate / not allowed.

## 2. MEDIUM — the CLI test must exercise the predicate
Add a task to the fixture in `torn_close_candidates_yield_to_any_later_lifecycle_event`
(`manager.rs:~12331`) whose journal `task.state_transitioned` is EARLIER than its close and
assert it IS still returned. Mirror the earlier-than case in the daemon test
`recorded_close_repair_yields_to_later_journal_transition`. A reviewer must be able to
delete the time predicate and watch a test go red.

## 3. LOW — error scope
`manager.rs:~9800`: an unreadable or unparseable journal must skip THAT candidate only (it is
"not repairable"), not abort the batch — `if let Ok(..)` per candidate, no `?`. Daemon
(`api.rs:~18523`): an unreadable/unparseable journal refuses THAT repair with a 400 that names
the journal path, not a 500. One test each.

## 4. LOW — say it in the comment
At the ART-04FYD gate (`api.rs:~18420`) add one sentence: a torn close whose recorded
`LIFECYCLE_TO` is `done` on a task with an empty Evidence section is not repairable until an
operator records evidence; the CLI prints the daemon's 400, which names the command.

## 5. Optional cleanup (do it if it stays under ~10 lines)
`recorded_close_allows_repair` round-trips through `crate::index::journal_tx_entry` only to
read `.ty`/`.time`, which `JournalEntry` already carries. Read them directly and revert the
`pub(crate)` widening in `index.rs`.

## Gates (each to a log file, never pipe cargo output)
- `cargo test -p orgasmic-cli --bin orgasmic -- torn_close` (targeted; NEVER unfiltered)
- `cargo test -p orgasmic-daemon --lib -- repair evidence`
- `cargo test -p orgasmic-core --lib <your helper's test name>` if you add the helper in core
- `cargo clippy -p orgasmic-core -p orgasmic-cli -p orgasmic-daemon --all-targets -- -D warnings`
- `cargo fmt --all --check`

## Rules
- Work only in your worktree; one commit `TASK-EPG6H.1.1: fix(dispatch): <one line>`.
- NEVER `cargo test --workspace`; NEVER the whole `orgasmic-cli` crate in one command; NEVER
  set `ORGASMIC_HOME`; NEVER run `daemon start`; never touch the live ledger at
  `~/.orgasmic/ledgers/orgasmic`; never run
  `legacy_drivers_and_explicit_pairs_emit_equivalent_start_events`.
- Report: what changed (`file:line`), each gate with its pass/fail line and log path, unmet
  criteria, residual risk. Finish with `orgasmic dispatch finalize --summary-file <path>`
  (report only, no `--commit`).

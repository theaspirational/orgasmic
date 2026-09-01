# Review: TASK-EPG6H.1 — torn-close repair guards fold the task journal (H3)

Reviewed `git diff beee263b^1 beee263b` (commit `1139bc0f`, merged as `beee263b`):
`crates/orgasmic-cli/src/manager.rs`, `crates/orgasmic-daemon/src/api.rs`,
`crates/orgasmic-daemon/src/index.rs`, +199/-16.

**H3 is closed.** Both acceptance criteria are met, and I have live-ledger evidence that the
journal fold does real work rather than passing vacuously. Four findings, none blocking.

## Findings

### MEDIUM (test) — `crates/orgasmic-cli/src/manager.rs:12331`: the CLI test does not exercise the time comparison

`torn_close_candidates_yield_to_any_later_lifecycle_event` proves "a journal entry exists",
not "the journal entry is at or after the close". Reading the fixture:

- `TASK-JOURNAL`'s close (`close("tx-5", …)`, manager.rs:12336) is hardcoded at
  `[2026-07-29 Wed 10:00:00]`; its journal entry (manager.rs:12374) is at
  `[2026-07-29 Wed 11:00:00]` — one hour **after**.
- `TASK-TORN`, the only surviving candidate, has no journal file at all.
- `TASK-LANDED` / `TASK-REDISPATCHED` / `TASK-LEGACY` have no journals either.

So no fixture has a journal `task.state_transitioned` that *predates* its close. Replace
`&& entry.time >= close_time` (manager.rs:9814) with nothing at all and the `assert_eq!` is
unchanged. The half of the guard that protects a genuine legacy repair — "an older journal
entry must NOT suppress the repair" — has zero CLI coverage.

The daemon twin is better: `recorded_close_repair_yields_to_later_journal_transition`
(api.rs:22457) puts the journal entry at exactly the close's second, so `>` instead of `>=`
would fail it. It likewise has no earlier-than case.

Fix direction: add a fourth task to the CLI fixture whose journal `task.state_transitioned`
sits *before* its close, and assert it is still returned as a candidate.

### MEDIUM (correctness, latent) — `manager.rs:9814` / `api.rs:18541`: lexicographic TIME compare rests on an unenforced format invariant

Both guards decide by `String` comparison of raw org `TIME` property values. Nothing
validates the shape of `TIME`: `TxEntry::validate` (crates/orgasmic-core/src/tx.rs:338) only
checks key syntax and value printability, and `parse_tx_file` / `parse_journal` copy the
property through verbatim (tx.rs:695, node_kernel.rs:151).

The counterexample shape is already on the ledger. Surveying `~/.orgasmic/ledgers/orgasmic`
read-only:

```
$ grep -rh "^:TIME:" .orgasmic/machines/*/tx/*.org .orgasmic/tx/*.org | sed 's/[0-9]/N/g' | sort | uniq -c
   ... 6767 :TIME:  [NNNN-NN-NN Xxx NN:NN:NN]
     12 :TIME:         [NNNN-NN-NN Fri]
      8 :TIME:         [NNNN-NN-NN Wed]
```

Those 20 date-only stamps sit on real `implementer.done` / `reviewer.done` /
`manager.action` entries in `.orgasmic/tx/2026-06.org` — i.e. on exactly the entry types
whose `entry.time` becomes `close_time`. Because `' '` (0x20) sorts below `']'` (0x5D),
`"[2026-06-05 Fri 23:59:59]" >= "[2026-06-05 Fri]"` is **false**: a date-only close time
outranks every full-width journal timestamp from the same day, and the guard silently fails
open (repair allowed) rather than closed.

Not exploitable today, and I verified that:
- none of the 20 date-only entries carries `LIFECYCLE_FROM` (awk over all tx files: 0 hits),
  so none can become a candidate;
- every current producer of a tx/journal `TIME` is `tx_time_string_utc` (api.rs:2900),
  fixed-width and UTC — `grep -rn "Local::now\|chrono::Local" crates/` returns nothing, so
  there is no timezone skew;
- the writer copies the tx `TIME` into the journal byte-for-byte
  (`journal_entry`, writer.rs:2737), so both sides of the compare share one producer.

The defect is that the invariant is undocumented and unchecked on a surface that demonstrably
already violated it once. Fix direction: parse both sides into `NaiveDateTime` and treat an
unparseable stamp as "clears the candidate" (fail closed); or, at minimum, require equal byte
length before trusting the string compare.

### LOW (error handling) — `manager.rs:9800`: one bad journal disables reconciliation for every task

The two `?` on `read_to_string` / `parse_journal` propagate to `reconcile_torn_closes` →
`reconcile_torn_closes_best_effort` (manager.rs:9754), which prints
`warning: torn-close reconciliation skipped: …` and abandons **all** remaining candidates —
including ones whose journals are fine. `parse_journal` is also stricter than this guard
needs: it runs `JournalEntry::validate` on every entry (node_kernel.rs:169), so one
malformed drawer anywhere in one task's journal takes out the batch.

The daemon already has a house policy for this: `collect_journal_file` (index.rs:3711)
records a `ParseError` and carries on. The new daemon arm does the opposite —
`ApiError::internal` (api.rs:18523/18531) turns a bad journal into a 500 on a state update
that would otherwise be decidable.

Blast radius is bounded (all three CLI call sites are the best-effort wrapper: manager.rs:955,
1120, 2975 — the command itself still succeeds), which is why this is LOW. Fix direction:
per-candidate `if let Ok(...)`, treating an unreadable journal as "not repairable".

### LOW (usability) — `api.rs:18420`: a `to_state == done` torn close on an evidence-free task is now permanently unrepairable

Acceptance criterion 2 is met and the blast radius is confined: `repair_closed_tx` has exactly
one producer in the tree (`post_task_state`, manager.rs:9676 — grep confirms no UI or other
caller), and the atomic dispatch close is a separate endpoint
(`post_task_dispatch_close_commit`, api.rs:18153) that never touches this gate.

The consequence worth stating: the reconciler replays the close's recorded `LIFECYCLE_TO`, and
40 close txs on the live ledger carry `LIFECYCLE_TO: done` while most of their tasks have an
empty `** Evidence` section. If such a close ever tears while its task still sits at
`LIFECYCLE_FROM`, the reconciler will now fail it on every manager command, forever, until an
operator writes evidence by hand. I consider this acceptable — the operator does see the
daemon's 400 verbatim through `eprintln!("warning: could not finish torn close … {err}")`
(manager.rs:9733), and that message names the section and the exact
`orgasmic node body set … --section Evidence` command. Today zero tasks are in that state
(see Verification Notes), so nothing is stuck.

## Open Questions

1. Should the CLI arm keep `manager.dispatch_started` in its journal predicate? I confirmed it
   is dead code: `event_routes_to_journal` (api.rs:8747-8768) does not list it, so it always
   lands in `machines/<id>/tx/`, and the only migrator that writes journals
   (project_migrate.rs:321) migrates artifact `reviews.org` comments only — never lifecycle
   events. Dead-but-harmless, and defensible as future-proofing if EPG6H's routing table
   grows. The corresponding brief worry is resolved in the other direction: the daemon arm is
   **complete**, not incomplete.
2. `journal_tx_entry` was widened to `pub(crate)` (index.rs:3731) purely to re-derive `.ty`
   and `.time`, which `JournalEntry` already carries verbatim (index.rs:3744-3746 copies them
   unchanged). The `.map()` also clones and escapes every entry body for nothing
   (index.rs:3763). Reading `entry.ty` / `entry.time` directly would drop the conversion and
   the visibility change. No behavioural difference; noted only because the widening buys
   nothing.

## Verification Notes

**Targeted tests, run in this worktree on the merged tree:**

- `cargo test -p orgasmic-daemon --lib -- recorded_close_repair_yields_to_later_journal_transition`
  → `1 passed` (log `/tmp/epg6h-review-daemon-test.log`, pid 33986).
- `cargo test -p orgasmic-cli --bin orgasmic -- torn_close_candidates_yield_to_any_later_lifecycle_event`
  → `1 passed` (log `/tmp/epg6h-review-cli-test.log`).

**Production-path probe (the important one).** A unit test cannot show whether the journal fold
actually changes anything on real data, so I re-implemented `torn_close_candidates`' exact
algorithm in Python (`/tmp/torn_sim.py`, read-only) and ran it against the live ledger at
`~/.orgasmic/ledgers/orgasmic`:

```
after tx pass:        68 pending candidates
after journal fold:   20 pending candidates
-- would-repair (tx-only, i.e. OLD behaviour) --   (none)
-- would-repair (NEW behaviour) --                 (none)
```

The fold drops **48 of 68** candidates that the tx-only scan kept — H3's premise (the
`task.state_transitioned` arm was dead against `tx/`) is real, and the fix is load-bearing on
this ledger, not vacuous. Caveat: that is my re-implementation, not the shipped binary; it
replicates the file-ordering (`sort by file_name, then path`), the close/transition state
machine, and the `>=` journal predicate, but any divergence is mine.

The same probe is my evidence for the LOW evidence-gate finding: no task on the ledger
currently sits at its close's `LIFECYCLE_FROM`, so nothing would be posted for repair by either
the old or the new code, and the Evidence-gate change strands nothing today.

**Static checks I ran:**

- Acceptance criterion 1, repo-wide: I checked every caller of the two tx-only scans
  (`read_tx_entries` at manager.rs:8818/9764/9846/10099/10684/10903; `project_tx_entries` at
  api.rs:18506). All of them match only on `implementer.done` / `reviewer.done` /
  `architector.done` / `manager.dispatch_aborted` / `manager.dispatch_started` / `run.created`
  — plus `fold_dispatches` (tx.rs:100-166), same set. None of those is in
  `event_routes_to_journal`, so no other guard arm reads a surface its event no longer lands
  on. `torn_close_candidates` was the only one.
- Same-second self-clearing (brief's concern): does **not** occur. The atomic close
  (`post_task_dispatch_close_commit`) commits one `transaction_multi(rewrites, vec![close_tx])`
  (api.rs:18272) and returns `transition_tx_id: String::new()` — it writes no
  `task.state_transitioned` at all, so a new close can never clear its own candidate. A
  same-second legitimate close could only be refused if an operator's move landed in the same
  UTC second; that is the safe direction (refuse rather than drag).
- The year-9999 trick in `task_close_requires_a_nonempty_evidence_section` is sound in both
  directions. `in_progress > in_review,todo,cancelled` in `shipped/schema/node-types/task.org:12`
  — `done` is **not** reachable from `in_progress` — so the request only gets past
  `allows_transition` if `repair_allowed` was genuinely true, and the fixture's real-time
  journal entries can never outrank `[9999-12-31 Fri 23:59:59]`. The test therefore proves both
  that the repair exception still works and that the Evidence gate now bites.
- Journal path resolution matches on both sides:
  `task_node_file_path(...).with_file_name(JOURNAL_FILE)` yields
  `.orgasmic/tasks/<ID>/journal.org` (paths.rs:86), which is exactly where `node_journal_path`
  (api.rs:8803/8811) routes a `task.state_transitioned` for a task with a node dir. For a task
  with no node dir, `node_journal_path` returns `None` and the transition goes to
  `machines/*/tx/` — where the tx pass still catches it. The two surfaces stay consistent.
- The daemon's `close_time = (...).then_some(entry.time)` refactor preserves the old clearing
  semantics: a later non-matching close still sets `None`, as does a tx-surface
  `task.state_transitioned`.

**What I did not check:**

- I did not re-run `cargo clippy` or `cargo fmt --check`; the brief records them clean on the
  merged tree and I had no reason to doubt it.
- I did not run the full `orgasmic-cli` or `orgasmic-daemon` suites, or any integration test —
  only the two named unit tests.
- I did not exercise a real torn close end-to-end against a live daemon (no isolated daemon was
  stood up). The claim that an operator move-back is no longer re-dragged rests on the unit
  tests plus the ledger simulation above, not on an observed repair.
- I did not read `verify/*/injection.patch`.
- I did not audit the pre-existing tx iteration order in `read_tx_entries` (sorted by month
  file name, then path, so two machines' entries within one month interleave by machine dir
  rather than by time). That is untouched by this diff and would need both a close and a
  transition on different machines in the same month to matter — out of scope here, but it is
  the same class of assumption as the MEDIUM timestamp finding.

## Fix Directions

1. Add the missing CLI case: a task whose journal `task.state_transitioned` is *earlier* than
   its close, asserted to still be a candidate. This is the one change I would ask for before
   calling H3's regression coverage complete.
2. Compare parsed timestamps, not strings, and fail closed on an unparseable stamp
   (manager.rs:9814, api.rs:18541).
3. Make an unreadable journal skip one candidate instead of the batch (manager.rs:9800), and
   refuse rather than 500 on the daemon side (api.rs:18523).
4. Optional: drop the `journal_tx_entry` round-trip in `recorded_close_allows_repair` and read
   `entry.ty` / `entry.time` off the `JournalEntry`, reverting the `pub(crate)` widening in
   index.rs.

Verdict: APPROVE WITH FOLLOW-UPS.

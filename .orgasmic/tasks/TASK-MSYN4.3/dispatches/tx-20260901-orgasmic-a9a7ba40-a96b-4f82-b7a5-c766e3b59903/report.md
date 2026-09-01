# Review: TASK-MSYN4.3 — UUID tx ids on every project path

Reviewed `git diff 568cb5be^1 568cb5be` (3 files, +130/-437) plus every consumer of the tx-id
shape across `crates/`, `ui/src` and `shipped/`. The finding is closed. Three LOWs, no HIGH,
no MEDIUM.

## Verdict

**APPROVE WITH FOLLOW-UPS.**

The acceptance criteria are met:
1. *Machine component / fold keys.* Both `ProjectSequence` mint paths now emit
   `tx-{date}-{slug}-{uuid_v4}` (`writer.rs:2819-2827`). The old `is_machine_tx_path` split is
   gone, so node journals, the legacy `.orgasmic/tx/` dir and `machines/*/tx` mint identically.
   Existing numeric ids stay valid as references — nothing in the tree parses, validates or
   range-checks a numeric tail (see Verification).
2. *`project_max` after a pull.* Satisfied by deletion: `ProjectTxSeqCache`,
   `next_project_tx_id` and `scan_project_tx_max_seq` are gone, so there is no stale in-memory
   max to invalidate. `rg 'scan_count|max_seq|ProjectTxSeq|next_project_tx_id|project_tx_sequence'
   crates/ ui/src shipped/` returns zero hits — no dead references, no orphaned metric.
3. *Tests + gates.* Re-ran the three new tests myself; all pass (below).

## Findings

### LOW — `crates/orgasmic-daemon/src/index.rs:4315` — same-second activity order became random
`entries.sort_by(|a,b| a.time.cmp(&b.time).then_with(|| a.tx_id.cmp(&b.tx_id)))`. Every
non-claim tx is stamped at **second** precision (`api.rs:2924, 7368, 18989, 19117, 19377, 19514,
19981, 20137, 20232, 20414, 20651`; `ledger_sync.rs:389`), so ties are ordinary. With the numeric
tail the tie-break was a project-monotone counter and reproduced insertion order; with a uuid it
is deterministic-but-arbitrary.

*Failure scenario:* two comments posted on one task inside the same second render in the wrong
order in `GET /tasks/:id/activity` (and therefore in `TaskDialog.tsx` / `ActivityView.tsx`).

*Impact: cosmetic, not correctness.* `activity_index` is built only at `index.rs:1164, 2499,
3002, 4105` and read only by the activity endpoint (`api.rs:2478`). No fold consumes it. The
lifecycle folds are unaffected — `torn_close_candidates` (`manager.rs:9762`) and
`recorded_close_allows_repair` (`api.rs:18531`) both iterate entries in **file order** and
last-write-wins per task; neither sorts by `tx_id`, and neither compares equal TIMEs to break a
tie (they use TIME only afterwards, against the journal, at `manager.rs:9805` /
`api.rs:18568`).

*Fix direction:* if same-second ordering matters, stamp `%.6f` like the claim path already does
(`api.rs:7477`), or carry an append ordinal. Otherwise close as accepted cosmetic drift.

### LOW — `crates/orgasmic-core/src/tx.rs:1170` — the fold regression test pins nothing new
`dispatch_fold_keeps_two_machine_generations_distinct_by_uuid_tx_id` builds two starts with two
**already distinct** uuids and asserts each `CLOSED_TX` closes its own generation. The diff
touches nothing but tests in `tx.rs`, so this test passes verbatim on `568cb5be^1` — it is not a
regression test. It never constructs the finding's actual shape (one id shared by two
generations, which is what mis-attributed the close).

Acceptance is still satisfied, by the *writer* tests
(`project_sequence_policy_mints_uuid_for_node_journal`,
`two_writers_cannot_mint_the_same_node_journal_tx_id`) — those do fail pre-fix. But "two-writer
collision test in the fold" is only nominally covered.

*Fix direction:* add the negative case — two `manager.dispatch_started` sharing one `TX_ID` on two
machines, then one `CLOSED_TX`. Assert the documented behaviour (both close / ambiguity is
detected). That test would fail on the old runtime's output and pins the fold's contract.

### LOW — `crates/orgasmic-daemon/src/writer.rs:2819` — a path guard was dropped with the scan
Deleting `project_tx_dir` also deleted its two error arms ("machine tx path is not under
.orgasmic", "project journal is not under .orgasmic"). Pre-fix, a `ProjectSequence` append whose
`tx_path` did not sit under a `.orgasmic` ancestor was **rejected**; post-fix `prepare_tx_entry`
mints an id unconditionally and the write lands wherever it was told.

*Failure scenario:* a future caller passes a mis-rooted `tx_path` (a bare temp dir, a worktree
path resolved wrong). It used to surface as an append error; now it silently writes a stray
ledger file. Note the new `two_writers_cannot_mint_the_same_node_journal_tx_id` test itself
relies on this: its paths are `<tmp>/machine-a/.orgasmic/tasks/...`, which the old guard would
have accepted, but the loosened contract is what makes such shapes free.

*Fix direction:* either accept (the guard was incidental to sequence computation, not a stated
invariant) or re-assert it in `prepare_tx_entry` as an explicit `ProjectSequence` precondition.

## Open Questions

1. **`ProjectSequence` is now a misnomer.** `TxIdPolicy::ProjectSequence` (`writer.rs:275`) mints
   no sequence, and `api.rs:3007, 8554` still render the placeholder string
   `"pending-project-sequence"`. 13 live call sites. Purely naming — no bug, not filed as a
   finding. Rename to `ProjectMinted` in a follow-up if the manager wants it.
2. **Out of scope, spotted in passing:** `ledger_sync.rs:406` writes `ledger.sync_conflict` to
   `.orgasmic/machines/<id>/{YYYY-MM}.org` — *not* under `machines/<id>/tx/`. Both
   `read_tx_entries` (`manager.rs:10787`) and `project_tx_entries` (`api.rs:3801`) only scan
   `machines/*/tx`, so those entries look invisible to the folds. Pre-existing (TASK-8DWJP),
   untouched by this diff, not verified further.

## Verification Notes

**Tests I ran** (targeted only, per the brief):
```
cargo test -p orgasmic-core --lib tx::tests::dispatch_fold_keeps_two_machine_generations_distinct_by_uuid_tx_id   → 1 passed
cargo test -p orgasmic-daemon --lib -- writer::tests::project_sequence_policy_mints_uuid_for_node_journal \
                                       writer::tests::two_writers_cannot_mint_the_same_node_journal_tx_id        → 2 passed
```
I did not re-run the manager's five gates; they are recorded in the task Evidence.

**Consumers of the id shape — all clear.**
- No numeric-tail parser or validator survives anywhere:
  `rg 'tx-\d|split\(.-.\)|rsplit|parse::<u|numeric|seq' crates/ ui/src shipped/` — the only
  structural parse left is `integration.rs:1382`
  (`Uuid::parse_str(tx_id.splitn(4,'-').nth(3))`), which already **required** a uuid pre-fix
  (it targets a `machines/*/tx` path) and still passes.
- No tx-id cursor API. `rg 'after|since|cursor' api.rs | grep tx` — every hit is
  `refresh_after_tx`/`release_claim_after_terminal_tx`, none is a pagination cursor. Nothing
  slices or index-reads a tx id (`rg 'tx_id\[|tx_id.get\(|tx_id.chars'` → zero hits).
- **Sorting by `tx_id`:** exactly three sites. `index.rs:4315` (Finding 1). `claims.rs:28` and
  `claims.rs:53` are **safe** — the sort is `time → machine → tx_id`, and (a) claim events are the
  one path stamped at microsecond precision (`api.rs:7477`), so time ties are effectively
  impossible, and (b) `active` is keyed `(task, machine)`, so the second sort has one entry per
  machine and its `tx_id` arm is unreachable.
- **Docs:** `shipped/schema/tx.org` and `shipped/schema/journal.org` document the id only as
  "globally unique within the file" / "globally unique daemon-minted tx-style entry id" — no shape
  is specified. No docs drift, nothing to update.
- **Paths:** `dispatch_record_dir` / `dispatch_record_report_rel` (`paths.rs:115-135`) use the id
  as a directory name via `sanitize_started_tx` (`paths.rs:137`), which rejects only `/`, `\`,
  `..`, NUL and empty. A uuid passes; the name grows 25→57 chars, far under any limit. Machine-tx
  dispatch dirs already carried uuids.
- **UI:** `tx_id` is used as a React key, a thread identity and display text only
  (`TaskDialog.tsx:773, 841-863`; `ActivityView.tsx:219, 591, 612-623`). No truncation or padding;
  `ActivityView.tsx:551` sanitizes to a DOM id with `replace(/[^a-zA-Z0-9_-]/g,'-')`, which uuids
  survive unchanged. `ui/src/lib/types.ts` types it `string`.
- **Hardcoded numeric ids** (`rg 'tx-\d{8}-[a-z0-9]+-\d{4}'`) are all *literals inside tests and
  fixtures* — `paths.rs:1188-1214`, `tx.rs:757-1057`, `writer_durability.rs:484-515`,
  `integration.rs:1350`, `task_title_edit_cli.rs:46`. None asserts on a **minted** id. This is the
  direct evidence for "existing ids stay valid as references".

**Mixed-version fleet — safe, no release note needed.** I read the deleted
`scan_project_tx_max_seq` / `project_tx_sequence` in `568cb5be^1`. `project_tx_sequence` splits on
`-` and requires exactly four segments; a uuid tail adds four more, so `parts.next().is_some()`
short-circuits to `None` (the digit check would also fail). An old machine A therefore **ignores**
new machine B's uuids, and its scan still covers `.orgasmic/tx`, every `machines/*/tx` and every
node `journal.org` — so it still sees B's *old* numeric ids and takes the max across them. A
cannot collide with itself or with any pre-upgrade B id. The residual risk is the original
old↔old collision, which persists only until both machines upgrade. The live ledger has one
machine dir (`.orgasmic/machines/08c4c046-…`), so there is no fleet exposure today. (Confirmed
the live daemon still runs the pre-fix runtime: my own finding tx minted
`tx-20260901-orgasmic-6908` — expected per the brief, not a defect.)

Edge case worth naming: on a project whose ids are *all* uuids (created entirely post-fix), an old
machine's scan finds `max_seen == 0`, logs the "sequence restarts at 1" warning and mints `0001`.
Harmless — there are no numeric ids for it to collide with.

**Deleted tests — no durability property lost.** Five removals, audited one by one:
| Test | Covered | Verdict |
|---|---|---|
| `writer.rs project_tx_sequence_survives_the_five_digit_rollover` | the deleted parser only | fine |
| `writer.rs ap971_project_sequence_cold_start_scans_tx_and_node_journals` | cold-start scan (gone) **and** that a node-journal `ProjectSequence` append round-trips through `parse_journal` and leaves no `placeholder` | **replaced** by `project_sequence_policy_mints_uuid_for_node_journal`, which keeps both surviving assertions |
| `writer_durability.rs project_tx_sequence_cache_avoids_rescan_on_hot_path` | cache hit-rate only (`scan_count`) | fine |
| `writer_durability.rs corrupt_sibling_tx_file_does_not_block_appends` | that a corrupt *sibling* file in the tx dir does not break an append | now trivially true — I checked the append path reads only its own target (`read_event_ids`, `writer.rs:2665/2723`) and no sibling. Fine |
| `writer_durability.rs tx_append_reopens_after_path_inode_swap_and_rescans_sequence` | inode swap **and** rescan | **kept** as `tx_append_reopens_after_path_inode_swap`; the durability assertion (post-swap append lands in the replacement file at the path, not the orphaned inode, `writer_durability.rs:515-519`) is intact, only the id assertions were relaxed |

No test removed a property that no surviving test pins. Reopen-after-rename and group-commit/fsync
coverage in `writer_durability.rs` is untouched.

**Deleted scan had no side effect.** The old `scan_project_tx_max_seq` was read-only —
`read_dir` + `read_to_string` + `parse_tx_file`/`parse_journal`, no `create_dir_all`, no header
write, no `File::create`. Directory and file creation happen in `append_txs_inner`
(`writer.rs:2896`) and `write_tx_append`, both untouched. Nothing depended on the read.

**Dedupe is unaffected and slightly stronger.** `event_id()` (`writer.rs:2714`) falls back to
`entry.tx_id` when no `EVENT_ID` is present, and that key backs the persistent no-op at
`writer.rs:2674-2688`. Previously two machines could mint the same fallback key; with uuids they
cannot.

## What I did not check

- The full workspace, the `orgasmic-cli` suite and the daemon integration suite — per the brief.
  Mitigation: `rg` over the whole tree found no reference to any deleted symbol, and the manager's
  `clippy --all-targets` on core+daemon would have caught in-crate breakage. `orgasmic-cli` was
  **not** clippy'd by anyone, but it references none of the deleted items.
- Live multi-machine behaviour. Only one machine dir exists in the ledger, so the cross-machine
  path is argued from source, not observed.
- The `ledger_sync` conflict-path visibility question in Open Questions #2.
- `verify/*/injection.patch` (forbidden by the brief).

## Fix Directions

1. *(Finding 2, worth doing)* Add the collision-shaped fold test: two starts sharing one `TX_ID`
   across two machines plus one `CLOSED_TX`; assert the intended behaviour. That is the test the
   acceptance line asked for and the only one that would fail on old-runtime data.
2. *(Finding 1, decide)* Either stamp comment/state tx at `%.6f` like `api.rs:7477`, or record
   same-second activity order as accepted cosmetic drift.
3. *(Finding 3, optional)* Re-assert the `.orgasmic`-ancestor precondition in `prepare_tx_entry`,
   or accept it as deliberately dropped.
4. *(Open Question 1, cosmetic)* Rename `TxIdPolicy::ProjectSequence` and the
   `"pending-project-sequence"` placeholder.

**APPROVE WITH FOLLOW-UPS.**
